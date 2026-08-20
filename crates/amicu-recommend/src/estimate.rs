//! The estimated half of the engine's input.
//!
//! A [`WorkloadEstimate`] carries the expected serving behavior *and the
//! assumptions it was derived from* (prompt/generation length, concurrency), so
//! that when a measured [`StackMetrics`](amicu_telemetry::StackMetrics)
//! diverges, the divergence is attributable to a specific assumption rather than
//! presented as truth. Expected latencies are [`EstimateRange`]s, never points.

use amicu_core::estimate::{Assumption, EstimateRange};
use serde::{Deserialize, Serialize};

/// Expected serving behavior and the assumptions behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadEstimate {
    /// Assumed prompt length (tokens) the estimate was derived for.
    pub assumed_prompt_tokens: u32,
    /// Assumed generation length (tokens).
    pub assumed_generation_tokens: u32,
    /// Assumed concurrent request count.
    pub assumed_concurrency: u32,
    /// Expected time-to-first-token (ms), as a range.
    pub expected_ttft_ms: EstimateRange<f64>,
    /// Expected time-per-output-token (ms), as a range.
    pub expected_tpot_ms: EstimateRange<f64>,
}

impl WorkloadEstimate {
    /// The stated assumptions, as [`Assumption`]s, for attaching to a
    /// recommendation so divergence is attributable.
    #[must_use]
    pub fn assumptions(&self) -> Vec<Assumption> {
        vec![
            Assumption::new(format!(
                "assumed prompt {} tokens, generation {} tokens",
                self.assumed_prompt_tokens, self.assumed_generation_tokens
            )),
            Assumption::new(format!("assumed concurrency {}", self.assumed_concurrency)),
        ]
    }

    /// A divergence note when a measured value exceeds the estimate's upper
    /// bound, else `None`. Used to explain a diagnosis honestly.
    #[must_use]
    pub fn ttft_divergence(&self, measured_ms: f64) -> Option<Assumption> {
        divergence("TTFT", measured_ms, *self.expected_ttft_ms.high())
    }

    #[must_use]
    pub fn tpot_divergence(&self, measured_ms: f64) -> Option<Assumption> {
        divergence("TPOT", measured_ms, *self.expected_tpot_ms.high())
    }
}

fn divergence(label: &str, measured_ms: f64, expected_high_ms: f64) -> Option<Assumption> {
    if measured_ms > expected_high_ms && expected_high_ms > 0.0 {
        let ratio = measured_ms / expected_high_ms;
        Some(Assumption::new(format!(
            "measured {label} {measured_ms:.0}ms is {ratio:.1}x the estimated upper bound {expected_high_ms:.0}ms"
        )))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::estimate::{Confidence, Provenance};

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

    #[test]
    fn assumptions_state_prompt_gen_and_concurrency() {
        let a = estimate().assumptions();
        assert_eq!(a.len(), 2);
        assert!(a[0].statement.contains("512"));
        assert!(a[1].statement.contains("concurrency 8"));
    }

    #[test]
    fn divergence_fires_only_above_upper_bound() {
        let e = estimate();
        assert!(e.ttft_divergence(250.0).is_none());
        let note = e.ttft_divergence(900.0).unwrap();
        assert!(note.statement.contains("3.0x"));
    }
}
