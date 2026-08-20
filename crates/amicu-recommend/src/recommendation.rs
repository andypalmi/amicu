//! The advisory recommendation contract.
//!
//! A [`Recommendation`] is **advisory and never auto-applied**: it describes a
//! bottleneck and the changes an operator might make, but carries no executable
//! directive and there is no `apply` path in this crate. Every recommendation
//! states its [`Evidence`], the [`RejectedAlternative`]s it ruled out, and the
//! [`Confidence`] and [`Assumption`]s behind it (reused from `amicu-core` so
//! divergence from an estimate is attributable).

use amicu_core::estimate::{Assumption, Confidence};
use serde::{Deserialize, Serialize};

use crate::bottleneck::BottleneckClass;
use crate::tier::Tier;

/// Direction of a suggested change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeDirection {
    Increase,
    Decrease,
    Enable,
    Replace,
}

/// A described (never applied) tuning change: a knob or action, a direction,
/// and a human-readable detail. Descriptive text only — nothing here is
/// executable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuggestedChange {
    /// The knob or action, e.g. `"replicas"` or `"chunked-prefill"`.
    pub action: String,
    pub direction: ChangeDirection,
    pub detail: String,
}

impl SuggestedChange {
    #[must_use]
    pub fn new(
        action: impl Into<String>,
        direction: ChangeDirection,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            action: action.into(),
            direction,
            detail: detail.into(),
        }
    }
}

/// Whether a measured metric sits above or below its trigger threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Above,
    Below,
}

/// A measured metric that triggered a recommendation, with its threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// Dotted path into the metric surface, e.g. `"queue.wait_ms.p99"`.
    pub metric: String,
    pub observed: f64,
    pub threshold: f64,
    pub comparison: Comparison,
}

impl Evidence {
    #[must_use]
    pub fn above(metric: impl Into<String>, observed: f64, threshold: f64) -> Self {
        Self {
            metric: metric.into(),
            observed,
            threshold,
            comparison: Comparison::Above,
        }
    }

    #[must_use]
    pub fn below(metric: impl Into<String>, observed: f64, threshold: f64) -> Self {
        Self {
            metric: metric.into(),
            observed,
            threshold,
            comparison: Comparison::Below,
        }
    }

    /// Whether the observed value actually satisfies the stated comparison.
    #[must_use]
    pub fn holds(&self) -> bool {
        match self.comparison {
            Comparison::Above => self.observed > self.threshold,
            Comparison::Below => self.observed < self.threshold,
        }
    }
}

/// A change the engine considered and ruled out, with why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedAlternative {
    pub change: String,
    pub reason: String,
}

impl RejectedAlternative {
    #[must_use]
    pub fn new(change: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            change: change.into(),
            reason: reason.into(),
        }
    }
}

/// An advisory, explainable tuning recommendation.
///
/// # Advisory invariant
/// A recommendation is descriptive data only: the guarantee that it is never
/// auto-applied is structural — no method on this type, and no function in this
/// crate, applies it to a running stack. The [`advisory`](Self::advisory) flag
/// is set `true` by every constructor and reported to clients; the
/// `advisory_is_always_true` test pins the constructed value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub bottleneck: BottleneckClass,
    pub summary: String,
    pub changes: Vec<SuggestedChange>,
    /// Measured metrics that triggered this recommendation.
    pub evidence: Vec<Evidence>,
    /// Alternatives considered and rejected.
    pub rejected: Vec<RejectedAlternative>,
    pub confidence: Confidence,
    /// Assumptions behind the diagnosis, including any measured-vs-estimated
    /// divergence, so a wrong call is attributable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<Assumption>,
    /// Tier that unlocks this recommendation.
    pub required_tier: Tier,
    /// Always `true`: recommendations are never auto-applied.
    pub advisory: bool,
}

impl Recommendation {
    /// A recommendation for `bottleneck`. Always advisory.
    #[must_use]
    pub fn new(
        bottleneck: BottleneckClass,
        summary: impl Into<String>,
        confidence: Confidence,
        required_tier: Tier,
    ) -> Self {
        Self {
            bottleneck,
            summary: summary.into(),
            changes: Vec::new(),
            evidence: Vec::new(),
            rejected: Vec::new(),
            confidence,
            assumptions: Vec::new(),
            required_tier,
            advisory: true,
        }
    }

    #[must_use]
    pub fn with_change(mut self, change: SuggestedChange) -> Self {
        self.changes.push(change);
        self
    }

    #[must_use]
    pub fn with_evidence(mut self, evidence: Evidence) -> Self {
        self.evidence.push(evidence);
        self
    }

    #[must_use]
    pub fn with_rejected(mut self, rejected: RejectedAlternative) -> Self {
        self.rejected.push(rejected);
        self
    }

    #[must_use]
    pub fn with_assumption(mut self, assumption: impl Into<Assumption>) -> Self {
        self.assumptions.push(assumption.into());
        self
    }

    #[must_use]
    pub fn with_assumptions<I>(mut self, assumptions: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Assumption>,
    {
        self.assumptions
            .extend(assumptions.into_iter().map(Into::into));
        self
    }

    /// Whether this recommendation is advisory — always `true`; a
    /// recommendation is never applied automatically.
    #[must_use]
    pub const fn advisory(&self) -> bool {
        self.advisory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_is_always_true() {
        let r = Recommendation::new(
            BottleneckClass::DecodeBound,
            "decode-bound",
            Confidence::Medium,
            Tier::Pro,
        );
        assert!(r.advisory);
        assert!(r.advisory());
    }

    #[test]
    fn evidence_holds_reflects_comparison() {
        assert!(Evidence::above("gpu.sm_utilization", 0.9, 0.85).holds());
        assert!(!Evidence::above("gpu.sm_utilization", 0.8, 0.85).holds());
        assert!(Evidence::below("gpu.sm_utilization", 0.2, 0.35).holds());
    }

    #[test]
    fn builder_collects_fields() {
        let r = Recommendation::new(
            BottleneckClass::PrefillBound,
            "prefill-bound",
            Confidence::Medium,
            Tier::Pro,
        )
        .with_change(SuggestedChange::new(
            "chunked-prefill",
            ChangeDirection::Enable,
            "cap prompt concurrency",
        ))
        .with_evidence(Evidence::above("latency.ttft_ms.p90", 800.0, 500.0))
        .with_rejected(RejectedAlternative::new("switch model", "TPOT healthy"))
        .with_assumption("assumed prompt 512 tokens");

        assert_eq!(r.changes.len(), 1);
        assert_eq!(r.evidence.len(), 1);
        assert_eq!(r.rejected.len(), 1);
        assert_eq!(r.assumptions.len(), 1);
    }

    #[test]
    fn round_trips_json() {
        let r = Recommendation::new(
            BottleneckClass::OverProvisioned,
            "over-provisioned",
            Confidence::Low,
            Tier::Pro,
        )
        .with_change(SuggestedChange::new(
            "replicas",
            ChangeDirection::Decrease,
            "fewer replicas",
        ));
        let json = serde_json::to_string(&r).unwrap();
        let back: Recommendation = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
