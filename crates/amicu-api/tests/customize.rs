//! Integration tests for the stateless, auth-gated deterministic customize
//! endpoint: `POST /recommend/customize`.

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
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";

/// A plannable config: a llama_cpp deployment that fits its hardware. Mirrors
/// `engine.rs`'s `PLANNABLE`.
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

/// Invalid: a deployment referencing resources that do not exist. Mirrors
/// `engine.rs`'s `DANGLING_REFS`.
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

/// Build the API app under test, returning it plus a freshly-minted access
/// token carrying `tier`. The customize endpoint admits any authenticated
/// user (min tier `Community`), same as the other wizard endpoints.
fn app_with(tier: Tier) -> (Router, String) {
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
            tier,
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
    let config_state = ConfigApiState::new(Arc::new(amicu_store::InMemoryConfigStore::new()));

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

/// A valid set of wizard answers accepted by every preset (mirrors
/// `recommend_wizard.rs`'s `answers()`).
fn answers() -> Value {
    json!({
        "context_length": 8192,
        "concurrency": 16,
        "eu_residency_required": false,
        "budget": { "amount": 500.0, "currency": "EUR" },
        "max_latency_ms": 1500
    })
}

/// Fetch a real ranked candidate for `preset_id` via `POST
/// /recommend/candidates`, returning its `hcl` and `graph` fields.
async fn a_candidate(app: &Router, token: &str, preset_id: &str) -> (String, Value) {
    let (status, body) = send(
        app,
        post(
            "/recommend/candidates",
            Some(token),
            &json!({ "preset_id": preset_id, "answers": answers() }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "candidates request for {preset_id}");
    let candidate = body["candidates"][0].clone();
    let hcl = candidate["hcl"].as_str().expect("hcl string").to_owned();
    let graph = candidate["graph"].clone();
    (hcl, graph)
}

#[tokio::test]
async fn round_trip_unchanged_candidate_plans_empty_and_returns_valid_hcl() {
    let (app, token) = app_with(Tier::Community);
    let (hcl, graph) = a_candidate(&app, &token, "local_dev_chat").await;

    let (status, body) = send(
        &app,
        post(
            "/recommend/customize",
            Some(&token),
            &json!({ "hcl": hcl, "base_graph": graph }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body["report"]["valid"].as_bool().unwrap_or(false));

    // The candidate's own graph submitted against itself as the prior: every
    // deployment is unchanged, and `build_plan` skips unchanged deployments
    // entirely (see `amicu-engine/src/planner.rs`'s `build_plan`, which
    // `continue`s rather than emitting a no-op action) - so no actions at all,
    // not merely all-no-op actions.
    let actions = body["plan"]["actions"].as_array().expect("actions array");
    assert!(
        actions.is_empty(),
        "expected no actions for an unchanged graph: {actions:?}"
    );

    // The returned HCL is non-empty and round-trips through the parser again.
    let response_hcl = body["hcl"].as_str().expect("hcl string");
    assert_ne!(response_hcl, "");
    amicu_hcl::parse_graph(response_hcl).expect("response hcl re-parses");
}

#[tokio::test]
async fn dangling_reference_edit_returns_422_with_no_plan() {
    let (app, token) = app_with(Tier::Community);
    let (hcl, graph) = a_candidate(&app, &token, "local_dev_chat").await;

    // Introduce a dangling model reference by editing the emitted HCL: rename
    // the model's own id reference target so `model.<id>.id` now points at an
    // id that does not exist in the graph. Every candidate has exactly one
    // `resource "model" "<id>"` block; find its id and rewrite every
    // `model.<id>.` reference to `model.nonexistent.`.
    let model_id = graph["models"][0]["id"]
        .as_str()
        .expect("graph has a model id")
        .to_owned();
    let broken_hcl = hcl.replace(&format!("model.{model_id}."), "model.nonexistent.");
    assert_ne!(broken_hcl, hcl, "expected the reference rewrite to apply");

    let (status, body) = send(
        &app,
        post(
            "/recommend/customize",
            Some(&token),
            &json!({ "hcl": broken_hcl, "base_graph": graph }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(body.get("plan").is_none(), "plan should be absent: {body}");
    assert!(!body["report"]["valid"].as_bool().unwrap_or(true));
    assert_ne!(body["report"]["diagnostics"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn syntactically_broken_hcl_returns_422_with_diagnostics() {
    let (app, token) = app_with(Tier::Community);

    let (status, body) = send(
        &app,
        post(
            "/recommend/customize",
            Some(&token),
            &json!({ "hcl": DANGLING_REFS.replace('}', "") }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(body.get("plan").is_none(), "plan should be absent: {body}");
    assert!(!body["report"]["valid"].as_bool().unwrap_or(true));
    let diagnostics = body["report"]["diagnostics"].as_array().expect("array");
    assert_ne!(diagnostics.len(), 0);
    assert_eq!(
        diagnostics[0]["code"].as_str().unwrap(),
        "AMICU-PARSE-002",
        "expected the generic parse-failure code: {body}"
    );
    // Nothing parsed, so the HCL is echoed back verbatim (unformatted).
    assert_eq!(
        body["hcl"].as_str().unwrap(),
        DANGLING_REFS.replace('}', "")
    );
}

#[tokio::test]
async fn structurally_invalid_hcl_returns_422_with_diagnostics() {
    // Distinct from a syntax error: DANGLING_REFS parses fine but fails
    // structural validation (unknown resource references).
    let (app, token) = app_with(Tier::Community);

    let (status, body) = send(
        &app,
        post(
            "/recommend/customize",
            Some(&token),
            &json!({ "hcl": DANGLING_REFS }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(body.get("plan").is_none(), "plan should be absent: {body}");
    assert!(!body["report"]["valid"].as_bool().unwrap_or(true));
}

#[tokio::test]
async fn omitted_base_graph_plans_every_action_as_create() {
    let (app, token) = app_with(Tier::Community);

    let (status, body) = send(
        &app,
        post(
            "/recommend/customize",
            Some(&token),
            &json!({ "hcl": PLANNABLE }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body["report"]["valid"].as_bool().unwrap_or(false));
    let actions = body["plan"]["actions"].as_array().expect("actions array");
    assert!(!actions.is_empty(), "expected at least one action");
    for action in actions {
        assert_eq!(action["kind"].as_str().unwrap(), "create");
    }
    assert!(body["graph"].is_object());
}

#[tokio::test]
async fn unauthenticated_request_is_rejected() {
    let (app, _token) = app_with(Tier::Community);
    let (status, _) = send(
        &app,
        post("/recommend/customize", None, &json!({ "hcl": PLANNABLE })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn identifier_that_fails_to_reformat_returns_422_not_500() {
    // `resource "model" "..."` labels are HCL strings, not identifiers, so a
    // space in the label parses fine but cannot be canonically re-emitted
    // (`amicu_hcl::format_graph` round-trip-safety check). This is a
    // realistic user-editing mistake, not an internal error: it must surface
    // as a 422 diagnostic like any other bad `hcl`, never a 500.
    let (app, token) = app_with(Tier::Community);
    let hcl_with_bad_label = PLANNABLE.replacen("\"m\"", "\"my model\"", 1);

    let (status, body) = send(
        &app,
        post(
            "/recommend/customize",
            Some(&token),
            &json!({ "hcl": hcl_with_bad_label }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(body.get("plan").is_none(), "plan should be absent: {body}");
    assert!(!body["report"]["valid"].as_bool().unwrap_or(true));
    let diagnostics = body["report"]["diagnostics"].as_array().expect("array");
    assert_eq!(
        diagnostics[0]["code"].as_str().unwrap(),
        "AMICU-PARSE-002",
        "expected the generic parse-failure code: {body}"
    );
    // Nothing was reformattable, so the HCL is echoed back verbatim.
    assert_eq!(body["hcl"].as_str().unwrap(), hcl_with_bad_label);
}

#[tokio::test]
async fn unparsable_base_graph_returns_400() {
    let (app, token) = app_with(Tier::Community);
    let (status, body) = send(
        &app,
        post(
            "/recommend/customize",
            Some(&token),
            &json!({ "hcl": PLANNABLE, "base_graph": "not an object" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body["error"].is_string());
}
