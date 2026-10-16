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
//! - [`TelemetryReport`] is the opt-in, aggregates-only, versioned payload
//!   (hardware/model/runtime/provider plus windowed rollups) keyed by random,
//!   rotatable [`InstallId`] and [`AnonDeploymentId`]; see the `report` module
//!   docs for every field. Nothing in this crate sends it.
//! - [`StackMetrics`] is the typed metric surface `GET /stacks/:id/metrics`
//!   serializes: latency as [`Distribution`]s of percentiles, throughput,
//!   concurrency, queue, memory/KV, GPU, cost, and reliability. It is the
//!   measured input to the recommendation engine (`amicu-recommend`).

pub mod hardware;
mod identity;
mod log;
mod metrics;
pub mod prometheus;
mod report;
mod schema;

pub use identity::{
    AnonDeploymentId, Identity, IdentityStore, InstallId, MemoryIdentityStore, StoreError,
};
pub use log::{LogLevel, LoggingPolicy, Outcome, RequestLog, RequestRecord};
pub use metrics::{MetricsCollector, MetricsSnapshot};
pub use report::{
    HardwareInfo, Label, MAX_LABEL_LEN, MAX_WINDOW_SECONDS, ModelInfo, Percentiles, Rollup,
    RuntimeInfo, TELEMETRY_SCHEMA_VERSION, TelemetryReport, ValidationError, Window,
};
pub use schema::{
    ConcurrencyMetrics, CostMetrics, Distribution, GpuMetrics, LatencyMetrics, MemoryKvMetrics,
    QueueMetrics, ReliabilityMetrics, StackMetrics, ThroughputMetrics,
};
