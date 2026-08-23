//! HTTP surface for the stateless recommendation endpoint: `POST /recommend`.
//!
//! Like `/validate` and `/plan` this is a pure function of its request body. The
//! client submits the measured [`StackMetrics`](amicu_telemetry::StackMetrics)
//! surface plus the [`WorkloadEstimate`](amicu_recommend::WorkloadEstimate) it is
//! judged against (and, optionally, custom rule thresholds); the handler runs the
//! advisory [`diagnose`](amicu_recommend::diagnose) rule engine and returns the
//! [`TuningReport`](amicu_recommend::TuningReport). It reads no server state and
//! applies nothing - recommendations are advisory data only.
//!
//! Access is gated by [`auth_gate`](crate::engine::auth_gate). Tier is honored by
//! *content*, not by rejection: the verified caller's tier is mapped onto the
//! engine's tier ladder and [`TuningReport::for_tier`] withholds the advisory
//! recommendations a Community caller has not unlocked, so a Community token still
//! gets `200` (an empty, monitoring-only report) while Pro+ gets the full depth.
//!
//! The measured-metrics and estimate request DTOs plus the tuning-report response
//! DTOs are shared with `/monitor` and live in [`crate::dto`]; only the
//! recommend-specific [`ThresholdsDto`]/[`RecommendRequest`] wrappers are owned
//! here.

use amicu_auth::Claims;
use amicu_recommend::{Thresholds, WorkloadEstimate, diagnose};
use amicu_telemetry::StackMetrics;
use axum::Json;
use axum::extract::{Extension, State};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::dto::{DtoError, StackMetricsDto, TuningReportDto, WorkloadEstimateDto, map_tier};
use crate::engine::EngineApiState;

// ---------------------------------------------------------------------------
// Request DTOs (measured metrics + estimate + optional thresholds)
// ---------------------------------------------------------------------------

/// Rule trigger points. Every field is optional and independent: provide any
/// subset and the omitted fields fall back to the engine's conservative
/// defaults. Sending `{ "gpu_util_high": 0.9 }` overrides only that one trigger
/// point and leaves the rest at their defaults.
#[derive(Debug, Clone, Copy, Default, Deserialize, ToSchema)]
pub struct ThresholdsDto {
    pub gpu_util_high: Option<f64>,
    pub gpu_util_low: Option<f64>,
    pub queue_wait_p99_ms: Option<f64>,
    pub kv_utilization_high: Option<f64>,
    pub ttft_p90_ms: Option<f64>,
    pub tpot_p90_ms: Option<f64>,
}

impl From<ThresholdsDto> for Thresholds {
    fn from(t: ThresholdsDto) -> Self {
        // Start from the engine defaults and override only the fields the client
        // actually supplied, so a partial body is a per-field override.
        let defaults = Thresholds::default();
        Thresholds {
            gpu_util_high: t.gpu_util_high.unwrap_or(defaults.gpu_util_high),
            gpu_util_low: t.gpu_util_low.unwrap_or(defaults.gpu_util_low),
            queue_wait_p99_ms: t.queue_wait_p99_ms.unwrap_or(defaults.queue_wait_p99_ms),
            kv_utilization_high: t
                .kv_utilization_high
                .unwrap_or(defaults.kv_utilization_high),
            ttft_p90_ms: t.ttft_p90_ms.unwrap_or(defaults.ttft_p90_ms),
            tpot_p90_ms: t.tpot_p90_ms.unwrap_or(defaults.tpot_p90_ms),
        }
    }
}

/// Request body for `POST /recommend`: the measured metrics, the estimate they
/// are judged against, and (optionally) custom rule thresholds.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RecommendRequest {
    /// The measured serving-metric surface for one stack (the same shape as
    /// `GET /stacks/:id/metrics`).
    pub metrics: StackMetricsDto,
    /// The workload estimate the metrics are compared against.
    pub estimate: WorkloadEstimateDto,
    /// Custom rule thresholds. Omit the whole object to use the conservative
    /// engine defaults; provide any subset of fields to override just those,
    /// with the omitted fields still falling back to the engine defaults.
    #[serde(default)]
    pub thresholds: Option<ThresholdsDto>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Diagnose a stack and return advisory recommendations.
///
/// Runs the advisory rule engine over the submitted measured metrics and the
/// workload estimate, then returns the [`TuningReport`] *for the caller's tier*:
/// recommendations the caller has not unlocked are withheld (a Community token
/// gets `200` with an empty list, Pro+ gets the full depth). Stateless and
/// side-effect-free: nothing is applied - recommendations are advisory data.
#[utoipa::path(
    post,
    path = "/recommend",
    tag = "recommend",
    request_body = RecommendRequest,
    responses(
        (status = 200, description = "The tier-appropriate advisory tuning report.", body = TuningReportDto),
        (status = 400, description = "The request body is malformed."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient to reach the endpoint."),
        (status = 422, description = "The JSON body is well-formed but a field has the wrong type or is missing; body is a plain-text decode error."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn recommend_handler(
    State(_state): State<EngineApiState>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<RecommendRequest>,
) -> Result<Json<TuningReportDto>, DtoError> {
    let metrics: StackMetrics = body.metrics.try_into()?;
    let estimate: WorkloadEstimate = body.estimate.into();
    let thresholds = body.thresholds.map(Into::into).unwrap_or_default();

    let report = diagnose(&metrics, &estimate, &thresholds);
    let gated = report.for_tier(map_tier(claims.tier));
    Ok(Json(TuningReportDto::from(gated)))
}
