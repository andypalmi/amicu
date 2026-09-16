//! Integration tests for the config storage + version-history endpoints
//! (`/configs/*`): create/list/get, version history, append, rollback, and
//! the server-computed diff.
//!
//! These exercise the full contract over the assembled `api_router` against
//! an [`amicu_store::InMemoryConfigStore`] (no database needed): the auth gate
//! (401), canonicalization on create/append (400 on unparseable HCL), 404s for
//! unknown configs/versions, and that rollback mints a brand-new version
//! rather than rewriting history.

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

use std::sync::Arc;
use std::time::Duration;

use amicu_api::{
    AuthApiState, BillingApiState, ConfigApiState, EngineApiState, InMemoryOrgStore,
    InMemoryRateLimiter, OrgApiState, SessionApiState, api_router,
};
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verifier::StaticJwksProvider;
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_auth::{
    AuthService, IssuerConfig, Keyring, SigningKey, Tier, TokenIssuer, Verifier, VerifierConfig,
};
use amicu_auth::{Role, WorkspaceId};
use amicu_store::InMemoryConfigStore;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";

/// A valid config: one model + hardware profile, no deployments. Parses clean
/// and canonicalizes stably. Deliberately formatted with irregular whitespace
/// so a round-trip through the API's canonicalization step is observable.
const VALID_HCL: &str = r#"
resource "model"    "m" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-7B-Instruct"
  }
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "m-q4_k_m.gguf"
    size         = "4GiB"
  }
}

resource "hardware_profile" "hw" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m4_pro"
    count  = 1
    memory = "48GiB"
  }
  memory = "48GiB"
}
"#;

/// A second valid config, structurally different from [`VALID_HCL`] (a larger
/// context length), so a diff between the two is non-empty.
const VALID_HCL_V2: &str = r#"
resource "model" "m" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-7B-Instruct"
  }
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 16384
  variant {
    quantization = "q4_k_m"
    file         = "m-q4_k_m.gguf"
    size         = "4GiB"
  }
}

resource "hardware_profile" "hw" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m4_pro"
    count  = 1
    memory = "48GiB"
  }
  memory = "48GiB"
}
"#;

/// Build the API app under test plus a freshly-minted Community access token.
/// The config endpoints admit any authenticated user (min tier Community, the
/// same `EngineApiState` default `/validate`/`/plan` use).
fn app_with_token() -> (Router, String) {
    let key = SigningKey::generate().expect("generate key");
    let keyring = Keyring::new(key);
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));

    let jwks = keyring.jwks();
    let token = issuer
        .issue_access(
            keyring.active(),
            "usr_test",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Community,
            Vec::new(),
        )
        .expect("mint token");

    let service = Arc::new(AuthService::new(
        keyring,
        issuer,
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        ISS,
    ));
    let auth_state = AuthApiState::new(
        service.clone(),
        Arc::new(InMemoryRateLimiter::new(100, Duration::from_secs(60))),
    );

    let verifier = Arc::new(Verifier::new(
        StaticJwksProvider::new(jwks),
        VerifierConfig::new(ISS, AUD),
    ));
    let engine_state = EngineApiState::new(verifier.clone());
    let billing_state =
        BillingApiState::unconfigured(verifier.clone(), Arc::new(InMemoryUserStore::default()));
    let org_state = OrgApiState::new(verifier.clone(), Arc::new(InMemoryOrgStore::new()));
    let config_state = ConfigApiState::new(Arc::new(InMemoryConfigStore::new()));

    (
        api_router(
            auth_state,
            engine_state,
            billing_state,
            org_state,
            config_state,
            SessionApiState::new(service, verifier),
        ),
        token,
    )
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

fn post(path: &str, token: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .expect("build request")
}

/// Create a config via the API and return its id plus the created body.
async fn create(app: &Router, token: &str, name: &str, content: &str) -> (String, Value) {
    let (status, body) = send(
        app,
        post(
            "/configs",
            token,
            &json!({ "name": name, "content": content }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create {name}: {body}");
    (
        body["id"].as_str().expect("created id string").to_owned(),
        body,
    )
}

// ---------------------------------------------------------------------------
// Auth gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_create_is_401() {
    let (app, _token) = app_with_token();
    let (status, _) = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/configs")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "name": "n", "content": VALID_HCL }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unauthenticated_list_is_401() {
    let (app, _token) = app_with_token();
    let (status, _) = send(&app, get("/configs", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Create + canonicalization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_canonicalizes_content_and_returns_201() {
    let (app, token) = app_with_token();
    let (id, body) = create(&app, &token, "my-config", VALID_HCL).await;

    assert_eq!(body["name"], "my-config");
    assert_eq!(body["current_version"], 1);
    let content = body["content"].as_str().expect("content string");
    // Canonicalized: the irregular `"model"    "m"` spacing in VALID_HCL is
    // gone, and the content round-trips through the parser cleanly.
    assert!(!content.contains("\"model\"    \"m\""));
    let graph = amicu_hcl::parse_graph(content).expect("canonical content re-parses");
    assert_eq!(graph.models.len(), 1);
    assert!(!body["content_sha256"].as_str().unwrap().is_empty());

    // It is immediately fetchable by id.
    let (status, fetched) = send(&app, get(&format!("/configs/{id}"), Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["content"], body["content"]);
}

#[tokio::test]
async fn create_unparseable_hcl_is_400() {
    let (app, token) = app_with_token();
    let (status, body) = send(
        &app,
        post(
            "/configs",
            &token,
            &json!({ "name": "bad", "content": "this is not valid hcl {{{" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().is_some_and(|e| !e.is_empty()));
}

#[tokio::test]
async fn create_duplicate_name_is_409() {
    let (app, token) = app_with_token();
    create(&app, &token, "dup", VALID_HCL).await;

    let (status, body) = send(
        &app,
        post(
            "/configs",
            &token,
            &json!({ "name": "dup", "content": VALID_HCL_V2 }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("dup"));
}

// ---------------------------------------------------------------------------
// List + get
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_returns_every_created_config() {
    let (app, token) = app_with_token();
    create(&app, &token, "alpha", VALID_HCL).await;
    create(&app, &token, "beta", VALID_HCL).await;

    let (status, body) = send(&app, get("/configs", Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
}

#[tokio::test]
async fn get_unknown_config_is_404() {
    let (app, token) = app_with_token();
    let unknown = uuid::Uuid::new_v4();
    let (status, _) = send(&app, get(&format!("/configs/{unknown}"), Some(&token))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Version history: list, get, append
// ---------------------------------------------------------------------------

#[tokio::test]
async fn append_version_creates_a_new_version_and_updates_current() {
    let (app, token) = app_with_token();
    let (id, _) = create(&app, &token, "cfg", VALID_HCL).await;

    let (status, appended) = send(
        &app,
        post(
            &format!("/configs/{id}/versions"),
            &token,
            &json!({ "content": VALID_HCL_V2, "message": "bump context" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(appended["version_no"], 2);
    assert_eq!(appended["message"], "bump context");

    let (_, current) = send(&app, get(&format!("/configs/{id}"), Some(&token))).await;
    assert_eq!(current["current_version"], 2);
    assert!(current["content"].as_str().unwrap().contains("16384"));

    let (status, versions) =
        send(&app, get(&format!("/configs/{id}/versions"), Some(&token))).await;
    assert_eq!(status, StatusCode::OK);
    let versions = versions.as_array().unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["version_no"], 1);
    assert_eq!(versions[1]["version_no"], 2);
}

#[tokio::test]
async fn append_version_unparseable_hcl_is_400() {
    let (app, token) = app_with_token();
    let (id, _) = create(&app, &token, "cfg", VALID_HCL).await;

    let (status, _) = send(
        &app,
        post(
            &format!("/configs/{id}/versions"),
            &token,
            &json!({ "content": "not valid hcl {{{" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn append_version_unknown_config_is_404() {
    let (app, token) = app_with_token();
    let unknown = uuid::Uuid::new_v4();
    let (status, _) = send(
        &app,
        post(
            &format!("/configs/{unknown}/versions"),
            &token,
            &json!({ "content": VALID_HCL }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_specific_version_returns_its_content() {
    let (app, token) = app_with_token();
    let (id, _) = create(&app, &token, "cfg", VALID_HCL).await;
    send(
        &app,
        post(
            &format!("/configs/{id}/versions"),
            &token,
            &json!({ "content": VALID_HCL_V2 }),
        ),
    )
    .await;

    let (status, v1) = send(
        &app,
        get(&format!("/configs/{id}/versions/1"), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(v1["content"].as_str().unwrap().contains("8192"));

    let (status, v2) = send(
        &app,
        get(&format!("/configs/{id}/versions/2"), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(v2["content"].as_str().unwrap().contains("16384"));
}

#[tokio::test]
async fn get_unknown_version_is_404() {
    let (app, token) = app_with_token();
    let (id, _) = create(&app, &token, "cfg", VALID_HCL).await;
    let (status, _) = send(
        &app,
        get(&format!("/configs/{id}/versions/99"), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Rollback: append-only history
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rollback_creates_a_new_version_and_preserves_history() {
    let (app, token) = app_with_token();
    let (id, _) = create(&app, &token, "cfg", VALID_HCL).await;
    send(
        &app,
        post(
            &format!("/configs/{id}/versions"),
            &token,
            &json!({ "content": VALID_HCL_V2 }),
        ),
    )
    .await;

    // Roll back to v1's content.
    let (status, rolled) = send(
        &app,
        post(&format!("/configs/{id}/rollback/1"), &token, &Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // Rollback MINTS a new version (3), it never rewrites v1 or v2.
    assert_eq!(rolled["version_no"], 3);

    let (_, versions) = send(&app, get(&format!("/configs/{id}/versions"), Some(&token))).await;
    let versions = versions.as_array().unwrap();
    assert_eq!(
        versions.len(),
        3,
        "history is append-only: v1 and v2 remain"
    );

    // The config's current version now carries v1's content, copied.
    let (_, current) = send(&app, get(&format!("/configs/{id}"), Some(&token))).await;
    assert_eq!(current["current_version"], 3);
    assert!(current["content"].as_str().unwrap().contains("8192"));
    assert!(!current["content"].as_str().unwrap().contains("16384"));

    // v1 and v2 are both still independently fetchable, unchanged.
    let (_, v1) = send(
        &app,
        get(&format!("/configs/{id}/versions/1"), Some(&token)),
    )
    .await;
    assert!(v1["content"].as_str().unwrap().contains("8192"));
    let (_, v2) = send(
        &app,
        get(&format!("/configs/{id}/versions/2"), Some(&token)),
    )
    .await;
    assert!(v2["content"].as_str().unwrap().contains("16384"));
}

#[tokio::test]
async fn rollback_to_unknown_version_is_404() {
    let (app, token) = app_with_token();
    let (id, _) = create(&app, &token, "cfg", VALID_HCL).await;
    let (status, _) = send(
        &app,
        post(&format!("/configs/{id}/rollback/99"), &token, &Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

#[tokio::test]
async fn diff_returns_a_unified_diff_between_two_versions() {
    let (app, token) = app_with_token();
    let (id, _) = create(&app, &token, "cfg", VALID_HCL).await;
    send(
        &app,
        post(
            &format!("/configs/{id}/versions"),
            &token,
            &json!({ "content": VALID_HCL_V2 }),
        ),
    )
    .await;

    let (status, body) = send(
        &app,
        get(&format!("/configs/{id}/diff?from=1&to=2"), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["from"], 1);
    assert_eq!(body["to"], 2);
    let diff = body["diff"].as_str().expect("diff string");
    // A standard unified diff: `---`/`+++` file headers, `@@` hunk header, and
    // the changed line's old/new values both present.
    assert!(diff.contains("---"), "{diff}");
    assert!(diff.contains("+++"), "{diff}");
    assert!(diff.contains("@@"), "{diff}");
    assert!(diff.contains("8192"), "{diff}");
    assert!(diff.contains("16384"), "{diff}");
}

#[tokio::test]
async fn diff_unknown_version_is_404() {
    let (app, token) = app_with_token();
    let (id, _) = create(&app, &token, "cfg", VALID_HCL).await;
    let (status, _) = send(
        &app,
        get(&format!("/configs/{id}/diff?from=1&to=99"), Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
