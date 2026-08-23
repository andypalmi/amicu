//! Integration tests for the workspace-claim session surface
//! (andypalmi/amicu-planning#129): `GET /workspaces` and
//! `POST /session/workspace`.
//!
//! These exercise the real router, gate, and auth service end-to-end over the
//! in-memory stores. A user provisioned in-memory is Owner of exactly one
//! (personal) workspace, so the harness signs a user up + in through the service
//! to get a token whose `sub` the membership seam resolves.

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

use std::sync::Arc;

use amicu_api::{SessionApiState, session_router};
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::user::{InMemoryUserStore, UserStore};
use amicu_auth::verifier::StaticJwksProvider;
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_auth::{
    AuthService, IssuerConfig, Keyring, SigningKey, TokenIssuer, Verifier, VerifierConfig,
    WorkspaceId,
};
use amicu_core::secret::SecretValue;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";
const EMAIL: &str = "member@example.test";
const PASSWORD: &str = "correct horse staple";

/// Build the session router plus: a live access token for a freshly-provisioned
/// user, that user's `sub`, and their personal workspace id.
async fn harness() -> (Router, String, String, WorkspaceId) {
    let key = SigningKey::generate().expect("generate key");
    let keyring = Keyring::new(key);
    let jwks = keyring.jwks();
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));

    let users = Arc::new(InMemoryUserStore::default());
    let service = Arc::new(AuthService::new(
        keyring,
        issuer,
        users.clone(),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        ISS,
    ));

    service
        .sign_up(EMAIL, &pw(PASSWORD))
        .await
        .expect("sign up");
    let pair = service
        .sign_in(EMAIL, &pw(PASSWORD))
        .await
        .expect("sign in");
    let user = users
        .get_by_email(EMAIL)
        .await
        .expect("lookup")
        .expect("user present");

    let verifier = Arc::new(Verifier::new(
        StaticJwksProvider::new(jwks),
        VerifierConfig::new(ISS, AUD),
    ));
    let app = session_router(SessionApiState::new(service, verifier));
    (app, pair.access_token, user.id, user.personal_workspace_id)
}

fn pw(s: &str) -> SecretValue {
    SecretValue::new(s.as_bytes().to_vec())
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.expect("request");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, json)
}

fn get(path: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(t) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    builder.body(Body::empty()).expect("build request")
}

fn post(path: &str, token: Option<&str>, body: &Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("build request")
}

#[tokio::test]
async fn list_workspaces_returns_the_callers_personal_workspace() {
    let (app, token, _sub, personal) = harness().await;
    let (status, body) = send(&app, get("/workspaces", Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    let list = body["workspaces"].as_array().expect("workspaces array");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["workspace_id"], personal.to_string());
    assert_eq!(list[0]["role"], "owner");
}

#[tokio::test]
async fn list_workspaces_requires_authentication() {
    let (app, _token, _sub, _personal) = harness().await;
    let (status, _) = send(&app, get("/workspaces", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn switch_to_a_member_workspace_mints_a_scoped_pair() {
    let (app, token, _sub, personal) = harness().await;
    let (status, body) = send(
        &app,
        post(
            "/session/workspace",
            Some(&token),
            &json!({ "workspace_id": personal.to_string() }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["access_token"].is_string());
    assert!(body["refresh_token"].is_string());
    assert_eq!(body["token_type"], "Bearer");
}

#[tokio::test]
async fn switch_to_a_non_member_workspace_is_forbidden_and_mints_nothing() {
    let (app, token, _sub, _personal) = harness().await;
    // A workspace the caller is not a member of: re-checked server-side -> 403.
    let stranger = WorkspaceId::new();
    let (status, body) = send(
        &app,
        post(
            "/session/workspace",
            Some(&token),
            &json!({ "workspace_id": stranger.to_string() }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.get("access_token").is_none());
}

#[tokio::test]
async fn switch_rejects_a_malformed_workspace_id() {
    let (app, token, _sub, _personal) = harness().await;
    let (status, _) = send(
        &app,
        post(
            "/session/workspace",
            Some(&token),
            &json!({ "workspace_id": "not-a-uuid" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn switch_requires_authentication() {
    let (app, _token, _sub, personal) = harness().await;
    let (status, _) = send(
        &app,
        post(
            "/session/workspace",
            None,
            &json!({ "workspace_id": personal.to_string() }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
