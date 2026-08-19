//! The process-supervision seam.
//!
//! The adapter delegates its apply/runtime-time methods to a [`ProcessSupervisor`],
//! keeping launch/health/stop mechanics out of the runtime contract. Real
//! launchd/systemd supervisors land in TASK-008; this task ships only the trait
//! and a test fake so the full [`RuntimeAdapter`](amicu_runtime::RuntimeAdapter)
//! path is exercised.

use amicu_core::secret::SecretValue;
use amicu_runtime::{
    LaunchPlan, LogOptions, LogStream, RuntimeHandle, RuntimeMetrics, RuntimeStatus,
};
use async_trait::async_trait;

use crate::error::LlamaCppError;

/// A resolved secret handed to a supervisor at start time: its name and value.
///
/// Values never appear in the [`LaunchPlan`] or logs; they enter only here, at
/// apply time.
pub struct ResolvedSecret {
    /// The secret's reference name.
    pub name: String,
    /// The resolved value.
    pub value: SecretValue,
}

/// Starts, inspects, and stops a rendered launch against some process backend
/// (launchd, systemd, a raw child process).
#[async_trait]
pub trait ProcessSupervisor: Send + Sync {
    /// Start the rendered command, returning a handle to the instance.
    ///
    /// `secrets` are resolved values injected into the process environment; they
    /// must never be written to the plan or logs.
    async fn start(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError>;

    /// Report the instance's lifecycle state.
    async fn status(&self, handle: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError>;

    /// Read a metrics snapshot.
    async fn metrics(&self, handle: &RuntimeHandle) -> Result<RuntimeMetrics, LlamaCppError>;

    /// Read logs.
    async fn logs(
        &self,
        handle: &RuntimeHandle,
        options: LogOptions,
    ) -> Result<LogStream, LlamaCppError>;

    /// Stop the instance.
    async fn stop(&self, handle: &RuntimeHandle) -> Result<(), LlamaCppError>;
}
