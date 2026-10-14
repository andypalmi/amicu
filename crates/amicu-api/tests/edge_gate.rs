//! Edge-gate composition tests.
//!
//! Mirrors the composition `amicu-server`'s `build_app_with_database_url`
//! uses: [`edge_token_gate`] layered over the whole API surface (`auth_router`
//! stands in for the full `api_router` here - both the gate and the
//! surrounding `.layer` mechanics are identical), with the health router
//! merged in AFTER, so it stays outside the gate. Also covers the credential
//! rate limiter's `CF-Connecting-IP` trust decision end to end over a real
//! router (not just the unit-level [`amicu_api::edge::EdgeTrust`] tests).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use amicu_api::{
    AuthApiState, EdgeGateState, EdgeTrust, HealthApiState, InMemoryRateLimiter, TrustedProxies,
    auth_router, edge_token_gate, health_router,
};
use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::service::AuthService;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_core::secret::SecretValue;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn_with_state;
use serde_json::json;
use tower::ServiceExt;

const ISS: &str = "https://auth.amicu.dev";
const AUD: &str = "amicu";
const EDGE_TOKEN: &str = "s3cret-edge-token";
const EDGE_TOKEN_HEADER: &str = "x-amicu-edge-token";
const CF_CONNECTING_IP_HEADER: &str = "cf-connecting-ip";

fn peer(ip: &str) -> SocketAddr {
    SocketAddr::new(ip.parse().unwrap(), 40000)
}

/// Mirrors `amicu-server`'s composition: the edge gate wraps the whole API
/// surface (`auth_router` here), health is merged in AFTER - outside the
/// gate.
fn app(rate_max: u32, trusted: TrustedProxies, edge_trust: EdgeTrust) -> Router {
    let keyring = Keyring::new(SigningKey::generate().unwrap());
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));
    let service = Arc::new(AuthService::new(
        keyring,
        issuer,
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        "https://auth.amicu.dev",
    ));
    let limiter = Arc::new(InMemoryRateLimiter::new(rate_max, Duration::from_secs(60)));
    let auth_state = AuthApiState::new(service, limiter)
        .with_trusted_proxies(trusted)
        .with_edge_trust(edge_trust.clone());

    let api = auth_router(auth_state).layer(from_fn_with_state(
        EdgeGateState { trust: edge_trust },
        edge_token_gate,
    ));
    let health_state = HealthApiState::new(None, Duration::from_millis(2_000));
    api.merge(health_router(health_state))
}

async fn get(app: &Router, uri: &str, token: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(t) = token {
        builder = builder.header(EDGE_TOKEN_HEADER, t);
    }
    let req = builder.body(Body::empty()).unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// `POST /token` with bogus credentials, so a within-budget attempt is `401`
/// and an over-budget one is `429` - the two outcomes this file's rate-limit
/// tests distinguish between.
async fn attempt_token(
    app: &Router,
    peer_ip: &str,
    cf_ip: Option<&str>,
    edge_token: Option<&str>,
) -> StatusCode {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/token")
        .header("content-type", "application/json")
        .extension(ConnectInfo(peer(peer_ip)));
    if let Some(ip) = cf_ip {
        builder = builder.header(CF_CONNECTING_IP_HEADER, ip);
    }
    if let Some(t) = edge_token {
        builder = builder.header(EDGE_TOKEN_HEADER, t);
    }
    let body = json!({"email": "nobody@example.com", "password": "wrong"}).to_string();
    let req = builder.body(Body::from(body)).unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn health_routes_are_exempt_from_a_required_edge_token() {
    let edge_trust = EdgeTrust::new(Some(SecretValue::new(EDGE_TOKEN)), true);
    let app = app(100, TrustedProxies::default(), edge_trust);

    assert_eq!(get(&app, "/healthz", None).await, StatusCode::OK);
    assert_eq!(get(&app, "/readyz", None).await, StatusCode::OK);
}

#[tokio::test]
async fn missing_edge_token_is_403_before_any_handler_when_required() {
    let edge_trust = EdgeTrust::new(Some(SecretValue::new(EDGE_TOKEN)), true);
    let app = app(100, TrustedProxies::default(), edge_trust);

    assert_eq!(
        get(&app, "/.well-known/jwks.json", None).await,
        StatusCode::FORBIDDEN,
        "even an otherwise-unauthenticated, unthrottled route is gated"
    );
}

#[tokio::test]
async fn wrong_edge_token_is_403_when_required() {
    let edge_trust = EdgeTrust::new(Some(SecretValue::new(EDGE_TOKEN)), true);
    let app = app(100, TrustedProxies::default(), edge_trust);

    assert_eq!(
        get(&app, "/.well-known/jwks.json", Some("wrong")).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn correct_edge_token_is_let_through_when_required() {
    let edge_trust = EdgeTrust::new(Some(SecretValue::new(EDGE_TOKEN)), true);
    let app = app(100, TrustedProxies::default(), edge_trust);

    assert_eq!(
        get(&app, "/.well-known/jwks.json", Some(EDGE_TOKEN)).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn not_required_lets_every_request_through_regardless_of_token() {
    let edge_trust = EdgeTrust::new(Some(SecretValue::new(EDGE_TOKEN)), false);
    let app = app(100, TrustedProxies::default(), edge_trust);

    assert_eq!(
        get(&app, "/.well-known/jwks.json", None).await,
        StatusCode::OK
    );
    assert_eq!(
        get(&app, "/.well-known/jwks.json", Some("wrong")).await,
        StatusCode::OK
    );
}

/// `CF-Connecting-IP` becomes the rate-limit key ONLY once a valid edge token
/// is presented: two requests from DIFFERENT peer addresses but the SAME
/// `CF-Connecting-IP` + a valid token share one budget (the second is
/// throttled); the same two peers without a valid token get independent,
/// peer-keyed budgets (both merely `401`, since with `rate_max: 1` each peer's
/// own single hit is within its own budget).
#[tokio::test]
async fn cf_connecting_ip_is_the_rate_limit_key_only_with_a_valid_edge_token() {
    let edge_trust = EdgeTrust::new(Some(SecretValue::new(EDGE_TOKEN)), false);
    let app = app(1, TrustedProxies::default(), edge_trust);

    // Valid token + same CF-Connecting-IP from two different peers: one
    // shared budget of 1, so the second attempt is throttled.
    let first = attempt_token(&app, "203.0.113.1", Some("198.51.100.5"), Some(EDGE_TOKEN)).await;
    let second = attempt_token(&app, "203.0.113.2", Some("198.51.100.5"), Some(EDGE_TOKEN)).await;
    assert_eq!(
        first,
        StatusCode::UNAUTHORIZED,
        "first hit is within budget"
    );
    assert_eq!(
        second,
        StatusCode::TOO_MANY_REQUESTS,
        "CF-Connecting-IP was trusted, so both requests shared one bucket"
    );
}

#[tokio::test]
async fn cf_connecting_ip_is_ignored_without_a_valid_edge_token() {
    let edge_trust = EdgeTrust::new(Some(SecretValue::new(EDGE_TOKEN)), false);
    let app = app(1, TrustedProxies::default(), edge_trust);

    // Same CF-Connecting-IP on both, but no edge token: falls back to
    // peer-address keying, so two DIFFERENT peers get independent budgets.
    let first = attempt_token(&app, "203.0.113.1", Some("198.51.100.5"), None).await;
    let second = attempt_token(&app, "203.0.113.2", Some("198.51.100.5"), None).await;
    assert_eq!(first, StatusCode::UNAUTHORIZED);
    assert_eq!(
        second,
        StatusCode::UNAUTHORIZED,
        "no valid edge token: CF-Connecting-IP is not trusted, each peer has its own budget"
    );
}

#[tokio::test]
async fn cf_connecting_ip_is_ignored_with_a_wrong_edge_token() {
    let edge_trust = EdgeTrust::new(Some(SecretValue::new(EDGE_TOKEN)), false);
    let app = app(1, TrustedProxies::default(), edge_trust);

    let first = attempt_token(&app, "203.0.113.1", Some("198.51.100.5"), Some("wrong")).await;
    let second = attempt_token(&app, "203.0.113.2", Some("198.51.100.5"), Some("wrong")).await;
    assert_eq!(first, StatusCode::UNAUTHORIZED);
    assert_eq!(second, StatusCode::UNAUTHORIZED);
}

/// The pre-existing `X-Forwarded-For`/`AMICU_TRUSTED_PROXIES` path (no edge
/// token involved at all) keeps working unchanged: a trusted-proxy peer's XFF
/// chain is honored, right-to-left, same as before the edge-token gate.
#[tokio::test]
async fn x_forwarded_for_with_trusted_proxies_still_works_with_no_edge_trust_configured() {
    let trusted = TrustedProxies::from_strs(["10.0.0.0/8"]).unwrap();
    let app = app(1, trusted, EdgeTrust::default());

    let builder = |real_client: &str| {
        Request::builder()
            .method("POST")
            .uri("/token")
            .header("content-type", "application/json")
            .header("x-forwarded-for", real_client)
            .extension(ConnectInfo(peer("10.0.0.1"))) // trusted proxy peer
    };
    let body =
        || Body::from(json!({"email": "nobody@example.com", "password": "wrong"}).to_string());

    let first = app
        .clone()
        .oneshot(builder("198.51.100.9").body(body()).unwrap())
        .await
        .unwrap()
        .status();
    let second = app
        .clone()
        .oneshot(builder("198.51.100.9").body(body()).unwrap())
        .await
        .unwrap()
        .status();
    assert_eq!(first, StatusCode::UNAUTHORIZED);
    assert_eq!(
        second,
        StatusCode::TOO_MANY_REQUESTS,
        "same real client via trusted-proxy XFF shares one budget"
    );
}
