//! The runtime adapter family trait.

use amicu_core::adapter::{Adapter, AdapterError, DescribeCapabilities};
use amicu_core::diagnostic::Diagnostic;
use amicu_core::resource::Deployment;
use amicu_core::secret::SecretResolver;
use async_trait::async_trait;

use crate::capabilities::RuntimeCapabilities;
use crate::handle::{LogOptions, LogStream, RuntimeHandle, RuntimeMetrics, RuntimeStatus};
use crate::plan::LaunchPlan;

/// The contract llama.cpp, vLLM, and future serving runtimes implement.
///
/// The method's type is the load-bearing signal for *when* it runs:
///
/// - [`capabilities`](RuntimeAdapter::capabilities),
///   [`validate`](RuntimeAdapter::validate) and
///   [`plan_launch`](RuntimeAdapter::plan_launch) are **planning-time and
///   pure**: synchronous `fn`s returning owned data with no side effects and no
///   runtime required. `validate` reports artifact/hardware fit as
///   [`Diagnostic`]s; `plan_launch` renders an inspectable, secret-free
///   [`LaunchPlan`].
/// - [`apply`](RuntimeAdapter::apply), [`status`](RuntimeAdapter::status),
///   [`metrics`](RuntimeAdapter::metrics), [`logs`](RuntimeAdapter::logs) and
///   [`stop`](RuntimeAdapter::stop) are **apply/runtime-time**: `async fn`s that
///   touch the backend. `apply` resolves any referenced secret through the
///   [`SecretResolver`] — the only place secret values enter.
#[async_trait]
pub trait RuntimeAdapter: Adapter + DescribeCapabilities {
    /// This runtime's plain-data capability descriptor.
    fn capabilities(&self) -> RuntimeCapabilities;

    /// Planning-time, pure check of artifact/hardware fit for a deployment.
    fn validate(&self, d: &Deployment) -> Vec<Diagnostic>;

    /// Render the visible launch plan and its resource estimate.
    fn plan_launch(&self, d: &Deployment) -> Result<LaunchPlan, AdapterError>;

    /// Start the runtime instance, resolving referenced secrets at apply time.
    async fn apply(
        &self,
        p: &LaunchPlan,
        s: &dyn SecretResolver,
    ) -> Result<RuntimeHandle, AdapterError>;

    /// Report the instance's lifecycle state.
    async fn status(&self, h: &RuntimeHandle) -> Result<RuntimeStatus, AdapterError>;

    /// Read a metrics snapshot from the instance.
    async fn metrics(&self, h: &RuntimeHandle) -> Result<RuntimeMetrics, AdapterError>;

    /// Read logs from the instance.
    async fn logs(&self, h: &RuntimeHandle, o: LogOptions) -> Result<LogStream, AdapterError>;

    /// Stop the instance.
    async fn stop(&self, h: &RuntimeHandle) -> Result<(), AdapterError>;
}
