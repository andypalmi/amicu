//! Integration tests for the stateless, auth-gated `POST /recommend` endpoint.

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
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";

/// Build the API app under test plus a freshly-minted access token carrying
/// `tier`. `/recommend` admits any authenticated user (min tier `Community`).
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
        Arc::new(LoggingNotifier),
        ISS,
    ));
    let auth_state = AuthApiState::new(
        service,
        Arc::new(RateLimiter::new(100, Duration::from_secs(60))),
    );

    let verifier = Verifier::new(StaticJwksProvider::new(jwks), VerifierConfig::new(ISS, AUD));
    let engine_state = EngineApiState::new(Arc::new(verifier));

    (api_router(auth_state, engine_state), token)
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

/// A measured surface that fires the `under_provisioned` rule (hot queue, hot
/// GPU, KV eviction) plus `preemption_recompute` — both Pro-gated.
fn saturated_metrics() -> Value {
    json!({
        "deployment": "chat_prod",
        "window_seconds": 60,
        "latency": {
            "ttft_ms": { "p50": 90.0, "p90": 150.0, "p99": 220.0 },
            "tpot_ms": { "p50": 12.0, "p90": 18.0, "p99": 25.0 },
            "e2e_ms":  { "p50": 200.0, "p90": 400.0, "p99": 800.0 }
        },
        "throughput": { "output_tokens_per_s": 800.0, "prompt_tokens_per_s": 4000.0, "requests_per_s": 6.0 },
        "concurrency": { "in_flight_requests": 32, "running_batch_size": 16, "max_num_seqs": 16, "seq_saturation": 1.0 },
        "queue": { "depth": 40, "wait_ms": { "p50": 200.0, "p90": 700.0, "p99": 1200.0 }, "admitted": 500, "rejected": 12 },
        "memory": { "kv_cache_utilization": 0.97, "preemptions": 12, "recomputations": 4, "oom_events": 0 },
        "gpu": { "sm_utilization": 0.95, "vram_used": "40GiB", "power_watts": 320.0 },
        "cost": {
            "per_hour": { "amount": 1.5, "currency": "USD" },
            "per_million_tokens": { "amount": 0.5, "currency": "USD" },
            "per_request": { "amount": 0.001, "currency": "USD" },
            "utilization_adjusted_per_hour": { "amount": 1.6, "currency": "USD" }
        },
        "reliability": { "error_rate": 0.01, "timeout_rate": 0.0, "restarts": 0 }
    })
}

fn estimate() -> Value {
    json!({
        "assumed_prompt_tokens": 512,
        "assumed_generation_tokens": 256,
        "assumed_concurrency": 8,
        "expected_ttft_ms": { "low": 80.0, "high": 300.0, "provenance": "public_benchmark", "confidence": "estimated" },
        "expected_tpot_ms": { "low": 10.0, "high": 30.0, "provenance": "public_benchmark", "confidence": "estimated" }
    })
}

fn request_body() -> Value {
    json!({ "metrics": saturated_metrics(), "estimate": estimate() })
}

#[tokio::test]
async fn recommend_pro_returns_full_transparent_report() {
    let (app, token) = app_with(Tier::Pro);
    let (status, body) = send(&app, post("/recommend", Some(&token), &request_body())).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deployment"], "chat_prod");
    let recs = body["recommendations"].as_array().expect("recommendations");
    assert!(!recs.is_empty(), "Pro should see advisory recommendations");

    // The primary (severity-ordered) diagnosis is the saturated stack.
    let primary = &recs[0];
    assert_eq!(primary["bottleneck"], "under_provisioned");
    assert_eq!(primary["required_tier"], "pro");
    assert_eq!(primary["advisory"], true);
    // Full transparency: evidence, rejected alternatives, suggested changes, and
    // a confidence band all cross the wire.
    assert!(!primary["evidence"].as_array().unwrap().is_empty());
    assert!(!primary["rejected"].as_array().unwrap().is_empty());
    assert!(!primary["changes"].as_array().unwrap().is_empty());
    assert_eq!(primary["confidence"], "high");
    let ev = &primary["evidence"][0];
    assert!(ev["metric"].is_string());
    assert!(ev["observed"].is_number());
    assert!(ev["threshold"].is_number());
    assert!(ev["comparison"].is_string());
}

#[tokio::test]
async fn recommend_community_returns_gated_empty_report() {
    // Community unlocks basic monitoring but not the Pro-gated advisory
    // recommendations: a 200 with an empty list, never a 403.
    let (app, token) = app_with(Tier::Community);
    let (status, body) = send(&app, post("/recommend", Some(&token), &request_body())).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deployment"], "chat_prod");
    assert!(
        body["recommendations"].as_array().unwrap().is_empty(),
        "Community must not receive Pro-gated recommendations"
    );
}

#[tokio::test]
async fn recommend_unauthenticated_is_rejected() {
    let (app, _token) = app_with(Tier::Pro);
    let (status, _) = send(&app, post("/recommend", None, &request_body())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn recommend_malformed_body_is_400() {
    // A malformed byte-size string is the client's own bad request.
    let (app, token) = app_with(Tier::Pro);
    let mut body = request_body();
    body["metrics"]["gpu"]["vram_used"] = json!("not-a-byte-size");
    let (status, payload) = send(&app, post("/recommend", Some(&token), &body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(payload["error"].as_str().unwrap().contains("vram_used"));
}
