//! Generic estimate metadata: confidence, provenance, assumptions, and a
//! range type that carries them.
//!
//! These types are domain-agnostic plain data. Cost estimation
//! (`amicu-providers`), the recommendation engine, and telemetry comparison all
//! produce [`EstimateRange`] values so estimates uniformly expose their
//! low/high bounds, the [`Assumption`]s behind them, their [`Provenance`], and a
//! [`Confidence`] level. Estimates are ranges with assumptions, never
//! guarantees.

use serde::{Deserialize, Serialize};

/// How much an estimate can be relied on, per the PRD confidence table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Sufficient closely-matching observations; reliable under stated
    /// assumptions.
    High,
    /// Smaller relevant observation set or strong comparable public evidence;
    /// pilot recommended.
    Medium,
    /// Limited data or material extrapolation; directional only.
    Low,
    /// Formula / public hardware / provider data with no close direct
    /// observation; a planning estimate to measure before relying on.
    Estimated,
}

/// Where the evidence behind an estimate comes from, per the PRD provenance
/// categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Aggregated anonymized telemetry.
    TelemetryAggregate,
    /// Public benchmark data.
    PublicBenchmark,
    /// Provider price lists.
    ProviderPrice,
    /// A formula over known inputs.
    Formula,
}

/// A single stated assumption an estimate depends on, e.g. `"730 hours/month"`
/// or `"spot availability assumed"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assumption {
    /// Human-readable statement of the assumption.
    pub statement: String,
}

impl Assumption {
    /// An assumption from a statement.
    #[must_use]
    pub fn new(statement: impl Into<String>) -> Self {
        Self {
            statement: statement.into(),
        }
    }
}

impl<S: Into<String>> From<S> for Assumption {
    fn from(statement: S) -> Self {
        Self::new(statement)
    }
}

/// An estimate expressed as a low/high range of `T`, carrying the assumptions
/// behind it, its provenance, and a confidence level.
///
/// `T` is the estimated quantity (money, latency, throughput, …). The range is
/// inclusive; `low` and `high` are equal for a point estimate. Ordering
/// (`low <= high`) is not enforced by construction — use [`is_ordered`] to
/// check it for orderable `T`.
///
/// [`is_ordered`]: EstimateRange::is_ordered
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimateRange<T> {
    /// Lower bound of the estimate.
    pub low: T,
    /// Upper bound of the estimate.
    pub high: T,
    /// Assumptions the estimate depends on.
    #[serde(default = "Vec::new", skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<Assumption>,
    /// Where the estimate's evidence comes from.
    pub provenance: Provenance,
    /// How much the estimate can be relied on.
    pub confidence: Confidence,
}

impl<T> EstimateRange<T> {
    /// A range from `low` to `high` with no assumptions yet.
    #[must_use]
    pub fn new(low: T, high: T, provenance: Provenance, confidence: Confidence) -> Self {
        Self {
            low,
            high,
            assumptions: Vec::new(),
            provenance,
            confidence,
        }
    }

    /// A point estimate where `low == high`.
    #[must_use]
    pub fn point(value: T, provenance: Provenance, confidence: Confidence) -> Self
    where
        T: Clone,
    {
        Self::new(value.clone(), value, provenance, confidence)
    }

    /// Add one assumption.
    #[must_use]
    pub fn with_assumption(mut self, assumption: impl Into<Assumption>) -> Self {
        self.assumptions.push(assumption.into());
        self
    }

    /// Append several assumptions.
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

    /// The lower bound.
    #[must_use]
    pub fn low(&self) -> &T {
        &self.low
    }

    /// The upper bound.
    #[must_use]
    pub fn high(&self) -> &T {
        &self.high
    }

    /// The assumptions behind the estimate.
    #[must_use]
    pub fn assumptions(&self) -> &[Assumption] {
        &self.assumptions
    }

    /// The estimate's provenance.
    #[must_use]
    pub fn provenance(&self) -> Provenance {
        self.provenance
    }

    /// The estimate's confidence.
    #[must_use]
    pub fn confidence(&self) -> Confidence {
        self.confidence
    }
}

impl<T: PartialOrd> EstimateRange<T> {
    /// Whether the bounds are ordered `low <= high`.
    #[must_use]
    pub fn is_ordered(&self) -> bool {
        self.low <= self.high
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Confidence::Estimated).unwrap(),
            "\"estimated\""
        );
        for c in [
            Confidence::High,
            Confidence::Medium,
            Confidence::Low,
            Confidence::Estimated,
        ] {
            let json = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<Confidence>(&json).unwrap(), c);
        }
    }

    #[test]
    fn provenance_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Provenance::ProviderPrice).unwrap(),
            "\"provider_price\""
        );
        for p in [
            Provenance::TelemetryAggregate,
            Provenance::PublicBenchmark,
            Provenance::ProviderPrice,
            Provenance::Formula,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(serde_json::from_str::<Provenance>(&json).unwrap(), p);
        }
    }

    #[test]
    fn range_round_trips_and_carries_metadata() {
        let range = EstimateRange::new(10_i64, 20_i64, Provenance::Formula, Confidence::Estimated)
            .with_assumption("730 hours/month")
            .with_assumptions(["spot availability assumed", "single instance"]);

        assert_eq!(range.low(), &10);
        assert_eq!(range.high(), &20);
        assert_eq!(range.assumptions().len(), 3);
        assert_eq!(range.provenance(), Provenance::Formula);
        assert_eq!(range.confidence(), Confidence::Estimated);

        let json = serde_json::to_string(&range).unwrap();
        let back: EstimateRange<i64> = serde_json::from_str(&json).unwrap();
        assert_eq!(range, back);
    }

    #[test]
    fn is_ordered_reports_bound_order() {
        let ok = EstimateRange::new(1_i64, 2, Provenance::Formula, Confidence::Estimated);
        assert!(ok.is_ordered());
        let point = EstimateRange::point(5_i64, Provenance::Formula, Confidence::Estimated);
        assert!(point.is_ordered());
        let bad = EstimateRange::new(9_i64, 3, Provenance::Formula, Confidence::Estimated);
        assert!(!bad.is_ordered());
    }

    #[test]
    fn empty_assumptions_are_omitted_from_json() {
        let range = EstimateRange::new(0_u32, 0, Provenance::ProviderPrice, Confidence::Estimated);
        let value = serde_json::to_value(&range).unwrap();
        assert!(value.get("assumptions").is_none());
    }
}
