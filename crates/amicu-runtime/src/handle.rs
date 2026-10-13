//! Apply/runtime-time types: instance handle, status, metrics, and logs.

use amicu_core::ByteSize;
use amicu_core::id::DeploymentId;
use serde::{Deserialize, Serialize};

use crate::exposure::{EndpointExposure, GatewayAuth};

/// Identifies a started runtime instance and how to address it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeHandle {
    /// Opaque, backend-assigned instance id.
    pub id: String,
    /// The deployment this instance serves.
    pub deployment: DeploymentId,
    /// The address the instance serves on, e.g. `http://127.0.0.1:8080`.
    pub endpoint: String,
    /// How `endpoint` is reachable. Every adapter must declare it explicitly:
    /// `apply` uses it to decide what must be protected and how to check it.
    /// State written before this field existed deserializes as public with no
    /// gateway auth - the fail-closed reading.
    #[serde(default = "legacy_exposure")]
    pub exposure: EndpointExposure,
}

fn legacy_exposure() -> EndpointExposure {
    EndpointExposure::Public {
        gateway: GatewayAuth::None,
    }
}

/// Lifecycle state of a runtime instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimeStatus {
    /// Serving requests.
    Running {
        /// Safe summary (e.g. uptime, readiness).
        detail: String,
    },
    /// Stopped cleanly.
    Stopped {
        /// Safe summary of why it stopped.
        detail: String,
    },
    /// Terminated abnormally.
    Failed {
        /// Safe summary of the failure.
        detail: String,
    },
}

impl RuntimeStatus {
    /// Whether the instance is currently [`RuntimeStatus::Running`].
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, RuntimeStatus::Running { .. })
    }
}

/// A small, representative snapshot of runtime metrics.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RuntimeMetrics {
    /// Total requests served since start.
    pub requests_total: u64,
    /// Median request latency, in milliseconds.
    pub latency_p50_ms: f64,
    /// Generation throughput, in tokens per second.
    pub tokens_per_second: f64,
    /// Device memory currently in use.
    pub memory_used: ByteSize,
}

/// Options controlling a log read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LogOptions {
    /// Keep streaming new lines rather than returning immediately.
    pub follow: bool,
    /// Return only the last `n` lines when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u32>,
}

/// A simple owned batch of log lines.
///
/// Kept intentionally minimal for now; this can become a real async stream
/// (e.g. a `Stream<Item = String>`) once log tailing is wired up, without
/// changing the trait's shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogStream(pub Vec<String>);

impl LogStream {
    /// The captured lines.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.0
    }
}
