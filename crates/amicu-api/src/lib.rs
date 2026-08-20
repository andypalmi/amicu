//! axum API service.
//!
//! Two surfaces, one crate and one OpenAPI document:
//! - Auth: `.well-known` JWKS/discovery plus sign-up / token / refresh / logout
//!   (see [`auth::auth_router`]).
//! - Engine: the stateless, auth-gated [`POST /validate`](engine::validate_handler)
//!   and [`POST /plan`](engine::plan_handler) endpoints, which run the shared
//!   `amicu-engine` parse → validate → plan orchestration with no side effects.
//! - Recommend: the stateless, auth-gated [`POST /recommend`](recommend::recommend_handler)
//!   endpoint, which runs the advisory `amicu-recommend` diagnosis engine over a
//!   submitted metrics + estimate snapshot and returns a tier-appropriate report.
//! - Monitor: the stateless, auth-gated [`POST /monitor`](monitor::monitor_handler)
//!   endpoint, which normalizes a submitted metrics + estimate snapshot and
//!   returns the monitoring view (metric surface + estimate + advisory
//!   suggestions) the measured-vs-estimated dashboard renders.
//!
//! [`api_router`] assembles both plus a live `GET /openapi.json`. The served
//! spec and the committed `crates/amicu-schema/openapi.json` artifact both come
//! from [`api_openapi`], so they can never drift.
//!
//! Deliberately **not** exposed: `/apply` (state-mutating; executes downloads
//! and launches — needs an execution sandbox) and `/stacks` / `/config`
//! persistence (need server-side multi-tenant state that does not exist yet).

// The suggested `Duration::from_mins`/`from_days` are unstable
// (duration_constructors) on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

pub mod auth;
pub mod billing;
pub mod dto;
pub mod engine;
pub mod monitor;
pub mod openapi;
pub mod org;
pub mod ratelimit;
pub mod recommend;

use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn_with_state;
use axum::routing::get;
use axum::{Json, Router};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub use auth::{AuthApiState, auth_router};
pub use billing::{BillingApiState, StripeConfig, billing_router, stripe_config_from_env};
pub use engine::{AccessVerifier, EngineApiState};
pub use openapi::ApiDoc;
pub use org::{InMemoryOrgStore, OrgApiState, OrgStore, org_router};
pub use ratelimit::{ProxyParseError, RateLimiter, TrustedProxies};

/// Build the engine sub-router (validate/plan, state not yet bound) together
/// with the aggregated OpenAPI document. Both the live spec endpoint and the
/// committed artifact flow through here, so they stay in lock-step.
fn engine_parts() -> (Router<EngineApiState>, utoipa::openapi::OpenApi) {
    OpenApiRouter::with_openapi(openapi::ApiDoc::openapi())
        .routes(routes!(engine::validate_handler))
        .routes(routes!(engine::plan_handler))
        .routes(routes!(recommend::recommend_handler))
        .routes(routes!(monitor::monitor_handler))
        .split_for_parts()
}

/// The aggregated OpenAPI document describing every served route.
///
/// Deterministic across runs (stable ordering), so a regenerate-and-diff test
/// against the committed artifact is a reliable CI gate.
#[must_use]
pub fn api_openapi() -> utoipa::openapi::OpenApi {
    engine_parts().1
}

/// Serialize [`api_openapi`] to the committed-artifact byte form: pretty JSON
/// with a trailing newline. This is the exact content of
/// `crates/amicu-schema/openapi.json`.
#[must_use]
pub fn openapi_json() -> String {
    let mut json = api_openapi()
        .to_pretty_json()
        .expect("OpenAPI document serializes to JSON");
    json.push('\n');
    json
}

/// Assemble the full API router: the unauthenticated auth surface, a live
/// `GET /openapi.json`, the auth-gated engine endpoints, the billing surface
/// (auth-gated checkout/portal/status plus the signature-authenticated webhook),
/// and the Enterprise-gated organization console (`/org/*`: membership, RBAC,
/// audit log).
pub fn api_router(
    auth_state: AuthApiState,
    engine_state: EngineApiState,
    billing_state: BillingApiState,
    org_state: OrgApiState,
) -> Router {
    let (engine_router, spec) = engine_parts();
    let engine_router = engine_router
        // Cap request bodies before extraction (inner), and gate on auth
        // (outer, runs first) so unauthenticated clients are turned away before
        // any body is buffered.
        .layer(DefaultBodyLimit::max(engine::MAX_ENGINE_BODY_BYTES))
        .layer(from_fn_with_state(engine_state.clone(), engine::auth_gate))
        .with_state(engine_state);

    Router::new()
        .route(
            "/openapi.json",
            get(move || {
                let spec = spec.clone();
                async move { Json(spec) }
            }),
        )
        .merge(auth_router(auth_state))
        .merge(engine_router)
        .merge(billing_router(billing_state))
        .merge(org_router(org_state))
}
