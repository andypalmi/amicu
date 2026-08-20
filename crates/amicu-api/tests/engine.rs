//! Integration tests for the stateless, auth-gated engine endpoints.

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

use std::sync::Arc;
use std::time::Duration;

use amicu_api::{AuthApiState, EngineApiState, RateLimiter, api_router};
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verifier::StaticJwksProvider;
use amicu_auth::{
    AuthService, IssuerConfig, Keyring, SigningKey, Tier, TokenIssuer, Verifier, VerifierConfig,
};
use amicu_core::diagnostic::DiagnosticReport;
use amicu_plan::Plan;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";

/// A valid config: one model + hardware profile, no deployments. Parses clean
/// and validates with no diagnostics.
const VALID_NO_DEPLOY: &str = r#"
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

/// A plannable config: a llama_cpp deployment that fits its hardware.
const PLANNABLE: &str = r#"
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

resource "deployment" "d" {
  model    = model.m.id
  hardware = hardware_profile.hw.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}
"#;

/// Invalid: a deployment referencing resources that do not exist.
const DANGLING_REFS: &str = r#"
resource "deployment" "d" {
  model    = model.missing.id
  hardware = hardware_profile.missing.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}
"#;

/// Build the API app under test, returning it plus a freshly-minted access token
/// carrying `tier`. The engine endpoints require at least `min_tier`.
fn app_with(tier: Tier, min_tier: Tier) -> (Router, String) {
    let key = SigningKey::generate().expect("generate key");
    let keyring = Keyring::new(key);
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));

    let jwks = keyring.jwks();
    let token = issuer
        .issue_access(keyring.active(), "usr_test", tier, Vec::new())
        .expect("mint token");

    let service = Arc::new(AuthService::new(
        keyring,
        issuer,
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(LoggingNotifier),
        ISS,
    ));
    let auth_state = AuthApiState::new(
        service,
        Arc::new(RateLimiter::new(100, Duration::from_secs(60))),
    );

    let verifier = Verifier::new(StaticJwksProvider::new(jwks), VerifierConfig::new(ISS, AUD));
    let engine_state = EngineApiState::new(Arc::new(verifier)).with_min_tier(min_tier);

    (api_router(auth_state, engine_state), token)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = app.clone().oneshot(req).await.expect("request");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, json)
}

fn post(path: &str, token: Option<&str>, body: &serde_json::Value) -> Request<Body> {
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
async fn validate_valid_config_returns_200_valid_report() {
    let (app, token) = app_with(Tier::Community, Tier::Community);
    let (status, body) = send(
        &app,
        post(
            "/validate",
            Some(&token),
            &serde_json::json!({ "config": VALID_NO_DEPLOY }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let report: DiagnosticReport = serde_json::from_value(body).expect("report");
    assert!(
        report.valid,
        "expected a clean config to be valid: {report:?}"
    );
}

#[tokio::test]
async fn validate_invalid_config_returns_200_with_diagnostics() {
    let (app, token) = app_with(Tier::Community, Tier::Community);
    let (status, body) = send(
        &app,
        post(
            "/validate",
            Some(&token),
            &serde_json::json!({ "config": DANGLING_REFS }),
        ),
    )
    .await;
    // A config that parses but fails validation is still a 200; the report
    // carries the errors.
    assert_eq!(status, StatusCode::OK);
    let report: DiagnosticReport = serde_json::from_value(body).expect("report");
    assert!(!report.valid);
    assert!(!report.diagnostics.is_empty());
}

#[tokio::test]
async fn validate_parse_error_returns_400() {
    let (app, token) = app_with(Tier::Community, Tier::Community);
    let (status, _) = send(
        &app,
        post(
            "/validate",
            Some(&token),
            &serde_json::json!({ "config": "this is not valid hcl {{{" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn plan_valid_config_returns_200_plan() {
    let (app, token) = app_with(Tier::Community, Tier::Community);
    let (status, body) = send(
        &app,
        post(
            "/plan",
            Some(&token),
            &serde_json::json!({ "config": PLANNABLE }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let plan: Plan = serde_json::from_value(body).expect("plan");
    assert_eq!(plan.version, Plan::VERSION);
    assert!(!plan.actions.is_empty(), "expected at least one action");
}

#[tokio::test]
async fn plan_invalid_config_returns_422_with_diagnostics() {
    let (app, token) = app_with(Tier::Community, Tier::Community);
    let (status, body) = send(
        &app,
        post(
            "/plan",
            Some(&token),
            &serde_json::json!({ "config": DANGLING_REFS }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let report: DiagnosticReport = serde_json::from_value(body).expect("report");
    assert!(!report.valid);
}

#[tokio::test]
async fn unauthenticated_request_is_rejected() {
    let (app, _token) = app_with(Tier::Community, Tier::Community);
    for path in ["/validate", "/plan"] {
        let (status, _) = send(
            &app,
            post(
                path,
                None,
                &serde_json::json!({ "config": VALID_NO_DEPLOY }),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} should require auth"
        );
    }
}

#[tokio::test]
async fn insufficient_tier_is_forbidden() {
    // Endpoint requires Enterprise; a Pro token is authenticated but too low.
    let (app, token) = app_with(Tier::Pro, Tier::Enterprise);
    let (status, _) = send(
        &app,
        post(
            "/validate",
            Some(&token),
            &serde_json::json!({ "config": VALID_NO_DEPLOY }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn oversized_body_is_rejected() {
    // A valid token passes the auth gate; the body cap then rejects the payload
    // before parsing. Build a body comfortably larger than the 256 KiB limit.
    // The 413 body is plain text, so bypass the JSON-parsing `send` helper.
    let (app, token) = app_with(Tier::Community, Tier::Community);
    let huge = "x".repeat(400 * 1024);
    let resp = app
        .oneshot(post(
            "/validate",
            Some(&token),
            &serde_json::json!({ "config": huge }),
        ))
        .await
        .expect("request");
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn openapi_json_is_served() {
    let (app, _token) = app_with(Tier::Community, Tier::Community);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["openapi"].as_str().unwrap_or(""), "3.1.0");
    assert!(doc["paths"]["/validate"].is_object());
}
