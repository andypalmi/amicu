//! The process-supervision seam and its launchd/systemd implementations.
//!
//! The adapter delegates its apply/runtime-time methods to a [`ProcessSupervisor`],
//! keeping launch/health/stop mechanics out of the runtime contract.
//! [`ServiceSupervisor`] is the real implementation, generic over a
//! [`ServiceManager`] (launchd or systemd) and a [`HealthProbe`]; both ports are
//! injectable so unit tests run without a real service manager or network.
//! [`LaunchdSupervisor`] and [`SystemdSupervisor`] pin the host wiring.

use amicu_core::ByteSize;
use amicu_core::resource::Runtime;
use amicu_core::secret::SecretValue;
use amicu_runtime::{
    LaunchPlan, LogOptions, LogStream, RuntimeHandle, RuntimeMetrics, RuntimeStatus,
};
use async_trait::async_trait;

use crate::error::LlamaCppError;
use crate::health::{Health, HealthProbe, HttpHealthProbe};
use crate::service::{
    HostExec, LaunchdManager, ServiceManager, ServiceSpec, ServiceState, SystemdManager,
};

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

/// The real supervisor: a [`ServiceManager`] runs the process, a [`HealthProbe`]
/// checks endpoint readiness, and the two combine into the health tri-state
/// surfaced through [`status`](ProcessSupervisor::status).
pub struct ServiceSupervisor<M, P> {
    manager: M,
    probe: P,
}

impl<M: ServiceManager, P: HealthProbe> ServiceSupervisor<M, P> {
    /// Build over an injected service manager and health probe.
    pub fn new(manager: M, probe: P) -> Self {
        Self { manager, probe }
    }
}

/// The launchd (macOS) supervisor: a LaunchAgent plist driven by `launchctl`.
pub type LaunchdSupervisor = ServiceSupervisor<LaunchdManager<HostExec>, HttpHealthProbe>;

/// The systemd (Linux) supervisor: a transient `--user` unit driven by `systemctl`.
pub type SystemdSupervisor = ServiceSupervisor<SystemdManager<HostExec>, HttpHealthProbe>;

impl LaunchdSupervisor {
    /// Host-wired launchd supervisor.
    #[must_use]
    pub fn host() -> Self {
        Self::new(LaunchdManager::host(), HttpHealthProbe::new())
    }
}

impl SystemdSupervisor {
    /// Host-wired systemd supervisor.
    #[must_use]
    pub fn host() -> Self {
        Self::new(SystemdManager::host(), HttpHealthProbe::new())
    }
}

/// The supervisor for the host's service manager: launchd on macOS, systemd
/// elsewhere.
#[must_use]
pub fn host_supervisor() -> Box<dyn ProcessSupervisor> {
    #[cfg(target_os = "macos")]
    {
        Box::new(LaunchdSupervisor::host())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(SystemdSupervisor::host())
    }
}

fn endpoint_for(plan: &LaunchPlan) -> String {
    let port = match &plan.settings {
        Runtime::LlamaCpp(s) => s.port,
        Runtime::Vllm(s) => s.port,
    };
    format!("http://127.0.0.1:{port}")
}

#[async_trait]
impl<M: ServiceManager, P: HealthProbe> ProcessSupervisor for ServiceSupervisor<M, P> {
    async fn start(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        let argv = plan
            .rendered
            .argv()
            .ok_or_else(|| {
                LlamaCppError::Supervisor(
                    "llama.cpp supervisor expects a command launch".to_owned(),
                )
            })?
            .to_vec();
        // Secrets enter the process environment only - never argv or logs.
        let env = secrets
            .iter()
            .map(|s| {
                s.value
                    .expose_secret_str()
                    .map(|v| (s.name.clone(), v.to_owned()))
                    .map_err(|_| {
                        LlamaCppError::Supervisor(format!("secret '{}' is not valid utf-8", s.name))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let label = format!("io.amicu.llama.{}", plan.deployment);
        let spec = ServiceSpec {
            label: label.clone(),
            argv,
            env,
        };
        self.manager.start(&spec)?;
        Ok(RuntimeHandle {
            id: label,
            deployment: plan.deployment.clone(),
            endpoint: endpoint_for(plan),
        })
    }

    async fn status(&self, handle: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError> {
        match self.manager.state(&handle.id)? {
            ServiceState::Inactive => Ok(RuntimeStatus::Stopped {
                detail: "service inactive".to_owned(),
            }),
            ServiceState::Failed => Ok(RuntimeStatus::Failed {
                detail: format!("{}: service exited abnormally", Health::Failed),
            }),
            ServiceState::Active => {
                let health = if self.probe.is_ready(&handle.endpoint).await {
                    Health::Healthy
                } else {
                    Health::Degraded
                };
                let detail = match health {
                    Health::Healthy => health.to_string(),
                    _ => format!("{health}: endpoint not ready"),
                };
                Ok(RuntimeStatus::Running { detail })
            }
        }
    }

    async fn metrics(&self, _handle: &RuntimeHandle) -> Result<RuntimeMetrics, LlamaCppError> {
        Ok(RuntimeMetrics {
            requests_total: 0,
            latency_p50_ms: 0.0,
            tokens_per_second: 0.0,
            memory_used: ByteSize::from_bytes(0),
        })
    }

    async fn logs(
        &self,
        handle: &RuntimeHandle,
        options: LogOptions,
    ) -> Result<LogStream, LlamaCppError> {
        Ok(LogStream(self.manager.logs(&handle.id, options)?))
    }

    async fn stop(&self, handle: &RuntimeHandle) -> Result<(), LlamaCppError> {
        self.manager.stop(&handle.id)
    }
}
