//! HTTP surface for the stateless engine endpoints: `POST /validate` and
//! `POST /plan`.
//!
//! Both are pure functions of their request body - they parse the submitted
//! config, run the shared [`amicu_engine`] parse → validate → plan orchestration,
//! and return the JSON contract. They read no server state, touch no filesystem,
//! resolve no secrets, and launch nothing. Applying a plan (which *does* execute
//! downloads/launches) is deliberately not exposed here; see the crate docs.
//!
//! Access is gated by [`auth_gate`]: a valid first-party access JWT (verified
//! offline against the issuer JWKS) whose tier claim meets the configured
//! minimum. The verified [`Claims`] are stashed in the request extensions.

use std::sync::Arc;

use amicu_auth::verifier::{JwksProvider, MonoClock, Verifier};
use amicu_auth::{AuthError, Claims, Role, Tier, WorkspaceId};
use amicu_core::diagnostic::DiagnosticReport;
use amicu_hcl::HclError;
use amicu_plan::Plan;
use async_trait::async_trait;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

/// Offline access-token verification seam.
///
/// Boxed behind a trait so [`EngineApiState`] stays non-generic while any
/// concrete [`Verifier`] (over any JWKS provider / clock) can back it.
#[async_trait]
pub trait AccessVerifier: Send + Sync {
    /// Verify a bearer access token and return its claims.
    ///
    /// # Errors
    /// Returns [`AuthError`] when the token is missing a key, malformed,
    /// expired, or otherwise fails verification.
    async fn verify(&self, token: &str) -> Result<Claims, AuthError>;
}

#[async_trait]
impl<P: JwksProvider, C: MonoClock> AccessVerifier for Verifier<P, C> {
    async fn verify(&self, token: &str) -> Result<Claims, AuthError> {
        Verifier::verify(self, token).await
    }
}

/// Shared state for the engine endpoints: the token verifier plus the minimum
/// access tier required to call them.
#[derive(Clone)]
pub struct EngineApiState {
    verifier: Arc<dyn AccessVerifier>,
    min_tier: Tier,
}

impl EngineApiState {
    /// State that admits any authenticated user (minimum tier `Community`).
    #[must_use]
    pub fn new(verifier: Arc<dyn AccessVerifier>) -> Self {
        Self {
            verifier,
            min_tier: Tier::Community,
        }
    }

    /// Require at least `min_tier` to reach the engine endpoints.
    #[must_use]
    pub fn with_min_tier(mut self, min_tier: Tier) -> Self {
        self.min_tier = min_tier;
        self
    }
}

/// The verified per-request tenancy context, derived ENTIRELY from the access
/// token's claims - never from a request header
/// (andypalmi/amicu-planning#130). Every auth gate stashes one of these in the
/// request extensions after verifying the token, so a handler always scopes its
/// work to the workspace the token was minted for (`claims.wsid`), acting as the
/// subject (`claims.sub`) with the role and tier the claim carries. The client
/// cannot assert any of it: the issuer resolved `wsid`/`wsrole`/`tier`
/// server-side from `workspace_members` at mint time and re-validates them on
/// every refresh rotation.
#[derive(Debug, Clone)]
pub struct WorkspaceContext {
    /// The subject (token `sub`) acting in this request.
    pub user_id: String,
    /// The active workspace this token is scoped to (`claims.wsid`).
    pub workspace: WorkspaceId,
    /// The subject's role in [`workspace`](Self::workspace) (`claims.wsrole`).
    pub role: Role,
    /// The active workspace's billing tier (`claims.tier`).
    pub tier: Tier,
}

impl WorkspaceContext {
    /// Derive the tenancy context from verified claims.
    #[must_use]
    pub fn from_claims(claims: &Claims) -> Self {
        Self {
            user_id: claims.sub.clone(),
            workspace: claims.wsid,
            role: claims.wsrole,
            tier: claims.tier,
        }
    }
}

/// Ordinal ranking of tiers for the "at least" gate.
fn tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::Community => 0,
        Tier::Pro => 1,
        Tier::Enterprise => 2,
        Tier::Hosted => 3,
    }
}

/// Extract a `Bearer` token from the `Authorization` header.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Auth middleware for the engine endpoints.
///
/// Rejects requests without a valid access token (`401`) and authenticated
/// requests whose tier is below the configured minimum (`403`). On success the
/// verified [`Claims`] are inserted into the request extensions.
pub async fn auth_gate(
    State(state): State<EngineApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_token(request.headers()) else {
        return unauthorized("missing or malformed bearer token");
    };
    match state.verifier.verify(token).await {
        Ok(claims) if tier_rank(claims.tier) >= tier_rank(state.min_tier) => {
            // The verified claim is the sole source of tenancy: stash both the
            // full claims and the derived WorkspaceContext so config handlers
            // scope to claims.wsid without ever reading a header.
            request
                .extensions_mut()
                .insert(WorkspaceContext::from_claims(&claims));
            request.extensions_mut().insert(claims);
            next.run(request).await
        }
        Ok(_) => forbidden("access tier is insufficient for this endpoint"),
        Err(_) => unauthorized("invalid or expired access token"),
    }
}

fn unauthorized(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
}

fn forbidden(message: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": message }))).into_response()
}

/// Maximum accepted request-body size for the engine endpoints (256 KiB).
///
/// An Amicu HCL config comfortably fits well under this; the explicit cap
/// bounds parser cost and rejects oversized payloads (`413`) before they are
/// buffered, rather than relying on axum's larger implicit default.
pub const MAX_ENGINE_BODY_BYTES: usize = 256 * 1024;

/// Request body for `POST /validate`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ValidateRequest {
    /// The Amicu HCL configuration text to validate.
    pub config: String,
}

/// Request body for `POST /plan`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PlanRequest {
    /// The Amicu HCL configuration text to plan.
    pub config: String,
    /// Optional prior-state config, as HCL, to diff against. When omitted the
    /// plan is computed against an empty prior (everything is a create).
    #[serde(default)]
    pub prior: Option<String>,
}

/// Error surfaced by the engine handlers.
///
/// A runtime adapter failure while planning is *not* a variant here
/// (amicu-planning#150 review hardening): [`amicu_engine::build_plan`] folds
/// any such failure into the [`amicu_engine::PlanError`] diagnostic report
/// instead, which `plan_handler` handles directly as a `422` - there is no
/// remaining path that needs to turn a bare `AdapterError` into a `500`.
pub(crate) enum EngineHttpError {
    /// The submitted config text is not valid HCL (bad request).
    Parse(HclError),
}

impl From<HclError> for EngineHttpError {
    fn from(e: HclError) -> Self {
        Self::Parse(e)
    }
}

impl IntoResponse for EngineHttpError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            // A parse failure is the client's *own* malformed HCL: echoing the
            // syntax detail back is safe and useful, and reveals nothing about
            // the server internals.
            EngineHttpError::Parse(e) => (StatusCode::BAD_REQUEST, e.to_string()),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// Validate a submitted configuration.
///
/// Parses the HCL and runs structural + per-deployment validation, returning the
/// [`DiagnosticReport`] contract. A config that parses but fails validation
/// still returns `200` with `valid: false`; only a parse error is a `400`.
#[utoipa::path(
    post,
    path = "/validate",
    tag = "engine",
    request_body = ValidateRequest,
    responses(
        (status = 200, description = "Diagnostic report for the submitted config (may contain errors).", body = DiagnosticReport),
        (status = 400, description = "The config text is not valid Amicu HCL."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn validate_handler(
    State(_state): State<EngineApiState>,
    Json(body): Json<ValidateRequest>,
) -> Result<Json<DiagnosticReport>, EngineHttpError> {
    let graph = amicu_engine::parse_config(&body.config)?;
    Ok(Json(amicu_engine::report_for(&graph)))
}

/// Plan a submitted configuration.
///
/// Parses the HCL (and optional prior-state HCL), surfaces structural errors
/// through the same diagnostic path as `/validate`, then diffs into the [`Plan`]
/// contract. When the config cannot be planned (structural errors, or
/// adapter-level validation errors) the response is `422` carrying the blocking
/// [`DiagnosticReport`]. Stateless and side-effect-free: no plan is applied.
#[utoipa::path(
    post,
    path = "/plan",
    tag = "engine",
    request_body = PlanRequest,
    responses(
        (status = 200, description = "The computed plan.", body = Plan),
        (status = 400, description = "The config (or prior) text is not valid Amicu HCL."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 422, description = "The config is invalid and cannot be planned; body is a DiagnosticReport.", body = DiagnosticReport),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn plan_handler(
    State(_state): State<EngineApiState>,
    Json(body): Json<PlanRequest>,
) -> Result<Response, EngineHttpError> {
    let graph = amicu_engine::parse_config(&body.config)?;
    let prior = match body.prior.as_deref() {
        Some(text) => Some(amicu_engine::parse_config(text)?),
        None => None,
    };

    // Structural errors (dangling refs, duplicate ids) block a plan, mirroring
    // the CLI: surface them before `build_plan` can hard-error.
    let report = amicu_engine::report_for(&graph);
    if !report.valid {
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, Json(report)).into_response());
    }

    let result = match amicu_engine::build_plan(&graph, prior.as_ref()) {
        Ok(result) => result,
        // A deployment failed to plan (e.g. a CAP-002/CAP-003 extra_args/env
        // conflict); `build_plan` hands back every diagnostic it collected
        // instead of a bare adapter error, so this stays a structured `422`
        // rather than falling through to `EngineHttpError::Plan`'s opaque
        // `500` (amicu-planning#150 review hardening).
        Err(amicu_engine::PlanError(report)) => {
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, Json(report)).into_response());
        }
    };
    if result.has_errors() {
        let report = DiagnosticReport::new(result.diagnostics.clone());
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, Json(report)).into_response());
    }

    Ok(Json(result.plan).into_response())
}
