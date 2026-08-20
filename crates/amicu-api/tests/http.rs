//! End-to-end HTTP tests over the auth router (in-process, via `oneshot`).
#![allow(clippy::duration_suboptimal_units)]

use std::sync::Arc;
use std::time::Duration;

use amicu_api::{AuthApiState, RateLimiter, auth_router};
use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::jwks::Jwks;
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::service::AuthService;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verifier::{StaticJwksProvider, Verifier, VerifierConfig};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://auth.amicu.dev";
const AUD: &str = "amicu-api";

fn app(rate_max: u32) -> Router {
    let keyring = Keyring::new(SigningKey::generate().unwrap());
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));
    let service = Arc::new(AuthService::new(
        keyring,
        issuer,
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        "https://auth.amicu.dev",
    ));
    let limiter = Arc::new(RateLimiter::new(rate_max, Duration::from_secs(60)));
    auth_router(AuthApiState::new(service, limiter))
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

    // Sign up.
    let (status, body) = call(&app, "POST", "/signup", Some(creds.clone()), "10.0.0.1").await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["user_id"].as_str().unwrap().starts_with("usr_"));

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
async fn duplicate_signup_conflicts() {
    let app = app(100);
    let creds = json!({"email": "dup@example.com", "password": "some-password"});
    let (s1, _) = call(&app, "POST", "/signup", Some(creds.clone()), "3.3.3.3").await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, _) = call(&app, "POST", "/signup", Some(creds), "3.3.3.3").await;
    assert_eq!(s2, StatusCode::CONFLICT);
}

#[tokio::test]
async fn rate_limit_trips_after_max() {
    let app = app(3);
    let body = json!({"email": "rl@example.com", "password": "bad"});
    // The 4th credential request from the same client is throttled.
    for _ in 0..3 {
        let (status, _) = call(&app, "POST", "/token", Some(body.clone()), "9.9.9.9").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED); // unknown user, but not throttled
    }
    let (status, _) = call(&app, "POST", "/token", Some(body), "9.9.9.9").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // The unthrottled well-known endpoint still works from the same client.
    let (status, _) = call(&app, "GET", "/.well-known/jwks.json", None, "9.9.9.9").await;
    assert_eq!(status, StatusCode::OK);
}
