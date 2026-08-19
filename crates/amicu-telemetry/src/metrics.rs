//! Basic per-deployment metrics aggregation.
//!
//! [`MetricsCollector`] folds content-free [`RequestRecord`]s into per-deployment
//! aggregates and reports a [`MetricsSnapshot`] that aligns with
//! [`RuntimeMetrics`](amicu_runtime::RuntimeMetrics). Only counts, latencies,
//! and token totals are kept — never request or response content. Aggregation
//! is deliberately simple and clock-free: latency samples are retained so the
//! median is exact, and throughput is total generated tokens over total latency.

use std::collections::HashMap;
use std::sync::RwLock;

use amicu_core::ByteSize;
use amicu_core::id::DeploymentId;
use amicu_runtime::RuntimeMetrics;

use crate::log::RequestRecord;

/// Per-deployment running aggregate. Holds only numeric measurements.
#[derive(Debug, Clone)]
struct Aggregate {
    requests_total: u64,
    /// Retained latency samples (ms) for an exact median.
    latencies_ms: Vec<f64>,
    completion_tokens: u64,
    /// Sum of latencies (ms) across requests, for throughput.
    latency_sum_ms: f64,
    /// Last observed device-memory gauge.
    memory_used: ByteSize,
}

impl Default for Aggregate {
    fn default() -> Self {
        Self {
            requests_total: 0,
            latencies_ms: Vec::new(),
            completion_tokens: 0,
            latency_sum_ms: 0.0,
            memory_used: ByteSize::from_bytes(0),
        }
    }
}

impl Aggregate {
    fn record(&mut self, record: &RequestRecord) {
        self.requests_total += 1;
        self.latencies_ms.push(record.latency_ms);
        self.latency_sum_ms += record.latency_ms;
        self.completion_tokens += u64::from(record.completion_tokens);
    }

    fn p50_ms(&self) -> f64 {
        if self.latencies_ms.is_empty() {
            return 0.0;
        }
        let mut sorted = self.latencies_ms.clone();
        sorted.sort_by(f64::total_cmp);
        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 1 {
            sorted[mid]
        } else {
            f64::midpoint(sorted[mid - 1], sorted[mid])
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn tokens_per_second(&self) -> f64 {
        let seconds = self.latency_sum_ms / 1000.0;
        if seconds <= 0.0 {
            0.0
        } else {
            self.completion_tokens as f64 / seconds
        }
    }
}

/// A point-in-time aggregate for one deployment.
///
/// Field-compatible with [`RuntimeMetrics`]; use [`to_runtime_metrics`](Self::to_runtime_metrics).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricsSnapshot {
    pub requests_total: u64,
    pub latency_p50_ms: f64,
    pub tokens_per_second: f64,
    pub memory_used: ByteSize,
}

impl MetricsSnapshot {
    /// Convert to the runtime-facing [`RuntimeMetrics`] shape.
    #[must_use]
    pub fn to_runtime_metrics(&self) -> RuntimeMetrics {
        RuntimeMetrics {
            requests_total: self.requests_total,
            latency_p50_ms: self.latency_p50_ms,
            tokens_per_second: self.tokens_per_second,
            memory_used: self.memory_used,
        }
    }
}

/// Thread-safe collector of per-deployment metrics.
#[derive(Debug, Default)]
pub struct MetricsCollector {
    by_deployment: RwLock<HashMap<DeploymentId, Aggregate>>,
}

impl MetricsCollector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a content-free request record into its deployment's aggregate.
    pub fn record(&self, record: &RequestRecord) {
        self.by_deployment
            .write()
            .expect("metrics lock poisoned")
            .entry(record.deployment.clone())
            .or_default()
            .record(record);
    }

    /// Update the device-memory gauge for a deployment.
    pub fn observe_memory(&self, deployment: &DeploymentId, memory_used: ByteSize) {
        self.by_deployment
            .write()
            .expect("metrics lock poisoned")
            .entry(deployment.clone())
            .or_default()
            .memory_used = memory_used;
    }

    /// Snapshot a deployment's aggregate, if any requests were recorded.
    #[must_use]
    pub fn snapshot(&self, deployment: &DeploymentId) -> Option<MetricsSnapshot> {
        let guard = self.by_deployment.read().expect("metrics lock poisoned");
        let agg = guard.get(deployment)?;
        Some(MetricsSnapshot {
            requests_total: agg.requests_total,
            latency_p50_ms: agg.p50_ms(),
            tokens_per_second: agg.tokens_per_second(),
            memory_used: agg.memory_used,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(latency_ms: f64, completion_tokens: u32) -> RequestRecord {
        RequestRecord::new(
            0,
            DeploymentId::new("chat_local"),
            10,
            completion_tokens,
            latency_ms,
        )
    }

    #[test]
    fn snapshot_none_before_any_record() {
        let c = MetricsCollector::new();
        assert!(c.snapshot(&DeploymentId::new("chat_local")).is_none());
    }

    #[test]
    fn aggregates_count_median_and_throughput() {
        let c = MetricsCollector::new();
        // Latencies 10, 20, 30 ms; completion tokens 5, 10, 15 => 30 tokens.
        c.record(&record(10.0, 5));
        c.record(&record(30.0, 15));
        c.record(&record(20.0, 10));

        let snap = c.snapshot(&DeploymentId::new("chat_local")).unwrap();
        assert_eq!(snap.requests_total, 3);
        assert!((snap.latency_p50_ms - 20.0).abs() < 1e-9);
        // 30 tokens over (10+20+30)/1000 = 0.06 s => 500 tok/s.
        assert!((snap.tokens_per_second - 500.0).abs() < 1e-9);
        assert_eq!(snap.memory_used, ByteSize::from_bytes(0));
    }

    #[test]
    fn even_sample_count_medians() {
        let c = MetricsCollector::new();
        c.record(&record(10.0, 1));
        c.record(&record(20.0, 1));
        let snap = c.snapshot(&DeploymentId::new("chat_local")).unwrap();
        assert!((snap.latency_p50_ms - 15.0).abs() < 1e-9);
    }

    #[test]
    fn memory_gauge_and_runtime_conversion() {
        let c = MetricsCollector::new();
        c.record(&record(10.0, 4));
        c.observe_memory(&DeploymentId::new("chat_local"), ByteSize::from_bytes(2048));
        let snap = c.snapshot(&DeploymentId::new("chat_local")).unwrap();
        assert_eq!(snap.memory_used, ByteSize::from_bytes(2048));

        let rt = snap.to_runtime_metrics();
        assert_eq!(rt.requests_total, 1);
        assert_eq!(rt.memory_used, ByteSize::from_bytes(2048));
    }

    #[test]
    fn deployments_are_isolated() {
        let c = MetricsCollector::new();
        c.record(&record(10.0, 1));
        c.record(&RequestRecord::new(
            0,
            DeploymentId::new("other"),
            1,
            2,
            40.0,
        ));
        assert_eq!(
            c.snapshot(&DeploymentId::new("chat_local"))
                .unwrap()
                .requests_total,
            1
        );
        assert_eq!(
            c.snapshot(&DeploymentId::new("other"))
                .unwrap()
                .requests_total,
            1
        );
    }
}
