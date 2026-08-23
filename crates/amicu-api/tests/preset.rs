//! Integration tests for the stateless, auth-gated guided-wizard preset
//! endpoints: `GET /presets`, `GET /presets/{id}`, and `POST /presets/{id}/emit`.

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

use std::sync::Arc;
use std::time::Duration;

use amicu_api::{
    AuthApiState, BillingApiState, ConfigApiState, EngineApiState, InMemoryOrgStore, OrgApiState,
    RateLimiter, api_router,
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
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";

/// The three catalog preset ids the endpoints expose.
const PRESET_IDS: [&str; 3] = [
    "local_dev_chat",
    "private_eu_production",
    "cost_optimized_batch",
];

/// Build the API app under test plus a freshly-minted access token carrying
/// `tier`. The preset endpoints admit any authenticated user (min tier
/// `Community`).
fn app_with(tier: Tier) -> (Router, String) {
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
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        ISS,
    ));
    let auth_state = AuthApiState::new(
        service,
        Arc::new(RateLimiter::new(100, Duration::from_secs(60))),
    );

    let verifier = Arc::new(Verifier::new(
        StaticJwksProvider::new(jwks),
        VerifierConfig::new(ISS, AUD),
    ));
    let engine_state = EngineApiState::new(verifier.clone());
    let billing_state =
        BillingApiState::unconfigured(verifier.clone(), Arc::new(InMemoryUserStore::default()));
    let org_state = OrgApiState::new(verifier, Arc::new(InMemoryOrgStore::new()));
    let config_state = ConfigApiState::new(Arc::new(amicu_store::InMemoryConfigStore::new()));

    (
        api_router(
            auth_state,
            engine_state,
            billing_state,
            org_state,
            config_state,
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

/// A valid set of wizard answers accepted by every preset.
fn answers() -> Value {
    json!({
        "context_length": 8192,
        "concurrency": 16,
        "eu_residency_required": false,
        "budget": { "amount": 500.0, "currency": "EUR" },
        "max_latency_ms": 1500
    })
}

#[tokio::test]
async fn list_presets_returns_the_three_catalog_entries() {
    let (app, token) = app_with(Tier::Community);
    let (status, body) = send(&app, get("/presets", Some(&token))).await;

    assert_eq!(status, StatusCode::OK);
    let entries = body.as_array().expect("array of summaries");
    assert_eq!(entries.len(), 3, "the catalog has three presets");

    let ids: Vec<&str> = entries
        .iter()
        .map(|e| e["id"].as_str().expect("id string"))
        .collect();
    for expected in PRESET_IDS {
        assert!(ids.contains(&expected), "missing preset id {expected}");
    }

    // Every card carries its gallery fields and is Community-usable.
    for entry in entries {
        assert!(entry["name"].is_string());
        assert!(entry["tagline"].is_string());
        assert!(entry["fit"].is_string());
        assert!(entry["constraints"].is_array());
        assert_eq!(entry["min_tier"], "community");
    }
}

#[tokio::test]
async fn get_preset_by_id_returns_detail_for_every_preset() {
    let (app, token) = app_with(Tier::Community);
    for id in PRESET_IDS {
        let (status, body) = send(&app, get(&format!("/presets/{id}"), Some(&token))).await;
        assert_eq!(status, StatusCode::OK, "{id}");
        assert_eq!(body["summary"]["id"], id, "{id}");
        // The detail seeds the wizard: requirement defaults and an estimate.
        assert!(body["requirement_defaults"]["id"].is_string(), "{id}");
        assert!(
            body["default_estimate"]["assumed_prompt_tokens"].is_number(),
            "{id}"
        );
        assert!(
            body["default_estimate"]["expected_ttft_ms"].is_object(),
            "{id}"
        );
    }
}

#[tokio::test]
async fn get_unknown_preset_is_404() {
    let (app, token) = app_with(Tier::Community);
    let (status, body) = send(&app, get("/presets/does_not_exist", Some(&token))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"].as_str().unwrap().contains("does_not_exist"),
        "the error should name the missing id"
    );
}

#[tokio::test]
async fn emit_returns_valid_hcl_for_every_preset() {
    let (app, token) = app_with(Tier::Community);
    for id in PRESET_IDS {
        let (status, body) = send(
            &app,
            post(&format!("/presets/{id}/emit"), Some(&token), &answers()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{id}");

        let hcl = body["hcl"].as_str().expect("hcl string");
        assert!(!hcl.is_empty(), "{id} emitted empty HCL");
        // Every preset produces a model, hardware profile, and deployment.
        assert!(hcl.contains("deployment"), "{id} HCL lacks a deployment");
        assert!(hcl.contains("model"), "{id} HCL lacks a model");

        // The emitted HCL round-trips back through the parser to a graph with the
        // expected resource blocks.
        let graph = amicu_hcl::parse_graph(hcl).expect("emitted HCL parses");
        assert_eq!(graph.models.len(), 1, "{id}");
        assert_eq!(graph.deployments.len(), 1, "{id}");
        assert_eq!(graph.hardware_profiles.len(), 1, "{id}");

        // The estimate carried through is the preset's own default estimate.
        assert!(
            body["estimate"]["assumed_prompt_tokens"].is_number(),
            "{id}"
        );
    }
}

#[tokio::test]
async fn emit_rejects_invalid_answers_with_400() {
    let (app, token) = app_with(Tier::Community);
    // A zero concurrency is a well-formed body with an out-of-range value, which
    // `build_graph` rejects as an invalid answer.
    let mut body = answers();
    body["concurrency"] = json!(0);
    let (status, payload) = send(
        &app,
        post("/presets/local_dev_chat/emit", Some(&token), &body),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        payload["error"].as_str().unwrap().contains("concurrency"),
        "the error should name the offending field"
    );
}

#[tokio::test]
async fn emit_unknown_preset_is_404() {
    let (app, token) = app_with(Tier::Community);
    let (status, _) = send(
        &app,
        post("/presets/does_not_exist/emit", Some(&token), &answers()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn community_token_can_call_all_three_endpoints() {
    // The guided wizard is a free-tier feature: a Community token reaches every
    // endpoint (never a 403).
    let (app, token) = app_with(Tier::Community);

    let (list, _) = send(&app, get("/presets", Some(&token))).await;
    assert_eq!(list, StatusCode::OK);
    assert_ne!(list, StatusCode::FORBIDDEN);

    let (detail, _) = send(&app, get("/presets/local_dev_chat", Some(&token))).await;
    assert_eq!(detail, StatusCode::OK);
    assert_ne!(detail, StatusCode::FORBIDDEN);

    let (emit, _) = send(
        &app,
        post("/presets/local_dev_chat/emit", Some(&token), &answers()),
    )
    .await;
    assert_eq!(emit, StatusCode::OK);
    assert_ne!(emit, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn preset_endpoints_reject_unauthenticated_requests() {
    let (app, _token) = app_with(Tier::Community);
    let (list, _) = send(&app, get("/presets", None)).await;
    assert_eq!(list, StatusCode::UNAUTHORIZED);
    let (detail, _) = send(&app, get("/presets/local_dev_chat", None)).await;
    assert_eq!(detail, StatusCode::UNAUTHORIZED);
    let (emit, _) = send(&app, post("/presets/local_dev_chat/emit", None, &answers())).await;
    assert_eq!(emit, StatusCode::UNAUTHORIZED);
}
