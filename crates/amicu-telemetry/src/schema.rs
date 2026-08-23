//! Serving-metric surface exposed by `GET /stacks/:id/metrics`.
//!
//! Typed, content-free measurements the Management API serializes and the UI
//! renders (the frontend computes nothing - see web RFC 0001 §4.4). Latency is
//! reported as [`Distribution`]s of percentiles, never means. Utilizations are
//! fractions in `[0, 1]`; token counts and event counts are exact. These types
//! are the measured half of the recommendation engine's input.

use amicu_core::ByteSize;
use amicu_core::id::DeploymentId;
use amicu_core::resource::Money;
use serde::{Deserialize, Serialize};

/// A quantity summarized by percentiles (P50/P90/P99), not a mean.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Distribution {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
}

impl Distribution {
    /// A distribution from explicit percentiles.
    #[must_use]
    pub fn new(p50: f64, p90: f64, p99: f64) -> Self {
        Self { p50, p90, p99 }
    }

    /// Summarize samples by nearest-rank percentile. Empty input is all-zero.
    #[must_use]
    pub fn from_samples(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        Self {
            p50: nearest_rank(&sorted, 50.0),
            p90: nearest_rank(&sorted, 90.0),
            p99: nearest_rank(&sorted, 99.0),
        }
    }
}

/// Nearest-rank percentile of a pre-sorted, non-empty slice. `pct` in `[0, 100]`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn nearest_rank(sorted: &[f64], pct: f64) -> f64 {
    let n = sorted.len();
    let rank = (pct / 100.0 * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

/// Latency surface: time-to-first-token, per-output-token, end-to-end (ms).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LatencyMetrics {
    /// Time to first token.
    pub ttft_ms: Distribution,
    /// Time per output token / inter-token latency.
    pub tpot_ms: Distribution,
    /// End-to-end request latency.
    pub e2e_ms: Distribution,
}

/// Throughput surface.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ThroughputMetrics {
    pub output_tokens_per_s: f64,
    pub prompt_tokens_per_s: f64,
    pub requests_per_s: f64,
}

/// Concurrency & batching surface.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ConcurrencyMetrics {
    pub in_flight_requests: u32,
    pub running_batch_size: u32,
    /// Configured `max-num-seqs`.
    pub max_num_seqs: u32,
    /// Running batch over `max-num-seqs`, a fraction in `[0, 1]`.
    pub seq_saturation: f64,
}

/// Admission-queue surface.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct QueueMetrics {
    pub depth: u32,
    /// Time spent waiting in queue before admission (ms).
    pub wait_ms: Distribution,
    pub admitted: u64,
    pub rejected: u64,
}

/// Memory / KV-cache surface.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct MemoryKvMetrics {
    /// KV-cache utilization, a fraction in `[0, 1]`.
    pub kv_cache_utilization: f64,
    /// vLLM preemptions in the window.
    pub preemptions: u64,
    /// KV recomputations in the window.
    pub recomputations: u64,
    /// Out-of-memory events in the window.
    pub oom_events: u64,
}

/// GPU surface.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GpuMetrics {
    /// SM (compute) utilization, a fraction in `[0, 1]`.
    pub sm_utilization: f64,
    pub vram_used: ByteSize,
    pub power_watts: f64,
}

impl Default for GpuMetrics {
    fn default() -> Self {
        Self {
            sm_utilization: 0.0,
            vram_used: ByteSize::from_bytes(0),
            power_watts: 0.0,
        }
    }
}

/// Cost surface, including utilization-adjusted idle burn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostMetrics {
    pub per_hour: Money,
    pub per_million_tokens: Money,
    pub per_request: Money,
    /// Per-hour cost divided by useful utilization - surfaces idle burn.
    pub utilization_adjusted_per_hour: Money,
}

impl Default for CostMetrics {
    fn default() -> Self {
        let zero = || Money::new(0.0, "USD");
        Self {
            per_hour: zero(),
            per_million_tokens: zero(),
            per_request: zero(),
            utilization_adjusted_per_hour: zero(),
        }
    }
}

/// Reliability surface. Rates are fractions in `[0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ReliabilityMetrics {
    pub error_rate: f64,
    pub timeout_rate: f64,
    pub restarts: u64,
}

/// The full measured metric surface for one stack over an observation window.
///
/// This is the body serialized by `GET /stacks/:id/metrics`. Content-free by
/// construction: every field is an identifier, a count, or a measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackMetrics {
    /// Deployment the window describes.
    pub deployment: DeploymentId,
    /// Length of the observation window, in seconds.
    pub window_seconds: u64,
    pub latency: LatencyMetrics,
    pub throughput: ThroughputMetrics,
    pub concurrency: ConcurrencyMetrics,
    pub queue: QueueMetrics,
    pub memory: MemoryKvMetrics,
    pub gpu: GpuMetrics,
    pub cost: CostMetrics,
    pub reliability: ReliabilityMetrics,
}

impl StackMetrics {
    /// An all-zero surface for a deployment with no measurements yet.
    #[must_use]
    pub fn empty(deployment: DeploymentId, window_seconds: u64) -> Self {
        Self {
            deployment,
            window_seconds,
            latency: LatencyMetrics::default(),
            throughput: ThroughputMetrics::default(),
            concurrency: ConcurrencyMetrics::default(),
            queue: QueueMetrics::default(),
            memory: MemoryKvMetrics::default(),
            gpu: GpuMetrics::default(),
            cost: CostMetrics::default(),
            reliability: ReliabilityMetrics::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_from_samples_nearest_rank() {
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        let d = Distribution::from_samples(&samples);
        assert!((d.p50 - 50.0).abs() < 1e-9);
        assert!((d.p90 - 90.0).abs() < 1e-9);
        assert!((d.p99 - 99.0).abs() < 1e-9);
    }

    #[test]
    fn distribution_ignores_sample_order() {
        let a = Distribution::from_samples(&[30.0, 10.0, 20.0, 50.0, 40.0]);
        let b = Distribution::from_samples(&[10.0, 20.0, 30.0, 40.0, 50.0]);
        assert_eq!(a, b);
    }

    #[test]
    fn distribution_empty_is_zero() {
        assert_eq!(Distribution::from_samples(&[]), Distribution::default());
    }

    #[test]
    fn distribution_single_sample() {
        let d = Distribution::from_samples(&[7.0]);
        assert_eq!(d, Distribution::new(7.0, 7.0, 7.0));
    }

    #[test]
    fn stack_metrics_round_trips() {
        let mut m = StackMetrics::empty(DeploymentId::new("chat_prod"), 60);
        m.latency.ttft_ms = Distribution::new(120.0, 480.0, 900.0);
        m.gpu.sm_utilization = 0.91;
        m.cost.per_hour = Money::new(1.5, "USD");

        let json = serde_json::to_string(&m).unwrap();
        let back: StackMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn metric_surface_serializes_percentiles_not_means() {
        let mut m = StackMetrics::empty(DeploymentId::new("d"), 60);
        m.latency.e2e_ms = Distribution::new(1.0, 2.0, 3.0);
        let value = serde_json::to_value(&m).unwrap();
        let e2e = &value["latency"]["e2e_ms"];
        assert!(e2e.get("p50").is_some());
        assert!(e2e.get("p99").is_some());
        assert!(e2e.get("mean").is_none());
    }
}
