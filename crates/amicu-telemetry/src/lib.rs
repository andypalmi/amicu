//! Local telemetry: basic per-deployment metrics and privacy-safe request
//! logging.
//!
//! Three pieces, all content-free by construction:
//! - [`MetricsCollector`] aggregates counts, latency, throughput, and a memory
//!   gauge per deployment into a [`MetricsSnapshot`] aligned with
//!   [`RuntimeMetrics`](amicu_runtime::RuntimeMetrics).
//! - [`RequestRecord`] captures only metadata (ids, token **counts**, latency,
//!   outcome) - it has no field able to hold prompt or completion text, mirroring
//!   [`SecretValue`](amicu_core::secret::SecretValue)'s leak-prevention by
//!   omission. [`LoggingPolicy`] governs whether that metadata is emitted;
//!   content is never a logging option.
//! - [`StackMetrics`] is the typed metric surface `GET /stacks/:id/metrics`
//!   serializes: latency as [`Distribution`]s of percentiles, throughput,
//!   concurrency, queue, memory/KV, GPU, cost, and reliability. It is the
//!   measured input to the recommendation engine (`amicu-recommend`).

mod log;
mod metrics;
mod schema;

pub use log::{LogLevel, LoggingPolicy, Outcome, RequestLog, RequestRecord};
pub use metrics::{MetricsCollector, MetricsSnapshot};
pub use schema::{
    ConcurrencyMetrics, CostMetrics, Distribution, GpuMetrics, LatencyMetrics, MemoryKvMetrics,
    QueueMetrics, ReliabilityMetrics, StackMetrics, ThroughputMetrics,
};
