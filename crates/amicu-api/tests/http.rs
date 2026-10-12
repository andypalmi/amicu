//! End-to-end HTTP tests over the auth router (in-process, via `oneshot`).
#![allow(clippy::duration_suboptimal_units)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use amicu_api::{AuthApiState, InMemoryRateLimiter, TrustedProxies, auth_router};
use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::jwks::Jwks;
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::notify::{LoggingNotifier, Notification, NotificationKind, Notifier};
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::service::AuthService;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verifier::{StaticJwksProvider, Verifier, VerifierConfig};
use amicu_auth::verify::InMemoryVerifyStore;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://auth.amicu.dev";
const AUD: &str = "amicu-api";

fn app(rate_max: u32) -> Router {
    app_with_proxies(rate_max, TrustedProxies::default())
}

fn app_with_proxies(rate_max: u32, trusted: TrustedProxies) -> Router {
    build_app(rate_max, trusted, Arc::new(LoggingNotifier))
}

/// Build an app whose notifier captures the out-of-band reset token, so an HTTP
/// test can drive the full `/recover` → `/reset` flow end to end.
fn app_capturing(rate_max: u32) -> (Router, Arc<CapturingNotifier>) {
    let notifier = Arc::new(CapturingNotifier::default());
    let app = build_app(rate_max, TrustedProxies::default(), notifier.clone());
    (app, notifier)
}

fn build_app(rate_max: u32, trusted: TrustedProxies, notifier: Arc<dyn Notifier>) -> Router {
    build_app_with_limiter(rate_max, trusted, notifier).0
}

/// Like [`build_app`], but also hands back the concrete [`InMemoryRateLimiter`]
/// so a test can inspect its bucket count directly (the router only holds it
/// behind `Arc<dyn RateLimiter>`).
fn build_app_with_limiter(
    rate_max: u32,
    trusted: TrustedProxies,
    notifier: Arc<dyn Notifier>,
) -> (Router, Arc<InMemoryRateLimiter>) {
    build_app_full(rate_max, trusted, notifier, true)
}

fn build_app_full(
    rate_max: u32,
    trusted: TrustedProxies,
    notifier: Arc<dyn Notifier>,
    signups_enabled: bool,
) -> (Router, Arc<InMemoryRateLimiter>) {
    let keyring = Keyring::new(SigningKey::generate().unwrap());
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));
    let service = Arc::new(AuthService::new(
        keyring,
        issuer,
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        notifier,
        "https://auth.amicu.dev",
    ));
    let limiter = Arc::new(InMemoryRateLimiter::new(rate_max, Duration::from_secs(60)));
    let router = auth_router(
        AuthApiState::new(service, limiter.clone())
            .with_trusted_proxies(trusted)
            .with_signups_enabled(signups_enabled),
    );
    (router, limiter)
}

/// A [`Notifier`] that captures the reset token a real mailer would embed in a
/// link, so an HTTP test can present it back to `/reset`.
#[derive(Default)]
struct CapturingNotifier {
    reset_tokens: std::sync::Mutex<Vec<String>>,
    verify_tokens: std::sync::Mutex<Vec<String>>,
}

impl CapturingNotifier {
    /// The most recently captured reset token.
    ///
    /// Read SYNCHRONOUSLY, with no yielding: since
    /// andypalmi/amicu-planning#141 the delivery hand-off happens ON the
    /// request path (it is a durable enqueue, not an SMTP send), so by the time
    /// `/recover` has responded the notifier has already been called. This used
    /// to poll `yield_now` in a loop because the work was detached onto
    /// `tokio::spawn` - which Cloud Run's post-response CPU throttling made
    /// unreliable. That this is now a plain read is itself the regression test.
    fn last_reset_token(&self) -> Option<String> {
        self.reset_tokens.lock().unwrap().last().cloned()
    }

    /// [`Self::last_reset_token`], panicking if nothing was captured.
    fn reset_token(&self) -> String {
        self.last_reset_token()
            .expect("a reset token must be captured before /recover responds")
    }

    /// The most recently captured email-verification token. Synchronous for the
    /// same reason as [`Self::last_reset_token`].
    fn last_verify_token(&self) -> Option<String> {
        self.verify_tokens.lock().unwrap().last().cloned()
    }

    /// [`Self::last_verify_token`], panicking if nothing was captured.
    fn verify_token(&self) -> String {
        self.last_verify_token()
            .expect("a verify token must be captured before the response returns")
    }
}

#[async_trait::async_trait]
impl Notifier for CapturingNotifier {
    async fn notify(&self, message: Notification) -> Result<(), amicu_auth::error::AuthError> {
        match message.kind {
            NotificationKind::PasswordReset { reset_token } => {
                self.reset_tokens
                    .lock()
                    .unwrap()
                    .push(reset_token.expose_secret_str().unwrap().to_string());
            }
            NotificationKind::VerifyEmail { verify_token } => {
                self.verify_tokens
                    .lock()
                    .unwrap()
                    .push(verify_token.expose_secret_str().unwrap().to_string());
            }
            NotificationKind::AccountExists => {}
        }
        Ok(())
    }
}

/// Call the router with an explicit peer socket address (as the connect-info
/// make service would inject) and an optional `X-Forwarded-For` header.
async fn call_peer(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    peer: SocketAddr,
    xff: Option<&str>,
) -> StatusCode {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(peer));
    if let Some(x) = xff {
        builder = builder.header("x-forwarded-for", x);
    }
    let req = if let Some(b) = body {
        builder = builder.header("content-type", "application/json");
        builder.body(Body::from(b.to_string())).unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    };
    app.clone().oneshot(req).await.unwrap().status()
}

fn peer(ip: &str) -> SocketAddr {
    SocketAddr::new(ip.parse().unwrap(), 40000)
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    xff: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-forwarded-for", xff);
    let req = if let Some(b) = body {
        builder = builder.header("content-type", "application/json");
        builder.body(Body::from(b.to_string())).unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn well_known_endpoints_serve_public_jwks_and_discovery() {
    let app = app(100);

    let (status, jwks) = call(&app, "GET", "/.well-known/jwks.json", None, "1.1.1.1").await;
    assert_eq!(status, StatusCode::OK);
    let keys = jwks["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["kty"], "OKP");
    assert_eq!(keys[0]["alg"], "EdDSA");
    assert!(keys[0]["kid"].as_str().is_some());
    // No private scalar is ever published.
    assert!(keys[0].get("d").is_none());

    let (status, disc) = call(
        &app,
        "GET",
        "/.well-known/openid-configuration",
        None,
        "1.1.1.1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disc["issuer"], ISS);
    assert_eq!(
        disc["jwks_uri"],
        "https://auth.amicu.dev/.well-known/jwks.json"
    );
    assert_eq!(disc["id_token_signing_alg_values_supported"][0], "EdDSA");
}

#[tokio::test]
async fn full_lifecycle_signup_token_verify_refresh_logout() {
    let app = app(100);
    let creds = json!({"email": "user@example.com", "password": "hunter2-hunter2"});

    // Sign up: generic accepted response, no user id echoed (anti-enumeration).
    let (status, body) = call(&app, "POST", "/signup", Some(creds.clone()), "10.0.0.1").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body["user_id"].is_null());
    assert!(body["message"].as_str().is_some());

    // Get tokens.
    let (status, tokens) = call(&app, "POST", "/token", Some(creds.clone()), "10.0.0.1").await;
    assert_eq!(status, StatusCode::OK);
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(tokens["token_type"], "Bearer");

    // Verify the access token offline against the published JWKS.
    let (_, jwks_json) = call(&app, "GET", "/.well-known/jwks.json", None, "10.0.0.1").await;
    let jwks: Jwks = serde_json::from_value(jwks_json).unwrap();
    let verifier = Verifier::new(StaticJwksProvider::new(jwks), VerifierConfig::new(ISS, AUD));
    let claims = verifier.verify(&access).await.unwrap();
    assert_eq!(claims.aud, AUD);

    // Refresh rotates.
    let (status, rotated) = call(
        &app,
        "POST",
        "/token/refresh",
        Some(json!({ "refresh_token": refresh })),
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let new_refresh = rotated["refresh_token"].as_str().unwrap().to_string();

    // Old refresh token is now revoked.
    let (status, _) = call(
        &app,
        "POST",
        "/token/refresh",
        Some(json!({ "refresh_token": refresh })),
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Logout revokes the current refresh token.
    let (status, _) = call(
        &app,
        "POST",
        "/logout",
        Some(json!({ "refresh_token": new_refresh })),
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        "POST",
        "/token/refresh",
        Some(json!({ "refresh_token": new_refresh })),
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_password_is_unauthorized() {
    let app = app(100);
    let creds = json!({"email": "a@example.com", "password": "right-password"});
    call(&app, "POST", "/signup", Some(creds), "2.2.2.2").await;

    let (status, _) = call(
        &app,
        "POST",
        "/token",
        Some(json!({"email": "a@example.com", "password": "wrong"})),
        "2.2.2.2",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn signup_disabled_returns_403_and_creates_no_user() {
    let (app, _) = build_app_full(
        100,
        TrustedProxies::default(),
        Arc::new(LoggingNotifier),
        false,
    );
    let creds = json!({"email": "user@example.com", "password": "hunter2-hunter2"});

    let (status, body) = call(&app, "POST", "/signup", Some(creds.clone()), "10.0.0.1").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "signups_disabled");
    assert!(body["error"].as_str().is_some());

    // No user row was created: the credentials cannot sign in.
    let (status, _) = call(&app, "POST", "/token", Some(creds), "10.0.0.1").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn duplicate_signup_is_indistinguishable() {
    let app = app(100);
    let creds = json!({"email": "dup@example.com", "password": "some-password"});
    // Both the first (new account) and second (collision) sign-up return the
    // exact same status and body - an attacker cannot enumerate registered
    // emails through /signup.
    let (s1, b1) = call(&app, "POST", "/signup", Some(creds.clone()), "3.3.3.3").await;
    let (s2, b2) = call(&app, "POST", "/signup", Some(creds), "3.3.3.3").await;
    assert_eq!(s1, StatusCode::ACCEPTED);
    assert_eq!(s2, StatusCode::ACCEPTED);
    assert_eq!(b1, b2);
    assert!(b1["user_id"].is_null());
}

#[tokio::test]
async fn signup_weak_password_is_rejected() {
    let app = app(100);
    let (status, _) = call(
        &app,
        "POST",
        "/signup",
        Some(json!({"email": "weak@example.com", "password": "short"})),
        "4.4.4.4",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rate_limit_trips_after_max() {
    let app = app(3);
    let body = json!({"email": "rl@example.com", "password": "bad"});
    let client = peer("9.9.9.9");
    // The 4th credential request from the same client is throttled.
    for _ in 0..3 {
        let status = call_peer(&app, "POST", "/token", Some(body.clone()), client, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED); // unknown user, but not throttled
    }
    let status = call_peer(&app, "POST", "/token", Some(body), client, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // The unthrottled well-known endpoint still works from the same client.
    let status = call_peer(&app, "GET", "/.well-known/jwks.json", None, client, None).await;
    assert_eq!(status, StatusCode::OK);
}

/// An unmatched path under the credential prefix 404s without ever touching
/// the rate limiter: `auth_router` wires `rate_limit` with `route_layer`, not
/// `layer`, so it only wraps routes actually registered on the `credentials`
/// sub-router. Before that fix, an unauthenticated caller could grow the
/// limiter's bucket map (and, on the Postgres backend, the
/// `rate_limit_buckets` table) without bound simply by varying the path of a
/// bogus request.
#[tokio::test]
async fn unmatched_credential_path_never_creates_a_bucket() {
    let (app, limiter) = build_app_with_limiter(
        3,
        TrustedProxies::default(),
        Arc::new(LoggingNotifier) as Arc<dyn Notifier>,
    );
    let client = peer("8.8.4.4");

    assert_eq!(limiter.tracked(), 0, "no buckets before any request");

    // Repeated hits against paths that don't match any registered route -
    // more than `rate_max` of them, so a naive `layer`-based wiring would
    // have thrown a 429 by the last one instead of a 404 every time.
    for path in [
        "/does-not-exist",
        "/signup/extra",
        "/token/extra/segment",
        "/whatever",
        "/whatever",
    ] {
        let status = call_peer(&app, "POST", path, None, client, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "path {path} must 404");
    }

    assert_eq!(
        limiter.tracked(),
        0,
        "an unmatched path must never create a rate-limit bucket"
    );

    // A real, matched route from the same client is still limited normally -
    // proving the fix didn't accidentally disable rate limiting altogether.
    let body = json!({"email": "unmatched-path@example.com", "password": "bad"});
    for _ in 0..3 {
        let status = call_peer(&app, "POST", "/token", Some(body.clone()), client, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let status = call_peer(&app, "POST", "/token", Some(body), client, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limiter.tracked(), 1, "matched routes still get one bucket");
}

#[tokio::test]
async fn missing_connect_info_fails_open_no_shared_lockout() {
    // If the server is wired without connect-info, requests must be let through
    // rather than collapse into one shared bucket that locks everyone out. The
    // `call` helper injects no ConnectInfo extension.
    let app = app(1);
    for n in 0..5 {
        // A distinct email per request: this test is about the per-IP limiter
        // failing open, not the per-account backoff (#194).
        let body = json!({"email": format!("rl{n}@example.com"), "password": "bad"});
        let (status, _) = call(&app, "POST", "/token", Some(body), "9.9.9.9").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "never throttled without a peer"
        );
    }
}

#[tokio::test]
async fn spoofed_xff_from_untrusted_peer_cannot_escape_the_limit() {
    // No trusted proxies: `X-Forwarded-For` must be ignored, so an attacker
    // rotating it on every request stays pinned to one (peer-derived) bucket.
    let app = app(3);
    let body = json!({"email": "rl@example.com", "password": "bad"});
    let attacker = peer("203.0.113.7");
    for i in 0..3 {
        let status = call_peer(
            &app,
            "POST",
            "/token",
            Some(body.clone()),
            attacker,
            Some(&format!("10.0.0.{i}")), // fresh spoofed client IP each time
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "request {i} not throttled"
        );
    }
    let status = call_peer(
        &app,
        "POST",
        "/token",
        Some(body),
        attacker,
        Some("10.0.0.99"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "rotating XFF must not grant a fresh bucket"
    );
}

#[tokio::test]
async fn absent_xff_keys_per_peer_with_no_global_lockout() {
    // With no forwarded header, each peer gets its own per-route bucket, so one
    // client exhausting its budget cannot lock another client out.
    let app = app(2);
    let body = json!({"email": "rl@example.com", "password": "bad"});
    let noisy = peer("198.51.100.10");
    let victim = peer("198.51.100.20");

    assert_eq!(
        call_peer(&app, "POST", "/token", Some(body.clone()), noisy, None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call_peer(&app, "POST", "/token", Some(body.clone()), noisy, None).await,
        StatusCode::UNAUTHORIZED
    );
    // Noisy client is now throttled...
    assert_eq!(
        call_peer(&app, "POST", "/token", Some(body.clone()), noisy, None).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // ...but a different peer is unaffected.
    assert_eq!(
        call_peer(&app, "POST", "/token", Some(body.clone()), victim, None).await,
        StatusCode::UNAUTHORIZED
    );

    // Buckets are also scoped per route: a fresh route has its own budget.
    assert_eq!(
        call_peer(&app, "POST", "/signin", Some(body), noisy, None).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn trusted_peer_honors_forwarded_for_per_client() {
    // When the peer is a trusted proxy, the real client (parsed right-to-left)
    // keys the bucket, so distinct real clients behind the proxy get independent
    // budgets.
    let app = app_with_proxies(2, TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap());
    let proxy = peer("10.9.9.9");
    let body = json!({"email": "rl@example.com", "password": "bad"});

    // Client A burns its budget.
    for _ in 0..2 {
        assert_eq!(
            call_peer(
                &app,
                "POST",
                "/token",
                Some(body.clone()),
                proxy,
                Some("203.0.113.1")
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call_peer(
            &app,
            "POST",
            "/token",
            Some(body.clone()),
            proxy,
            Some("203.0.113.1")
        )
        .await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Client B (same proxy, different forwarded IP) is independent.
    assert_eq!(
        call_peer(
            &app,
            "POST",
            "/token",
            Some(body),
            proxy,
            Some("203.0.113.2")
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn append_style_spoof_cannot_escape_the_limit_via_trusted_proxy() {
    // Behind an append-style trusted proxy the header is
    // `"<attacker-supplied>, <real-client>"`. Rotating the spoofed prefix must
    // not grant fresh buckets: all requests key on the proxy-appended real
    // client and hit the same limit.
    let app = app_with_proxies(2, TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap());
    let proxy = peer("10.9.9.9");
    let body = json!({"email": "rl@example.com", "password": "bad"});

    for i in 0..2 {
        let status = call_peer(
            &app,
            "POST",
            "/token",
            Some(body.clone()),
            proxy,
            Some(&format!("{i}.{i}.{i}.{i}, 203.0.113.7")), // rotating spoofed prefix
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "request {i} not throttled"
        );
    }
    let status = call_peer(
        &app,
        "POST",
        "/token",
        Some(body),
        proxy,
        Some("9.9.9.9, 203.0.113.7"),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn recover_is_always_202_and_indistinguishable() {
    let app = app(100);
    // Register one real account.
    call(
        &app,
        "POST",
        "/signup",
        Some(json!({"email": "known@example.com", "password": "known-password"})),
        "5.5.5.5",
    )
    .await;

    // A recover for the KNOWN account and for an UNKNOWN one return the exact
    // same status and body - no enumeration signal.
    let (s1, b1) = call(
        &app,
        "POST",
        "/recover",
        Some(json!({"email": "known@example.com"})),
        "5.5.5.5",
    )
    .await;
    let (s2, b2) = call(
        &app,
        "POST",
        "/recover",
        Some(json!({"email": "ghost@example.com"})),
        "5.5.5.5",
    )
    .await;
    assert_eq!(s1, StatusCode::ACCEPTED);
    assert_eq!(s2, StatusCode::ACCEPTED);
    assert_eq!(b1, b2, "known/unknown recover responses must be identical");
    assert!(b1["message"].as_str().is_some());
}

#[tokio::test]
async fn reset_with_valid_token_changes_password_and_revokes_sessions() {
    let (app, notifier) = app_capturing(100);
    let creds = json!({"email": "flow@example.com", "password": "before-reset-pw"});
    call(&app, "POST", "/signup", Some(creds.clone()), "6.6.6.6").await;

    // Establish a session whose refresh token must die on reset.
    let (_, tokens) = call(&app, "POST", "/token", Some(creds), "6.6.6.6").await;
    let old_refresh = tokens["refresh_token"].as_str().unwrap().to_string();

    // Request a reset; grab the token the notifier captured.
    let (status, _) = call(
        &app,
        "POST",
        "/recover",
        Some(json!({"email": "flow@example.com"})),
        "6.6.6.6",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let reset_token = notifier.reset_token();

    // Complete the reset.
    let (status, _) = call(
        &app,
        "POST",
        "/reset",
        Some(json!({"reset_token": reset_token, "new_password": "after-reset-pw"})),
        "6.6.6.6",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // New password authenticates; old one no longer does.
    let (status, _) = call(
        &app,
        "POST",
        "/token",
        Some(json!({"email": "flow@example.com", "password": "after-reset-pw"})),
        "6.6.6.6",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &app,
        "POST",
        "/token",
        Some(json!({"email": "flow@example.com", "password": "before-reset-pw"})),
        "6.6.6.6",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The pre-reset refresh token was revoked (forced re-login everywhere).
    let (status, _) = call(
        &app,
        "POST",
        "/token/refresh",
        Some(json!({"refresh_token": old_refresh})),
        "6.6.6.6",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reset_with_invalid_token_is_generic_failure() {
    let app = app(100);
    let (status, _) = call(
        &app,
        "POST",
        "/reset",
        Some(json!({"reset_token": "not-a-real-token", "new_password": "long-enough-pw"})),
        "7.7.7.7",
    )
    .await;
    // Generic 400: never a 401/404 that would distinguish a bad token from a
    // missing account.
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn recover_and_reset_are_rate_limited() {
    let app = app(3);
    let client = peer("8.8.8.8");
    let body = json!({"email": "rl@example.com"});
    for _ in 0..3 {
        let status = call_peer(&app, "POST", "/recover", Some(body.clone()), client, None).await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }
    // The 4th recover from the same client is throttled.
    let status = call_peer(&app, "POST", "/recover", Some(body), client, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // `/reset` has its own per-route budget.
    let reset_body = json!({"reset_token": "x", "new_password": "long-enough-pw"});
    for _ in 0..3 {
        let status = call_peer(
            &app,
            "POST",
            "/reset",
            Some(reset_body.clone()),
            client,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST); // invalid token, but not throttled
    }
    let status = call_peer(&app, "POST", "/reset", Some(reset_body), client, None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn verify_with_valid_token_confirms_email() {
    let (app, notifier) = app_capturing(100);
    // Sign up; the verification link is dispatched on a detached task.
    call(
        &app,
        "POST",
        "/signup",
        Some(json!({"email": "confirm@example.com", "password": "confirm-me-pw"})),
        "6.6.6.6",
    )
    .await;
    let token = notifier.verify_token();

    // A valid token confirms the email.
    let (status, _) = call(
        &app,
        "POST",
        "/verify",
        Some(json!({ "token": token })),
        "6.6.6.6",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Replaying the same token is rejected generically (single-use).
    let (status, _) = call(
        &app,
        "POST",
        "/verify",
        Some(json!({ "token": token })),
        "6.6.6.6",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn verify_with_bogus_token_is_generic_failure() {
    let app = app(100);
    let (status, _) = call(
        &app,
        "POST",
        "/verify",
        Some(json!({ "token": "not-a-real-verify-token" })),
        "7.7.7.7",
    )
    .await;
    // Generic 400: never a 401/404 that would distinguish a bad token from a
    // missing account.
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Durable outbox hand-off (andypalmi/amicu-planning#141)
//
// These flows used to detach the whole lookup -> mint -> store -> send sequence
// onto `tokio::spawn`, which Cloud Run freezes once the response is written. The
// work now runs ON the request path and ends in a durable enqueue, with a fixed
// response-time floor preserving the anti-enumeration property the detach used
// to provide. The tests below pin both halves of that.
// ---------------------------------------------------------------------------

/// The minimum response time `/recover` and `/verify/resend` are documented to
/// take (`amicu_auth::service`'s `ANTI_ENUMERATION_FLOOR`). Asserted rather than
/// imported because it is an internal constant of the service.
const ANTI_ENUMERATION_FLOOR: Duration = Duration::from_millis(250);

/// `/recover` hands the reset token to the notifier BEFORE it responds - it is
/// a durable enqueue now, not a detached background send. Read synchronously,
/// with no yielding: if the work were still detached this would be empty.
#[tokio::test]
async fn recover_enqueues_delivery_before_responding() {
    let (app, notifier) = app_capturing(100);
    let (status, _) = call(
        &app,
        "POST",
        "/signup",
        Some(json!({ "email": "outbox@example.com", "password": "correct horse battery staple" })),
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _) = call(
        &app,
        "POST",
        "/recover",
        Some(json!({ "email": "outbox@example.com" })),
        "10.0.0.1",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        notifier.last_reset_token().is_some(),
        "the reset link must be handed off on the request path, not spawned"
    );
}

/// Sign-up likewise queues the verification link inline.
#[tokio::test]
async fn signup_enqueues_verification_before_responding() {
    let (app, notifier) = app_capturing(100);
    let (status, _) = call(
        &app,
        "POST",
        "/signup",
        Some(json!({ "email": "inline@example.com", "password": "correct horse battery staple" })),
        "10.0.0.2",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(
        notifier.last_verify_token().is_some(),
        "the verification link must be handed off on the request path, not spawned"
    );
}

/// `/verify/resend` likewise queues inline for an existing, unverified account.
#[tokio::test]
async fn resend_verification_enqueues_before_responding() {
    let (app, notifier) = app_capturing(100);
    call(
        &app,
        "POST",
        "/signup",
        Some(json!({ "email": "resend@example.com", "password": "correct horse battery staple" })),
        "10.0.0.3",
    )
    .await;
    let queued_at_signup = notifier.last_verify_token();

    let (status, _) = call(
        &app,
        "POST",
        "/verify/resend",
        Some(json!({ "email": "resend@example.com" })),
        "10.0.0.3",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let after_resend = notifier.last_verify_token();
    assert!(after_resend.is_some());
    assert_ne!(
        queued_at_signup, after_resend,
        "resend must queue a FRESH verification token, on the request path"
    );
}

/// The anti-enumeration property, restated for the inline world: `/recover`
/// takes the same (floored) time whether or not the account exists. Both
/// branches must clear the floor - a branch that returned early would be an
/// account-existence oracle.
#[tokio::test]
async fn recover_response_time_is_floored_on_both_branches() {
    let (app, _notifier) = app_capturing(100);
    call(
        &app,
        "POST",
        "/signup",
        Some(json!({ "email": "timed@example.com", "password": "correct horse battery staple" })),
        "10.0.0.4",
    )
    .await;

    for email in ["timed@example.com", "nobody-here@example.com", ""] {
        let started = std::time::Instant::now();
        let (status, _) = call(
            &app,
            "POST",
            "/recover",
            Some(json!({ "email": email })),
            "10.0.0.4",
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(
            elapsed >= ANTI_ENUMERATION_FLOOR,
            "/recover for {email:?} returned in {elapsed:?}, under the \
             anti-enumeration floor of {ANTI_ENUMERATION_FLOOR:?}"
        );
    }
}

/// Same floor on `/verify/resend`, across all three of its branches (unknown
/// account, known-and-unverified, known-and-already-verified).
#[tokio::test]
async fn resend_verification_response_time_is_floored_on_every_branch() {
    let (app, notifier) = app_capturing(100);
    call(
        &app,
        "POST",
        "/signup",
        Some(json!({ "email": "floor@example.com", "password": "correct horse battery staple" })),
        "10.0.0.5",
    )
    .await;
    // Consume the verification link so the third branch (already verified) is
    // reachable below.
    let token = notifier.verify_token();
    let (status, _) = call(
        &app,
        "POST",
        "/verify",
        Some(json!({ "token": token })),
        "10.0.0.5",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for email in ["floor@example.com", "unknown@example.com"] {
        let started = std::time::Instant::now();
        let (status, _) = call(
            &app,
            "POST",
            "/verify/resend",
            Some(json!({ "email": email })),
            "10.0.0.5",
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(
            elapsed >= ANTI_ENUMERATION_FLOOR,
            "/verify/resend for {email:?} returned in {elapsed:?}, under the \
             anti-enumeration floor of {ANTI_ENUMERATION_FLOOR:?}"
        );
    }
}
