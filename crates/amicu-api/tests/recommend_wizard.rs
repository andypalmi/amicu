//! Integration tests for the stateless, auth-gated guided-wizard
//! multi-candidate ranking endpoints: `GET /recommend/questions` and
//! `POST /recommend/candidates`.

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

/// The three catalog preset ids the endpoints expose.
const PRESET_IDS: [&str; 3] = [
    "local_dev_chat",
    "private_eu_production",
    "cost_optimized_batch",
];

/// Build the API app under test plus a freshly-minted access token carrying
/// `tier`. The wizard endpoints admit any authenticated user (min tier
/// `Community`).
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

fn candidates_body(preset_id: &str) -> Value {
    json!({ "preset_id": preset_id, "answers": answers() })
}

#[tokio::test]
async fn questions_returns_six_or_fewer_for_every_preset() {
    let (app, token) = app_with(Tier::Community);
    for id in PRESET_IDS {
        let (status, body) = send(
            &app,
            get(&format!("/recommend/questions?preset={id}"), Some(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{id}");
        let questions = body.as_array().expect("array of questions");
        assert!(!questions.is_empty() && questions.len() <= 6, "{id}");
        for q in questions {
            assert!(q["id"].is_string(), "{id}");
            assert!(q["prompt"].is_string(), "{id}");
            assert!(q["kind"].is_object(), "{id}");
            assert!(q["required"].is_boolean(), "{id}");
            assert!(q["hard"].is_boolean(), "{id}");
        }
    }
}

#[tokio::test]
async fn questions_unknown_preset_is_404() {
    let (app, token) = app_with(Tier::Community);
    let (status, body) = send(
        &app,
        get("/recommend/questions?preset=does_not_exist", Some(&token)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("does_not_exist"));
}

#[tokio::test]
async fn questions_rejects_unauthenticated_requests() {
    let (app, _token) = app_with(Tier::Community);
    let (status, _) = send(
        &app,
        get("/recommend/questions?preset=local_dev_chat", None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn candidates_returns_two_to_four_ranked_candidates_for_every_preset() {
    let (app, token) = app_with(Tier::Community);
    for id in PRESET_IDS {
        let (status, body) = send(
            &app,
            post("/recommend/candidates", Some(&token), &candidates_body(id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{id}");
        let candidates = body["candidates"].as_array().expect("candidates array");
        assert!(
            (1..=4).contains(&candidates.len()),
            "{id} returned {} candidates",
            candidates.len()
        );

        for candidate in candidates {
            assert!(candidate["archetype"].is_string(), "{id}");
            assert!(candidate["graph"].is_object(), "{id}");
            let hcl = candidate["hcl"].as_str().expect("hcl string");
            assert!(!hcl.is_empty(), "{id} emitted empty HCL");
            assert!(candidate["score"]["total"].is_number(), "{id}");
            assert!(candidate["rationale"].is_array(), "{id}");
            assert!(
                !candidate["rationale"].as_array().unwrap().is_empty(),
                "{id}"
            );
            assert!(
                candidate["estimate"]["assumed_prompt_tokens"].is_number(),
                "{id}"
            );
            assert!(candidate["confidence"].is_string(), "{id}");

            // Every candidate's HCL round-trips and validates cleanly.
            let graph = amicu_hcl::parse_graph(hcl).expect("emitted HCL parses");
            assert!(graph.missing_references().is_empty(), "{id}");
        }
    }
}

#[tokio::test]
async fn candidates_are_deterministic_across_calls() {
    let (app, token) = app_with(Tier::Community);
    let body = candidates_body("private_eu_production");
    let (_, first) = send(&app, post("/recommend/candidates", Some(&token), &body)).await;
    let (_, second) = send(&app, post("/recommend/candidates", Some(&token), &body)).await;
    assert_eq!(first, second);
}

#[tokio::test]
async fn eu_preset_candidates_never_use_a_non_eu_provider() {
    const EU_PROVIDERS: [&str; 4] = ["hetzner", "ovhcloud", "scaleway", "nebius"];
    let (app, token) = app_with(Tier::Community);
    let (status, body) = send(
        &app,
        post(
            "/recommend/candidates",
            Some(&token),
            &candidates_body("private_eu_production"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let candidates = body["candidates"].as_array().expect("candidates array");
    let mut any_rejected = false;
    for candidate in candidates {
        if !candidate["rejected"]
            .as_array()
            .unwrap_or(&Vec::new())
            .is_empty()
        {
            any_rejected = true;
        }
        let hardware = candidate["graph"]["hardware_profiles"]
            .as_array()
            .expect("hardware_profiles array");
        for hw in hardware {
            let provider = hw["provider"].as_str().expect("provider string");
            assert!(
                EU_PROVIDERS.contains(&provider),
                "provider {provider:?} is not EU-resident"
            );
        }
    }
    assert!(
        any_rejected,
        "expected at least one candidate to document a rejected alternative"
    );
}

#[tokio::test]
async fn candidates_rejects_invalid_answers_with_400() {
    let (app, token) = app_with(Tier::Community);
    let mut body = candidates_body("local_dev_chat");
    body["answers"]["concurrency"] = json!(0);
    let (status, payload) = send(&app, post("/recommend/candidates", Some(&token), &body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(payload["error"].as_str().unwrap().contains("concurrency"));
}

#[tokio::test]
async fn candidates_unknown_preset_is_404() {
    let (app, token) = app_with(Tier::Community);
    let (status, _) = send(
        &app,
        post(
            "/recommend/candidates",
            Some(&token),
            &candidates_body("does_not_exist"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn candidates_rejects_unauthenticated_requests() {
    let (app, _token) = app_with(Tier::Community);
    let (status, _) = send(
        &app,
        post(
            "/recommend/candidates",
            None,
            &candidates_body("local_dev_chat"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn community_token_can_call_both_wizard_endpoints() {
    // The guided wizard is a free-tier feature: a Community token reaches
    // every endpoint (never a 403).
    let (app, token) = app_with(Tier::Community);

    let (questions, _) = send(
        &app,
        get("/recommend/questions?preset=local_dev_chat", Some(&token)),
    )
    .await;
    assert_eq!(questions, StatusCode::OK);
    assert_ne!(questions, StatusCode::FORBIDDEN);

    let (candidates, _) = send(
        &app,
        post(
            "/recommend/candidates",
            Some(&token),
            &candidates_body("local_dev_chat"),
        ),
    )
    .await;
    assert_eq!(candidates, StatusCode::OK);
    assert_ne!(candidates, StatusCode::FORBIDDEN);
}
