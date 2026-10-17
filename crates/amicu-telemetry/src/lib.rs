//! Local telemetry: basic per-deployment metrics and privacy-safe request
//! logging.
//!
//! All content-free by construction:
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
//!   docs for every field. Nothing in this crate opens a network connection.
//! - [`StackMetrics`] is the typed metric surface `GET /stacks/:id/metrics`
//!   serializes: latency as [`Distribution`]s of percentiles, throughput,
//!   concurrency, queue, memory/KV, GPU, cost, and reliability. It is the
//!   measured input to the recommendation engine (`amicu-recommend`).
//! - The telemetry agent's building blocks, used only when the user opted
//!   in: [`rollup`] folds `/metrics` scrapes into 5-minute windows, [`queue`]
//!   is the bounded on-disk buffer, and [`upload`] packs batches and backs
//!   off. The HTTP client and the consent check live in the CLI.

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

pub mod hardware;
mod identity;
mod log;
mod metrics;
pub mod prometheus;
pub mod queue;
mod report;
pub mod rollup;
mod schema;
pub mod upload;

pub use identity::{
    AnonDeploymentId, BatchId, Identity, IdentityStore, InstallId, MemoryIdentityStore, StoreError,
};
pub use log::{LogLevel, LoggingPolicy, Outcome, RequestLog, RequestRecord};
pub use metrics::{MetricsCollector, MetricsSnapshot};
pub use report::{
    E2E_BOUNDS_MS, HardwareInfo, Histogram, HistogramKind, Label, MAX_BATCH_BYTES,
    MAX_BATCH_REPORTS, MAX_LABEL_LEN, MAX_WINDOW_SECONDS, ModelInfo, Rollup, RuntimeInfo,
    TELEMETRY_SCHEMA_VERSION, TPOT_BOUNDS_MS, TTFT_BOUNDS_MS, TelemetryBatch, TelemetryReport,
    ValidationError, Window,
};
pub use schema::{
    ConcurrencyMetrics, CostMetrics, Distribution, GpuMetrics, LatencyMetrics, MemoryKvMetrics,
    QueueMetrics, ReliabilityMetrics, StackMetrics, ThroughputMetrics,
};
