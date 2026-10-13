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
///   [`SecretResolver`] - the only place secret values enter.
///
/// # Secrets and endpoint exposure contract
///
/// - **Secrets travel by environment or file, never argv.** Anything resolved
///   from a [`SecretResolver`] (the endpoint API key in
///   [`LaunchPlan::api_key`], tokens in `secret_refs`) must reach the runtime
///   through its environment (or a `0600` file/Modal Secret that holds the
///   environment). Argv is world-readable via the process table and is baked
///   into container images, so a secret on argv is a bug. The user-facing
///   `extra_args` / `env` denylists exist so only amicu itself can set the
///   key variables, and only from the resolved secret.
/// - **Cleanup on stop.** Whatever [`apply`](RuntimeAdapter::apply) wrote to
///   hold a secret (env files, plist/unit files, provider secrets) must be
///   removed by [`stop`](RuntimeAdapter::stop), and on a failed start.
/// - **Secrets stay out of every sink.** No secret value in logs, errors,
///   `Debug`, [`RuntimeHandle`] (which is persisted), plans, or state.
/// - **Declare exposure.** The returned [`RuntimeHandle::exposure`] must say
///   whether the endpoint is loopback-only or reachable off-machine (and which
///   gateway auth fronts it). `apply` then runs the shared
///   [`exposure::verify_endpoint`](crate::exposure::verify_endpoint) check on
///   it and calls [`stop`](RuntimeAdapter::stop) if the endpoint turns out to
///   be unprotected, so `stop` must be safe to call on a just-started
///   instance.
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
