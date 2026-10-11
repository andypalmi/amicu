//! API auth hardening (andypalmi/amicu-planning#194): security headers, body
//! limits, password max length, per-email buckets, per-account backoff, and
//! security-event emission (with a no-secrets-in-events check).

use std::sync::Arc;
use std::time::Duration;

use amicu_api::{
    AuthApiState, BillingApiState, ConfigApiState, EngineApiState, InMemoryOrgStore,
    InMemoryRateLimiter, OrgApiState, SessionApiState, api_router,
};
use amicu_auth::claims::Tier;
use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::security::testing::{Captured, capture};
use amicu_auth::service::AuthService;
use amicu_auth::throttle::InMemoryLoginThrottle;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verifier::{StaticJwksProvider, Verifier, VerifierConfig};
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_auth::{Role, WorkspaceId};
use amicu_store::InMemoryConfigStore;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://auth.amicu.dev";
const AUD: &str = "amicu-api";

struct Harness {
    app: Router,
    enterprise_token: String,
}

fn harness(email_limit: u32) -> Harness {
    let keyring = Keyring::new(SigningKey::generate().unwrap());
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));
    let jwks = keyring.jwks();
    let org = WorkspaceId::new();
    let actor = "00000000-0000-0000-0000-000000000001";
    let org_store = Arc::new(InMemoryOrgStore::new());
    org_store.add_member_seed(org, actor, Role::Owner);
    let enterprise_token = issuer
        .issue_access(
            keyring.active(),
            actor,
            org,
            Role::Owner,
            Tier::Enterprise,
            Vec::new(),
        )
        .unwrap();
    let service = Arc::new(
        AuthService::new(
            keyring,
            issuer,
            Arc::new(InMemoryUserStore::default()),
            Arc::new(InMemoryRefreshStore::default()),
            Arc::new(InMemoryResetStore::default()),
            Arc::new(InMemoryVerifyStore::default()),
            Arc::new(LoggingNotifier),
            ISS,
        )
        .with_login_throttle(Arc::new(InMemoryLoginThrottle::with_clock(Arc::new(
            || 1_000_000,
        )))),
    );
    let window = Duration::from_secs(60);
    let auth_state = AuthApiState::new(
        service.clone(),
        Arc::new(InMemoryRateLimiter::new(1000, window)),
    )
    .with_email_limiters(
        Arc::new(InMemoryRateLimiter::new(email_limit, window)),
        Arc::new(InMemoryRateLimiter::new(1000, window)),
    );
    let verifier = Arc::new(Verifier::new(
        StaticJwksProvider::new(jwks),
        VerifierConfig::new(ISS, AUD),
    ));
    let app = api_router(
        auth_state,
        EngineApiState::new(verifier.clone()),
        BillingApiState::unconfigured(verifier.clone(), Arc::new(InMemoryUserStore::default())),
        OrgApiState::new(verifier.clone(), org_store),
        ConfigApiState::new(Arc::new(InMemoryConfigStore::new())),
        SessionApiState::new(service, verifier),
    );
    Harness {
        app,
        enterprise_token,
    }
}

async fn post(
    app: &Router,
    uri: &str,
    body: Value,
    bearer: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    post_raw(app, uri, body.to_string(), bearer).await
}

async fn post_raw(
    app: &Router,
    uri: &str,
    body: String,
    bearer: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .extension(ConnectInfo(std::net::SocketAddr::from((
            [203, 0, 113, 9],
            5000,
        ))));
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get(app: &Router, uri: &str) -> (StatusCode, HeaderMap) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    (resp.status(), resp.headers().clone())
}

fn assert_security_headers(h: &HeaderMap) {
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert!(
        h["strict-transport-security"]
            .to_str()
            .unwrap()
            .starts_with("max-age=")
    );
    assert!(
        h["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    assert_eq!(h["x-frame-options"], "DENY");
    assert_eq!(h["referrer-policy"], "no-referrer");
}

#[tokio::test]
async fn security_headers_on_every_response_and_no_store_on_credential_routes() {
    let h = harness(100);
    let (s, hdrs, _) = post(
        &h.app,
        "/signin",
        json!({"email": "a@example.com", "password": "wrong-password"}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_security_headers(&hdrs);
    assert_eq!(hdrs["cache-control"], "no-store");

    // Success responses and unmatched routes too.
    let (s, hdrs, _) = post(&h.app, "/recover", json!({"email": "a@example.com"}), None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_security_headers(&hdrs);
    assert_eq!(hdrs["cache-control"], "no-store");
    let (_, hdrs) = get(&h.app, "/nope").await;
    assert_security_headers(&hdrs);

    // Public documents stay cacheable (no blanket no-store) but are still hardened.
    let (s, hdrs) = get(&h.app, "/.well-known/jwks.json").await;
    assert_eq!(s, StatusCode::OK);
    assert_security_headers(&hdrs);
    assert!(hdrs.get("cache-control").is_none());
}

#[tokio::test]
async fn oversized_bodies_are_rejected_with_413() {
    let h = harness(100);
    let big = "x".repeat(20 * 1024);
    let (s, hdrs, _) = post(
        &h.app,
        "/signin",
        json!({"email": "a@example.com", "password": big}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_security_headers(&hdrs);

    // Org routes carry an explicit limit too.
    let (s, _, _) = post(
        &h.app,
        "/org/members",
        json!({"user_id": "x".repeat(20 * 1024), "role": "member"}),
        Some(&h.enterprise_token),
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn password_max_length_is_enforced() {
    let h = harness(100);
    let ok = "p".repeat(256);
    let (s, _, _) = post(
        &h.app,
        "/signup",
        json!({"email": "long@example.com", "password": ok}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);

    let too_long = "p".repeat(257);
    let (s, _, body) = post(
        &h.app,
        "/signup",
        json!({"email": "long2@example.com", "password": too_long}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("maximum length"));

    // Sign-in with an over-long password is a plain generic 401.
    let (s, _, _) = post(
        &h.app,
        "/signin",
        json!({"email": "long@example.com", "password": "p".repeat(257)}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn per_email_bucket_limits_recover_and_is_per_email() {
    let h = harness(2);
    for _ in 0..2 {
        let (s, _, _) = post(&h.app, "/recover", json!({"email": "A@Example.com "}), None).await;
        assert_eq!(s, StatusCode::ACCEPTED);
    }
    // Same normalized email: throttled, whatever the casing/whitespace.
    let (s, _, _) = post(&h.app, "/recover", json!({"email": "a@example.com"}), None).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    // The resend route has its own bucket, and another email is unaffected.
    let (s, _, _) = post(&h.app, "/recover", json!({"email": "b@example.com"}), None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (s, _, _) = post(
        &h.app,
        "/verify/resend",
        json!({"email": "a@example.com"}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    // Unknown and registered emails are limited identically (no signal).
    post(
        &h.app,
        "/signup",
        json!({"email": "real@example.com", "password": "correct horse battery"}),
        None,
    )
    .await;
    let mut codes = Vec::new();
    for email in ["real@example.com", "ghost@example.com"] {
        let mut seq = Vec::new();
        for _ in 0..3 {
            seq.push(
                post(&h.app, "/recover", json!({ "email": email }), None)
                    .await
                    .0,
            );
        }
        codes.push(seq);
    }
    assert_eq!(codes[0], codes[1]);
}

#[tokio::test]
async fn account_backoff_is_generic_and_identical_for_unknown_accounts() {
    let h = harness(100);
    post(
        &h.app,
        "/signup",
        json!({"email": "victim@example.com", "password": "correct horse battery"}),
        None,
    )
    .await;

    let mut seqs = Vec::new();
    for email in ["victim@example.com", "ghost@example.com"] {
        let mut seq = Vec::new();
        for _ in 0..6 {
            let (s, hdrs, body) = post(
                &h.app,
                "/signin",
                json!({"email": email, "password": "wrong-password-1"}),
                None,
            )
            .await;
            seq.push((s, body["error"].as_str().unwrap().to_owned()));
            if s == StatusCode::TOO_MANY_REQUESTS {
                assert!(hdrs.contains_key("retry-after"));
            }
        }
        seqs.push(seq);
    }
    // Same status/body sequence whether or not the account exists.
    assert_eq!(seqs[0], seqs[1]);
    // 3 free attempts + the one that arms the lock are plain 401s, then 429.
    assert!(
        seqs[0][..4]
            .iter()
            .all(|(s, _)| *s == StatusCode::UNAUTHORIZED)
    );
    assert!(
        seqs[0][4..]
            .iter()
            .all(|(s, _)| *s == StatusCode::TOO_MANY_REQUESTS)
    );

    // Locked means locked: even the correct password is refused until it lapses.
    let (s, _, _) = post(
        &h.app,
        "/signin",
        json!({"email": "victim@example.com", "password": "correct horse battery"}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
}

fn dump(events: &[Captured]) -> String {
    format!("{events:?}")
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn security_events_are_emitted_and_leak_no_secrets() {
    let (collector, _guard) = capture();
    let h = harness(100);
    let email = "Secret.Person@Example.com";
    let password = "hunter2-very-secret-pw";

    post(
        &h.app,
        "/signup",
        json!({"email": email, "password": password}),
        None,
    )
    .await;
    let (s, _, tokens) = post(
        &h.app,
        "/signin",
        json!({"email": email, "password": password}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let refresh = tokens["refresh_token"].as_str().unwrap().to_owned();
    let access = tokens["access_token"].as_str().unwrap().to_owned();
    post(
        &h.app,
        "/signin",
        json!({"email": email, "password": "not-the-password"}),
        None,
    )
    .await;
    post(&h.app, "/recover", json!({"email": email}), None).await;
    // Refresh, then replay the old token => reuse detection.
    let (s, _, _) = post(
        &h.app,
        "/token/refresh",
        json!({"refresh_token": refresh}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = post(
        &h.app,
        "/token/refresh",
        json!({"refresh_token": refresh}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    post(
        &h.app,
        "/reset",
        json!({"reset_token": "totally-bogus-reset-token", "new_password": "another-secret-pw-1"}),
        None,
    )
    .await;
    post(
        &h.app,
        "/verify",
        json!({"token": "bogus-verify-token-value"}),
        None,
    )
    .await;
    // A role change in the org console.
    let target = "00000000-0000-0000-0000-0000000000aa";
    post(
        &h.app,
        "/org/members",
        json!({"user_id": target, "role": "member"}),
        Some(&h.enterprise_token),
    )
    .await;

    let events = collector.security_events();
    let names: Vec<&str> = events.iter().map(Captured::name).collect();
    for expected in [
        "signin_succeeded",
        "signin_failed",
        "reset_requested",
        "refresh_reuse_detected",
        "reset_completed",
        "verify_failed",
        "member_added",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }

    // Hashes are present on request-level events; the IP is hashed, not raw.
    let failed = events.iter().find(|e| e.name() == "signin_failed").unwrap();
    assert_eq!(failed.get("email_hash").unwrap().len(), 32);
    assert_eq!(failed.get("ip_hash").unwrap().len(), 32);
    assert_eq!(failed.get("reason"), Some("invalid_credentials"));

    // Nothing sensitive anywhere in any captured event (not just security ones).
    let all = format!("{:?}", collector.0.lock().unwrap());
    let secret_bits = [
        email,
        &email.to_lowercase(),
        "secret.person",
        password,
        "not-the-password",
        "another-secret-pw-1",
        "totally-bogus-reset-token",
        "bogus-verify-token-value",
        refresh.as_str(),
        access.as_str(),
        "203.0.113.9",
    ];
    for bit in secret_bits {
        assert!(
            !all.contains(bit),
            "event stream leaked {bit:?}: {}",
            dump(&events)
        );
    }
}
