//! Local telemetry: basic per-deployment metrics and privacy-safe request
//! logging.
//!
//! Two pieces, both content-free by construction:
//! - [`MetricsCollector`] aggregates counts, latency, throughput, and a memory
//!   gauge per deployment into a [`MetricsSnapshot`] aligned with
//!   [`RuntimeMetrics`](amicu_runtime::RuntimeMetrics).
//! - [`RequestRecord`] captures only metadata (ids, token **counts**, latency,
//!   outcome) — it has no field able to hold prompt or completion text, mirroring
//!   [`SecretValue`](amicu_core::secret::SecretValue)'s leak-prevention by
//!   omission. [`LoggingPolicy`] governs whether that metadata is emitted;
//!   content is never a logging option.

mod log;
mod metrics;

pub use log::{LogLevel, LoggingPolicy, Outcome, RequestLog, RequestRecord};
pub use metrics::{MetricsCollector, MetricsSnapshot};
