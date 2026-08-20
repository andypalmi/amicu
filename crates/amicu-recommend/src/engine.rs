//! The rule engine: measured metrics + estimate → advisory recommendations.
//!
//! [`diagnose`] runs the starter rules over a measured
//! [`StackMetrics`](amicu_telemetry::StackMetrics) and a [`WorkloadEstimate`],
//! emitting a [`TuningReport`] of advisory [`Recommendation`]s. Rules are pure
//! and deterministic; each attaches the [`Evidence`] that triggered it, the
//! [`RejectedAlternative`]s it ruled out, and the estimate's assumptions plus
//! any measured-vs-estimated divergence. Nothing here mutates or applies a
//! stack.

use amicu_core::estimate::Confidence;
use amicu_core::id::DeploymentId;
use amicu_telemetry::StackMetrics;
use serde::{Deserialize, Serialize};

use crate::bottleneck::BottleneckClass;
use crate::estimate::WorkloadEstimate;
use crate::recommendation::{
    ChangeDirection, Evidence, Recommendation, RejectedAlternative, SuggestedChange,
};
use crate::tier::Tier;

/// Trigger points for the rules. Defaults are conservative starting values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    /// GPU SM utilization considered saturated (fraction).
    pub gpu_util_high: f64,
    /// GPU SM utilization considered idle (fraction).
    pub gpu_util_low: f64,
    /// P99 queue wait considered excessive (ms).
    pub queue_wait_p99_ms: f64,
    /// KV-cache utilization considered pressured (fraction).
    pub kv_utilization_high: f64,
    /// P90 TTFT considered high (ms).
    pub ttft_p90_ms: f64,
    /// P90 TPOT considered high (ms).
    pub tpot_p90_ms: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            gpu_util_high: 0.85,
            gpu_util_low: 0.35,
            queue_wait_p99_ms: 500.0,
            kv_utilization_high: 0.90,
            ttft_p90_ms: 500.0,
            tpot_p90_ms: 40.0,
        }
    }
}

/// A diagnosis for one stack: its advisory recommendations.
///
/// Monitoring the underlying metric surface is [`Tier::MONITORING`]; the
/// recommendations here are gated at each [`Recommendation::required_tier`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningReport {
    pub deployment: DeploymentId,
    /// Recommendations in severity order; the first is the primary diagnosis.
    pub recommendations: Vec<Recommendation>,
}

impl TuningReport {
    /// The tier required to view the metric surface these recommendations rest
    /// on. Basic monitoring is always [`Tier::Community`].
    #[must_use]
    pub const fn monitoring_tier() -> Tier {
        Tier::MONITORING
    }

    /// The primary (most severe) diagnosis, if any rule fired.
    #[must_use]
    pub fn primary(&self) -> Option<&Recommendation> {
        self.recommendations.first()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.recommendations.is_empty()
    }

    /// The report as seen by a holder of `tier`: recommendations the tier does
    /// not unlock are withheld (never a partial or auto-applied change).
    #[must_use]
    pub fn for_tier(&self, tier: Tier) -> TuningReport {
        TuningReport {
            deployment: self.deployment.clone(),
            recommendations: self
                .recommendations
                .iter()
                .filter(|r| tier.unlocks(r.required_tier))
                .cloned()
                .collect(),
        }
    }

    /// Invariant check: every recommendation is advisory.
    #[must_use]
    pub fn all_advisory(&self) -> bool {
        self.recommendations.iter().all(|r| r.advisory)
    }
}

/// Diagnose a stack, emitting advisory recommendations in severity order.
#[must_use]
pub fn diagnose(
    metrics: &StackMetrics,
    estimate: &WorkloadEstimate,
    thresholds: &Thresholds,
) -> TuningReport {
    let rules = [
        under_provisioned,
        decode_bound,
        prefill_bound,
        preemption_recompute,
        over_provisioned,
    ];
    let recommendations = rules
        .into_iter()
        .filter_map(|rule| rule(metrics, estimate, thresholds))
        .collect();
    TuningReport {
        deployment: metrics.deployment.clone(),
        recommendations,
    }
}

fn under_provisioned(
    m: &StackMetrics,
    e: &WorkloadEstimate,
    t: &Thresholds,
) -> Option<Recommendation> {
    let queue_hot = m.queue.wait_ms.p99 > t.queue_wait_p99_ms;
    let gpu_hot = m.gpu.sm_utilization > t.gpu_util_high;
    let preempting = m.memory.preemptions > 0;
    let kv_full = m.memory.kv_cache_utilization > t.kv_utilization_high;
    if !(queue_hot && gpu_hot && (preempting || kv_full)) {
        return None;
    }
    // Attribute eviction to whichever signal fired.
    #[allow(clippy::cast_precision_loss)]
    let eviction = if preempting {
        Evidence::above("memory.preemptions", m.memory.preemptions as f64, 0.0)
    } else {
        Evidence::above(
            "memory.kv_cache_utilization",
            m.memory.kv_cache_utilization,
            t.kv_utilization_high,
        )
    };
    Some(
        Recommendation::new(
            BottleneckClass::UnderProvisioned,
            "Saturated: requests queue while the GPU is busy and the KV cache evicts.",
            Confidence::High,
            Tier::RECOMMENDATIONS,
        )
        .with_evidence(Evidence::above(
            "queue.wait_ms.p99",
            m.queue.wait_ms.p99,
            t.queue_wait_p99_ms,
        ))
        .with_evidence(Evidence::above(
            "gpu.sm_utilization",
            m.gpu.sm_utilization,
            t.gpu_util_high,
        ))
        .with_evidence(eviction)
        .with_change(SuggestedChange::new(
            "replicas",
            ChangeDirection::Increase,
            "scale out to shed queue depth",
        ))
        .with_change(SuggestedChange::new(
            "gpu",
            ChangeDirection::Replace,
            "move to a larger-VRAM GPU",
        ))
        .with_change(SuggestedChange::new(
            "kv-cache",
            ChangeDirection::Increase,
            "raise KV-cache allocation",
        ))
        .with_rejected(RejectedAlternative::new(
            "downsize / spot",
            "GPU is already saturated; shrinking capacity worsens queueing",
        ))
        .with_assumptions(e.assumptions()),
    )
}

fn over_provisioned(
    m: &StackMetrics,
    e: &WorkloadEstimate,
    t: &Thresholds,
) -> Option<Recommendation> {
    let gpu_idle = m.gpu.sm_utilization < t.gpu_util_low;
    let paying = m.cost.per_hour.amount > 0.0;
    let not_queueing = m.queue.wait_ms.p99 <= t.queue_wait_p99_ms;
    if !(gpu_idle && paying && not_queueing) {
        return None;
    }
    Some(
        Recommendation::new(
            BottleneckClass::OverProvisioned,
            "Idle burn: the GPU sits mostly idle while the stack bills.",
            Confidence::Medium,
            Tier::RECOMMENDATIONS,
        )
        .with_evidence(Evidence::below(
            "gpu.sm_utilization",
            m.gpu.sm_utilization,
            t.gpu_util_low,
        ))
        .with_change(SuggestedChange::new(
            "gpu",
            ChangeDirection::Replace,
            "downsize to a smaller/cheaper GPU",
        ))
        .with_change(SuggestedChange::new(
            "spot",
            ChangeDirection::Replace,
            "move to interruptible/spot capacity",
        ))
        .with_change(SuggestedChange::new(
            "replicas",
            ChangeDirection::Decrease,
            "reduce replica count",
        ))
        .with_rejected(RejectedAlternative::new(
            "scale out",
            "GPU is idle; adding capacity increases idle burn",
        ))
        .with_assumptions(e.assumptions()),
    )
}

fn prefill_bound(m: &StackMetrics, e: &WorkloadEstimate, t: &Thresholds) -> Option<Recommendation> {
    let ttft_hot = m.latency.ttft_ms.p90 > t.ttft_p90_ms;
    let tpot_ok = m.latency.tpot_ms.p90 <= t.tpot_p90_ms;
    if !(ttft_hot && tpot_ok) {
        return None;
    }
    let mut rec = Recommendation::new(
        BottleneckClass::PrefillBound,
        "Prefill-bound: first-token latency is high while per-token latency is healthy.",
        Confidence::Medium,
        Tier::RECOMMENDATIONS,
    )
    .with_evidence(Evidence::above(
        "latency.ttft_ms.p90",
        m.latency.ttft_ms.p90,
        t.ttft_p90_ms,
    ))
    .with_evidence(Evidence::below(
        "latency.tpot_ms.p90",
        m.latency.tpot_ms.p90,
        t.tpot_p90_ms,
    ))
    .with_change(SuggestedChange::new(
        "chunked-prefill",
        ChangeDirection::Enable,
        "interleave prefill with decode",
    ))
    .with_change(SuggestedChange::new(
        "prompt-concurrency",
        ChangeDirection::Decrease,
        "cap concurrent prompt processing",
    ))
    .with_rejected(RejectedAlternative::new(
        "smaller / quantized model",
        "TPOT is healthy; decode is not the bottleneck",
    ))
    .with_assumptions(e.assumptions());
    if let Some(div) = e.ttft_divergence(m.latency.ttft_ms.p90) {
        rec = rec.with_assumption(div);
    }
    Some(rec)
}

fn decode_bound(m: &StackMetrics, e: &WorkloadEstimate, t: &Thresholds) -> Option<Recommendation> {
    if m.latency.tpot_ms.p90 <= t.tpot_p90_ms {
        return None;
    }
    let mut rec = Recommendation::new(
        BottleneckClass::DecodeBound,
        "Decode-bound: per-token generation latency is high.",
        Confidence::Medium,
        Tier::RECOMMENDATIONS,
    )
    .with_evidence(Evidence::above(
        "latency.tpot_ms.p90",
        m.latency.tpot_ms.p90,
        t.tpot_p90_ms,
    ))
    .with_change(SuggestedChange::new(
        "model",
        ChangeDirection::Replace,
        "smaller or quantized model",
    ))
    .with_change(SuggestedChange::new(
        "gpu",
        ChangeDirection::Replace,
        "higher-memory-bandwidth GPU",
    ))
    .with_change(SuggestedChange::new(
        "tensor-parallel",
        ChangeDirection::Increase,
        "shard decode across GPUs",
    ))
    .with_rejected(RejectedAlternative::new(
        "chunked prefill",
        "prefill is not the bottleneck; this does not speed decode",
    ))
    .with_assumptions(e.assumptions());
    if let Some(div) = e.tpot_divergence(m.latency.tpot_ms.p90) {
        rec = rec.with_assumption(div);
    }
    Some(rec)
}

fn preemption_recompute(
    m: &StackMetrics,
    e: &WorkloadEstimate,
    _t: &Thresholds,
) -> Option<Recommendation> {
    if m.memory.preemptions == 0 && m.memory.recomputations == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let preemptions = m.memory.preemptions as f64;
    Some(
        Recommendation::new(
            BottleneckClass::PreemptionRecompute,
            "KV pressure is forcing preemption/recompute.",
            Confidence::Medium,
            Tier::RECOMMENDATIONS,
        )
        .with_evidence(Evidence::above("memory.preemptions", preemptions, 0.0))
        .with_change(SuggestedChange::new(
            "gpu-memory-utilization",
            ChangeDirection::Increase,
            "give the KV cache more of VRAM",
        ))
        .with_change(SuggestedChange::new(
            "vram-headroom",
            ChangeDirection::Increase,
            "add VRAM headroom (larger GPU)",
        ))
        .with_rejected(RejectedAlternative::new(
            "raise max-num-seqs",
            "more concurrent sequences increases KV pressure and preemption",
        ))
        .with_assumptions(e.assumptions()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::estimate::{EstimateRange, Provenance};
    use amicu_telemetry::Distribution;

    fn estimate() -> WorkloadEstimate {
        WorkloadEstimate {
            assumed_prompt_tokens: 512,
            assumed_generation_tokens: 256,
            assumed_concurrency: 8,
            expected_ttft_ms: EstimateRange::new(
                80.0,
                300.0,
                Provenance::PublicBenchmark,
                Confidence::Estimated,
            ),
            expected_tpot_ms: EstimateRange::new(
                10.0,
                30.0,
                Provenance::PublicBenchmark,
                Confidence::Estimated,
            ),
        }
    }

    fn healthy() -> StackMetrics {
        let mut m = StackMetrics::empty(DeploymentId::new("chat_prod"), 60);
        m.gpu.sm_utilization = 0.6;
        m.latency.ttft_ms = Distribution::new(90.0, 150.0, 220.0);
        m.latency.tpot_ms = Distribution::new(12.0, 18.0, 25.0);
        m.queue.wait_ms = Distribution::new(5.0, 20.0, 60.0);
        m.cost.per_hour = amicu_core::resource::Money::new(1.5, "USD");
        m
    }

    #[test]
    fn healthy_stack_yields_no_recommendations() {
        let report = diagnose(&healthy(), &estimate(), &Thresholds::default());
        assert!(report.is_empty());
        assert!(report.primary().is_none());
    }

    #[test]
    fn under_provisioned_fires_on_queue_gpu_and_eviction() {
        let mut m = healthy();
        m.queue.wait_ms = Distribution::new(200.0, 700.0, 1200.0);
        m.gpu.sm_utilization = 0.95;
        m.memory.preemptions = 12;
        let report = diagnose(&m, &estimate(), &Thresholds::default());
        let primary = report.primary().unwrap();
        assert_eq!(primary.bottleneck, BottleneckClass::UnderProvisioned);
        assert_eq!(primary.confidence, Confidence::High);
        // Every triggering piece of evidence actually holds.
        assert!(primary.evidence.iter().all(Evidence::holds));
        assert!(!primary.rejected.is_empty());
    }

    #[test]
    fn over_provisioned_fires_on_idle_gpu_with_cost() {
        let mut m = healthy();
        m.gpu.sm_utilization = 0.1;
        let report = diagnose(&m, &estimate(), &Thresholds::default());
        assert!(
            report
                .recommendations
                .iter()
                .any(|r| r.bottleneck == BottleneckClass::OverProvisioned)
        );
    }

    #[test]
    fn prefill_bound_fires_on_high_ttft_healthy_tpot() {
        let mut m = healthy();
        m.latency.ttft_ms = Distribution::new(300.0, 900.0, 1400.0);
        let report = diagnose(&m, &estimate(), &Thresholds::default());
        let rec = report
            .recommendations
            .iter()
            .find(|r| r.bottleneck == BottleneckClass::PrefillBound)
            .unwrap();
        // Divergence from the estimate is recorded as an assumption.
        assert!(rec.assumptions.iter().any(|a| a.statement.contains("TTFT")));
        assert!(
            report
                .recommendations
                .iter()
                .all(|r| r.bottleneck != BottleneckClass::DecodeBound)
        );
    }

    #[test]
    fn decode_bound_fires_on_high_tpot() {
        let mut m = healthy();
        m.latency.tpot_ms = Distribution::new(50.0, 80.0, 120.0);
        let report = diagnose(&m, &estimate(), &Thresholds::default());
        let rec = report
            .recommendations
            .iter()
            .find(|r| r.bottleneck == BottleneckClass::DecodeBound)
            .unwrap();
        assert!(rec.assumptions.iter().any(|a| a.statement.contains("TPOT")));
    }

    #[test]
    fn preemption_recompute_fires_and_rejects_more_seqs() {
        let mut m = healthy();
        m.memory.recomputations = 4;
        let report = diagnose(&m, &estimate(), &Thresholds::default());
        let rec = report
            .recommendations
            .iter()
            .find(|r| r.bottleneck == BottleneckClass::PreemptionRecompute)
            .unwrap();
        assert!(
            rec.rejected
                .iter()
                .any(|a| a.change.contains("max-num-seqs"))
        );
    }

    #[test]
    fn all_recommendations_are_advisory_and_pro_gated() {
        let mut m = healthy();
        m.queue.wait_ms = Distribution::new(200.0, 700.0, 1200.0);
        m.gpu.sm_utilization = 0.95;
        m.memory.preemptions = 12;
        m.latency.tpot_ms = Distribution::new(50.0, 80.0, 120.0);
        let report = diagnose(&m, &estimate(), &Thresholds::default());
        assert!(!report.is_empty());
        assert!(report.all_advisory());
        assert!(
            report
                .recommendations
                .iter()
                .all(|r| r.required_tier == Tier::Pro)
        );
    }

    #[test]
    fn tier_gate_withholds_recommendations_below_pro() {
        let mut m = healthy();
        m.latency.tpot_ms = Distribution::new(50.0, 80.0, 120.0);
        let report = diagnose(&m, &estimate(), &Thresholds::default());
        assert!(!report.is_empty());
        // Community sees monitoring but no advisory recommendations.
        assert!(report.for_tier(Tier::Community).is_empty());
        assert!(!report.for_tier(Tier::Pro).is_empty());
        assert_eq!(TuningReport::monitoring_tier(), Tier::Community);
    }
}
