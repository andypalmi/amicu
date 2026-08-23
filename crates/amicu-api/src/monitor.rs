//! HTTP surface for the stateless monitoring endpoint: `POST /monitor`.
//!
//! The server holds no live metric collection and no per-stack state (that is
//! future work, TASK-079), so - exactly like `/validate`, `/plan`, and
//! `/recommend` - this endpoint is a pure function of its request body. The
//! client submits the measured [`StackMetrics`](amicu_telemetry::StackMetrics)
//! snapshot plus the [`WorkloadEstimate`](amicu_recommend::WorkloadEstimate) it
//! is judged against; the handler validates and normalizes them and returns the
//! data the monitoring UI renders.
//!
//! The response is a [`MonitorViewDto`]: the server-normalized metric surface
//! echoed back for display (so the frontend renders validated values, not its
//! own re-parse), the normalized estimate with its stated assumptions and
//! confidence, and the engine's advisory [`TuningReport`](amicu_recommend::TuningReport)
//! from [`diagnose`](amicu_recommend::diagnose). No analysis or divergence math
//! is authored here: the metric distributions and the estimate are *displayed*
//! as provided, and the only "suggested changes" are the engine's own advisory
//! (never automatic) output.
//!
//! Access is gated by [`auth_gate`](crate::engine::auth_gate) (min tier
//! Community). Tier is honored by *content*: [`TuningReport::for_tier`] withholds
//! the advisory suggestions a Community caller has not unlocked, so a Community
//! token still gets `200` with the full metric surface and an empty suggestion
//! list, while Pro+ gets the full advisory depth.

use amicu_auth::Claims;
use amicu_recommend::{Thresholds, WorkloadEstimate, diagnose};
use amicu_telemetry::StackMetrics;
use axum::Json;
use axum::extract::{Extension, State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::dto::{DtoError, StackMetricsDto, TuningReportDto, WorkloadEstimateDto, map_tier};
use crate::engine::EngineApiState;

// ---------------------------------------------------------------------------
// Request + response DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /monitor`: the measured metrics snapshot and the
/// workload estimate it is displayed against. Shares the metric + estimate DTOs
/// with `/recommend` (see [`crate::dto`]).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MonitorRequest {
    /// The measured serving-metric surface for one stack (the same shape as
    /// `GET /stacks/:id/metrics`).
    pub metrics: StackMetricsDto,
    /// The workload estimate the measured surface is displayed alongside.
    pub estimate: WorkloadEstimateDto,
}

/// What the monitoring dashboard renders: the server-normalized metric surface,
/// the normalized estimate (with its stated assumptions and confidence), and the
/// engine's advisory - never automatic - suggested changes.
///
/// The measured-vs-estimated comparison is *presentation*: the frontend places
/// the measured distributions next to the estimate's ranges and stated
/// assumptions. This DTO computes no verdict.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MonitorViewDto {
    /// The measured metric surface, normalized server-side and echoed back so
    /// the client renders validated values (e.g. a canonical `vram_used`).
    pub metrics: StackMetricsDto,
    /// The workload estimate, normalized and carrying its stated assumptions and
    /// per-range confidence for honest side-by-side display.
    pub estimate: WorkloadEstimateDto,
    /// The engine's advisory tuning report for the caller's tier: suggested (not
    /// applied) changes with their evidence, rationale, confidence, and rejected
    /// alternatives. Empty for a caller whose tier unlocks none of them.
    pub suggestions: TuningReportDto,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Return the monitoring view for a measured metrics + estimate snapshot.
///
/// Validates and normalizes the submitted surface, runs the advisory
/// [`diagnose`] rule engine, and returns the metric surface, the estimate, and
/// the tier-appropriate advisory suggestions. Stateless and side-effect-free:
/// nothing is collected, stored, or applied.
#[utoipa::path(
    post,
    path = "/monitor",
    tag = "monitor",
    request_body = MonitorRequest,
    responses(
        (status = 200, description = "The normalized metric surface, estimate, and tier-appropriate advisory suggestions.", body = MonitorViewDto),
        (status = 400, description = "The request body is malformed."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient to reach the endpoint."),
        (status = 422, description = "The JSON body is well-formed but a field has the wrong type or is missing; body is a plain-text decode error."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn monitor_handler(
    State(_state): State<EngineApiState>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<MonitorRequest>,
) -> Result<Json<MonitorViewDto>, DtoError> {
    let metrics: StackMetrics = body.metrics.try_into()?;
    let estimate: WorkloadEstimate = body.estimate.into();

    // Monitoring displays the engine's advisory suggestions computed at the
    // conservative default thresholds; it authors no divergence math itself.
    let report = diagnose(&metrics, &estimate, &Thresholds::default());
    let gated = report.for_tier(map_tier(claims.tier));

    Ok(Json(MonitorViewDto {
        metrics: StackMetricsDto::from(metrics),
        estimate: WorkloadEstimateDto::from(estimate),
        suggestions: TuningReportDto::from(gated),
    }))
}
