//! HTTP surface for the stateless recommendation endpoint: `POST /recommend`.
//!
//! Like `/validate` and `/plan` this is a pure function of its request body. The
//! client submits the measured [`StackMetrics`](amicu_telemetry::StackMetrics)
//! surface plus the [`WorkloadEstimate`](amicu_recommend::WorkloadEstimate) it is
//! judged against (and, optionally, custom rule thresholds); the handler runs the
//! advisory [`diagnose`](amicu_recommend::diagnose) rule engine and returns the
//! [`TuningReport`](amicu_recommend::TuningReport). It reads no server state and
//! applies nothing — recommendations are advisory data only.
//!
//! Access is gated by [`auth_gate`](crate::engine::auth_gate). Tier is honored by
//! *content*, not by rejection: the verified caller's tier is mapped onto the
//! engine's tier ladder and [`TuningReport::for_tier`] withholds the advisory
//! recommendations a Community caller has not unlocked, so a Community token still
//! gets `200` (an empty, monitoring-only report) while Pro+ gets the full depth.
//!
//! The wire contract is a set of thin DTOs owned here, each `#[derive(ToSchema)]`,
//! that convert to/from the engine types. Keeping the boundary in this crate means
//! the engine crates (`amicu-recommend`, `amicu-telemetry`, `amicu-core`) need no
//! OpenAPI dependency and the served JSON shape is pinned locally.

use amicu_auth::{Claims, Tier as AuthTier};
use amicu_core::estimate::{Assumption, Confidence, EstimateRange, Provenance};
use amicu_core::id::DeploymentId;
use amicu_core::resource::Money;
use amicu_core::units::ByteSize;
use amicu_recommend::{
    BottleneckClass, ChangeDirection, Comparison, Evidence, Recommendation, RejectedAlternative,
    SuggestedChange, Thresholds, Tier as RecommendTier, TuningReport, WorkloadEstimate, diagnose,
};
use amicu_telemetry::{
    ConcurrencyMetrics, CostMetrics, Distribution, GpuMetrics, LatencyMetrics, MemoryKvMetrics,
    QueueMetrics, ReliabilityMetrics, StackMetrics, ThroughputMetrics,
};
use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use crate::engine::EngineApiState;

// ---------------------------------------------------------------------------
// Shared enum DTOs
// ---------------------------------------------------------------------------

/// How much an estimate or diagnosis can be relied on. Mirrors
/// [`amicu_core::estimate::Confidence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceDto {
    High,
    Medium,
    Low,
    Estimated,
}

impl From<ConfidenceDto> for Confidence {
    fn from(c: ConfidenceDto) -> Self {
        match c {
            ConfidenceDto::High => Confidence::High,
            ConfidenceDto::Medium => Confidence::Medium,
            ConfidenceDto::Low => Confidence::Low,
            ConfidenceDto::Estimated => Confidence::Estimated,
        }
    }
}

impl From<Confidence> for ConfidenceDto {
    fn from(c: Confidence) -> Self {
        match c {
            Confidence::High => ConfidenceDto::High,
            Confidence::Medium => ConfidenceDto::Medium,
            Confidence::Low => ConfidenceDto::Low,
            Confidence::Estimated => ConfidenceDto::Estimated,
        }
    }
}

/// Where the evidence behind an estimate comes from. Mirrors
/// [`amicu_core::estimate::Provenance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceDto {
    TelemetryAggregate,
    PublicBenchmark,
    ProviderPrice,
    Formula,
}

impl From<ProvenanceDto> for Provenance {
    fn from(p: ProvenanceDto) -> Self {
        match p {
            ProvenanceDto::TelemetryAggregate => Provenance::TelemetryAggregate,
            ProvenanceDto::PublicBenchmark => Provenance::PublicBenchmark,
            ProvenanceDto::ProviderPrice => Provenance::ProviderPrice,
            ProvenanceDto::Formula => Provenance::Formula,
        }
    }
}

/// Product tier that unlocks a recommendation. Mirrors
/// [`amicu_recommend::Tier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TierDto {
    Community,
    Pro,
    Enterprise,
    HostedCloud,
}

impl From<RecommendTier> for TierDto {
    fn from(t: RecommendTier) -> Self {
        match t {
            RecommendTier::Community => TierDto::Community,
            RecommendTier::Pro => TierDto::Pro,
            RecommendTier::Enterprise => TierDto::Enterprise,
            RecommendTier::HostedCloud => TierDto::HostedCloud,
        }
    }
}

/// The class of bottleneck a diagnosis assigns. Mirrors
/// [`amicu_recommend::BottleneckClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BottleneckClassDto {
    UnderProvisioned,
    OverProvisioned,
    PrefillBound,
    DecodeBound,
    PreemptionRecompute,
}

impl From<BottleneckClass> for BottleneckClassDto {
    fn from(b: BottleneckClass) -> Self {
        match b {
            BottleneckClass::UnderProvisioned => BottleneckClassDto::UnderProvisioned,
            BottleneckClass::OverProvisioned => BottleneckClassDto::OverProvisioned,
            BottleneckClass::PrefillBound => BottleneckClassDto::PrefillBound,
            BottleneckClass::DecodeBound => BottleneckClassDto::DecodeBound,
            BottleneckClass::PreemptionRecompute => BottleneckClassDto::PreemptionRecompute,
        }
    }
}

/// Direction of a suggested change. Mirrors
/// [`amicu_recommend::ChangeDirection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeDirectionDto {
    Increase,
    Decrease,
    Enable,
    Replace,
}

impl From<ChangeDirection> for ChangeDirectionDto {
    fn from(d: ChangeDirection) -> Self {
        match d {
            ChangeDirection::Increase => ChangeDirectionDto::Increase,
            ChangeDirection::Decrease => ChangeDirectionDto::Decrease,
            ChangeDirection::Enable => ChangeDirectionDto::Enable,
            ChangeDirection::Replace => ChangeDirectionDto::Replace,
        }
    }
}

/// Whether a measured metric sits above or below its threshold. Mirrors
/// [`amicu_recommend::Comparison`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonDto {
    Above,
    Below,
}

impl From<Comparison> for ComparisonDto {
    fn from(c: Comparison) -> Self {
        match c {
            Comparison::Above => ComparisonDto::Above,
            Comparison::Below => ComparisonDto::Below,
        }
    }
}

// ---------------------------------------------------------------------------
// Request DTOs (measured metrics + estimate + optional thresholds)
// ---------------------------------------------------------------------------

/// A quantity summarized by percentiles (P50/P90/P99).
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct DistributionDto {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
}

impl From<DistributionDto> for Distribution {
    fn from(d: DistributionDto) -> Self {
        Distribution::new(d.p50, d.p90, d.p99)
    }
}

/// Latency surface: TTFT, TPOT, end-to-end (ms), each a percentile distribution.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct LatencyMetricsDto {
    pub ttft_ms: DistributionDto,
    pub tpot_ms: DistributionDto,
    pub e2e_ms: DistributionDto,
}

impl From<LatencyMetricsDto> for LatencyMetrics {
    fn from(l: LatencyMetricsDto) -> Self {
        LatencyMetrics {
            ttft_ms: l.ttft_ms.into(),
            tpot_ms: l.tpot_ms.into(),
            e2e_ms: l.e2e_ms.into(),
        }
    }
}

/// Throughput surface.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct ThroughputMetricsDto {
    pub output_tokens_per_s: f64,
    pub prompt_tokens_per_s: f64,
    pub requests_per_s: f64,
}

impl From<ThroughputMetricsDto> for ThroughputMetrics {
    fn from(t: ThroughputMetricsDto) -> Self {
        ThroughputMetrics {
            output_tokens_per_s: t.output_tokens_per_s,
            prompt_tokens_per_s: t.prompt_tokens_per_s,
            requests_per_s: t.requests_per_s,
        }
    }
}

/// Concurrency & batching surface.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct ConcurrencyMetricsDto {
    pub in_flight_requests: u32,
    pub running_batch_size: u32,
    pub max_num_seqs: u32,
    pub seq_saturation: f64,
}

impl From<ConcurrencyMetricsDto> for ConcurrencyMetrics {
    fn from(c: ConcurrencyMetricsDto) -> Self {
        ConcurrencyMetrics {
            in_flight_requests: c.in_flight_requests,
            running_batch_size: c.running_batch_size,
            max_num_seqs: c.max_num_seqs,
            seq_saturation: c.seq_saturation,
        }
    }
}

/// Admission-queue surface.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct QueueMetricsDto {
    pub depth: u32,
    pub wait_ms: DistributionDto,
    pub admitted: u64,
    pub rejected: u64,
}

impl From<QueueMetricsDto> for QueueMetrics {
    fn from(q: QueueMetricsDto) -> Self {
        QueueMetrics {
            depth: q.depth,
            wait_ms: q.wait_ms.into(),
            admitted: q.admitted,
            rejected: q.rejected,
        }
    }
}

/// Memory / KV-cache surface.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct MemoryKvMetricsDto {
    pub kv_cache_utilization: f64,
    pub preemptions: u64,
    pub recomputations: u64,
    pub oom_events: u64,
}

impl From<MemoryKvMetricsDto> for MemoryKvMetrics {
    fn from(m: MemoryKvMetricsDto) -> Self {
        MemoryKvMetrics {
            kv_cache_utilization: m.kv_cache_utilization,
            preemptions: m.preemptions,
            recomputations: m.recomputations,
            oom_events: m.oom_events,
        }
    }
}

/// GPU surface. `vram_used` is a byte-size string, e.g. `"40GiB"` or `"0B"`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GpuMetricsDto {
    pub sm_utilization: f64,
    #[schema(value_type = String, example = "40GiB")]
    pub vram_used: String,
    pub power_watts: f64,
}

impl TryFrom<GpuMetricsDto> for GpuMetrics {
    type Error = RecommendHttpError;

    fn try_from(g: GpuMetricsDto) -> Result<Self, Self::Error> {
        let vram_used = g
            .vram_used
            .parse::<ByteSize>()
            .map_err(|e| RecommendHttpError::BadInput(format!("gpu.vram_used: {e}")))?;
        Ok(GpuMetrics {
            sm_utilization: g.sm_utilization,
            vram_used,
            power_watts: g.power_watts,
        })
    }
}

/// A monetary amount in a given ISO currency.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MoneyDto {
    pub amount: f64,
    pub currency: String,
}

impl From<MoneyDto> for Money {
    fn from(m: MoneyDto) -> Self {
        Money::new(m.amount, m.currency)
    }
}

/// Cost surface, including utilization-adjusted idle burn.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CostMetricsDto {
    pub per_hour: MoneyDto,
    pub per_million_tokens: MoneyDto,
    pub per_request: MoneyDto,
    pub utilization_adjusted_per_hour: MoneyDto,
}

impl From<CostMetricsDto> for CostMetrics {
    fn from(c: CostMetricsDto) -> Self {
        CostMetrics {
            per_hour: c.per_hour.into(),
            per_million_tokens: c.per_million_tokens.into(),
            per_request: c.per_request.into(),
            utilization_adjusted_per_hour: c.utilization_adjusted_per_hour.into(),
        }
    }
}

/// Reliability surface. Rates are fractions in `[0, 1]`.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct ReliabilityMetricsDto {
    pub error_rate: f64,
    pub timeout_rate: f64,
    pub restarts: u64,
}

impl From<ReliabilityMetricsDto> for ReliabilityMetrics {
    fn from(r: ReliabilityMetricsDto) -> Self {
        ReliabilityMetrics {
            error_rate: r.error_rate,
            timeout_rate: r.timeout_rate,
            restarts: r.restarts,
        }
    }
}

/// The full measured metric surface for one stack over an observation window.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StackMetricsDto {
    pub deployment: String,
    pub window_seconds: u64,
    pub latency: LatencyMetricsDto,
    pub throughput: ThroughputMetricsDto,
    pub concurrency: ConcurrencyMetricsDto,
    pub queue: QueueMetricsDto,
    pub memory: MemoryKvMetricsDto,
    pub gpu: GpuMetricsDto,
    pub cost: CostMetricsDto,
    pub reliability: ReliabilityMetricsDto,
}

impl TryFrom<StackMetricsDto> for StackMetrics {
    type Error = RecommendHttpError;

    fn try_from(m: StackMetricsDto) -> Result<Self, Self::Error> {
        Ok(StackMetrics {
            deployment: DeploymentId::new(m.deployment),
            window_seconds: m.window_seconds,
            latency: m.latency.into(),
            throughput: m.throughput.into(),
            concurrency: m.concurrency.into(),
            queue: m.queue.into(),
            memory: m.memory.into(),
            gpu: m.gpu.try_into()?,
            cost: m.cost.into(),
            reliability: m.reliability.into(),
        })
    }
}

/// An estimate range with the provenance and confidence behind it.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct EstimateRangeDto {
    pub low: f64,
    pub high: f64,
    pub provenance: ProvenanceDto,
    pub confidence: ConfidenceDto,
}

impl From<EstimateRangeDto> for EstimateRange<f64> {
    fn from(r: EstimateRangeDto) -> Self {
        EstimateRange::new(r.low, r.high, r.provenance.into(), r.confidence.into())
    }
}

/// Expected serving behavior and the assumptions behind it.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct WorkloadEstimateDto {
    pub assumed_prompt_tokens: u32,
    pub assumed_generation_tokens: u32,
    pub assumed_concurrency: u32,
    pub expected_ttft_ms: EstimateRangeDto,
    pub expected_tpot_ms: EstimateRangeDto,
}

impl From<WorkloadEstimateDto> for WorkloadEstimate {
    fn from(e: WorkloadEstimateDto) -> Self {
        WorkloadEstimate {
            assumed_prompt_tokens: e.assumed_prompt_tokens,
            assumed_generation_tokens: e.assumed_generation_tokens,
            assumed_concurrency: e.assumed_concurrency,
            expected_ttft_ms: e.expected_ttft_ms.into(),
            expected_tpot_ms: e.expected_tpot_ms.into(),
        }
    }
}

/// Rule trigger points. Any omitted field falls back to the engine default.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct ThresholdsDto {
    pub gpu_util_high: f64,
    pub gpu_util_low: f64,
    pub queue_wait_p99_ms: f64,
    pub kv_utilization_high: f64,
    pub ttft_p90_ms: f64,
    pub tpot_p90_ms: f64,
}

impl From<ThresholdsDto> for Thresholds {
    fn from(t: ThresholdsDto) -> Self {
        Thresholds {
            gpu_util_high: t.gpu_util_high,
            gpu_util_low: t.gpu_util_low,
            queue_wait_p99_ms: t.queue_wait_p99_ms,
            kv_utilization_high: t.kv_utilization_high,
            ttft_p90_ms: t.ttft_p90_ms,
            tpot_p90_ms: t.tpot_p90_ms,
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
    /// Custom rule thresholds. Omit to use conservative defaults.
    #[serde(default)]
    pub thresholds: Option<ThresholdsDto>,
}

// ---------------------------------------------------------------------------
// Response DTOs (the advisory tuning report, with full transparency)
// ---------------------------------------------------------------------------

/// A single stated assumption a diagnosis depends on.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AssumptionDto {
    pub statement: String,
}

impl From<Assumption> for AssumptionDto {
    fn from(a: Assumption) -> Self {
        AssumptionDto {
            statement: a.statement,
        }
    }
}

/// A described (never applied) tuning change.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SuggestedChangeDto {
    pub action: String,
    pub direction: ChangeDirectionDto,
    pub detail: String,
}

impl From<SuggestedChange> for SuggestedChangeDto {
    fn from(c: SuggestedChange) -> Self {
        SuggestedChangeDto {
            action: c.action,
            direction: c.direction.into(),
            detail: c.detail,
        }
    }
}

/// A measured metric that triggered a recommendation, with its threshold.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EvidenceDto {
    pub metric: String,
    pub observed: f64,
    pub threshold: f64,
    pub comparison: ComparisonDto,
}

impl From<Evidence> for EvidenceDto {
    fn from(e: Evidence) -> Self {
        EvidenceDto {
            metric: e.metric,
            observed: e.observed,
            threshold: e.threshold,
            comparison: e.comparison.into(),
        }
    }
}

/// A change the engine considered and ruled out, with why.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RejectedAlternativeDto {
    pub change: String,
    pub reason: String,
}

impl From<RejectedAlternative> for RejectedAlternativeDto {
    fn from(r: RejectedAlternative) -> Self {
        RejectedAlternativeDto {
            change: r.change,
            reason: r.reason,
        }
    }
}

/// An advisory, explainable tuning recommendation. `advisory` is always `true`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RecommendationDto {
    pub bottleneck: BottleneckClassDto,
    pub summary: String,
    pub changes: Vec<SuggestedChangeDto>,
    pub evidence: Vec<EvidenceDto>,
    pub rejected: Vec<RejectedAlternativeDto>,
    pub confidence: ConfidenceDto,
    pub assumptions: Vec<AssumptionDto>,
    pub required_tier: TierDto,
    pub advisory: bool,
}

impl From<Recommendation> for RecommendationDto {
    fn from(r: Recommendation) -> Self {
        RecommendationDto {
            bottleneck: r.bottleneck.into(),
            summary: r.summary,
            changes: r.changes.into_iter().map(Into::into).collect(),
            evidence: r.evidence.into_iter().map(Into::into).collect(),
            rejected: r.rejected.into_iter().map(Into::into).collect(),
            confidence: r.confidence.into(),
            assumptions: r.assumptions.into_iter().map(Into::into).collect(),
            required_tier: r.required_tier.into(),
            advisory: r.advisory,
        }
    }
}

/// A diagnosis for one stack: its advisory recommendations, tier-appropriate.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TuningReportDto {
    pub deployment: String,
    /// Recommendations in severity order; the first is the primary diagnosis.
    /// Empty for a caller whose tier unlocks none of them.
    pub recommendations: Vec<RecommendationDto>,
}

impl From<TuningReport> for TuningReportDto {
    fn from(report: TuningReport) -> Self {
        TuningReportDto {
            deployment: report.deployment.as_str().to_owned(),
            recommendations: report.recommendations.into_iter().map(Into::into).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Error + tier mapping + handler
// ---------------------------------------------------------------------------

/// Error surfaced by the recommend handler. The only fallible input conversion
/// is a malformed byte-size string, which is the client's own bad request.
///
/// Public because the DTO `TryFrom` conversions expose it as their associated
/// error type; it carries no server internals (just an echo of the bad field).
pub enum RecommendHttpError {
    /// A field in the submitted body is malformed (bad request).
    BadInput(String),
}

impl IntoResponse for RecommendHttpError {
    fn into_response(self) -> Response {
        let RecommendHttpError::BadInput(message) = self;
        (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
    }
}

/// Map a verified access tier onto the engine's recommendation tier ladder.
fn map_tier(tier: AuthTier) -> RecommendTier {
    match tier {
        AuthTier::Community => RecommendTier::Community,
        AuthTier::Pro => RecommendTier::Pro,
        AuthTier::Enterprise => RecommendTier::Enterprise,
        AuthTier::Hosted => RecommendTier::HostedCloud,
    }
}

/// Diagnose a stack and return advisory recommendations.
///
/// Runs the advisory rule engine over the submitted measured metrics and the
/// workload estimate, then returns the [`TuningReport`] *for the caller's tier*:
/// recommendations the caller has not unlocked are withheld (a Community token
/// gets `200` with an empty list, Pro+ gets the full depth). Stateless and
/// side-effect-free: nothing is applied — recommendations are advisory data.
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
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn recommend_handler(
    State(_state): State<EngineApiState>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<RecommendRequest>,
) -> Result<Json<TuningReportDto>, RecommendHttpError> {
    let metrics: StackMetrics = body.metrics.try_into()?;
    let estimate: WorkloadEstimate = body.estimate.into();
    let thresholds = body.thresholds.map(Into::into).unwrap_or_default();

    let report = diagnose(&metrics, &estimate, &thresholds);
    let gated = report.for_tier(map_tier(claims.tier));
    Ok(Json(TuningReportDto::from(gated)))
}
