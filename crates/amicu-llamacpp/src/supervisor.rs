//! The process-supervision seam and its launchd/systemd implementations.
//!
//! The adapter delegates its apply/runtime-time methods to a [`ProcessSupervisor`],
//! keeping launch/health/stop mechanics out of the runtime contract.
//! [`ServiceSupervisor`] is the real implementation, generic over a
//! [`ServiceManager`] (launchd or systemd) and a [`HealthProbe`]; both ports are
//! injectable so unit tests run without a real service manager or network.
//! [`LaunchdSupervisor`] and [`SystemdSupervisor`] pin the host wiring.

use std::path::Path;
use std::time::Duration;

use amicu_core::ByteSize;
use amicu_core::adapter::AdapterError;
use amicu_core::diagnostic::catalog;
use amicu_core::resource::Runtime;
use amicu_core::secret::{SecretResolver, SecretValue};
use amicu_runtime::exposure::CheckTiming;
use amicu_runtime::{
    EndpointExposure, LaunchPlan, LogOptions, LogStream, RuntimeHandle, RuntimeMetrics,
    RuntimeStatus,
};
use async_trait::async_trait;

use crate::binary::{CommandRunner, HostRunner};
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

/// Resolve everything a launch needs from the [`SecretResolver`], at apply
/// time: the plan's `secret_refs` (each injected under its own name) and the
/// endpoint API key, injected under the runtime's own key variable
/// ([`RuntimeKind::api_key_env_var`]: `LLAMA_API_KEY` / `VLLM_API_KEY`).
///
/// This is the only place the key becomes a value, and it is only ever handed
/// to a supervisor as a [`ResolvedSecret`] - which delivers it through the
/// process/container environment, never argv. Nothing here logs or formats a
/// value, and the errors name the secret reference, not its content.
///
/// # Errors
/// The resolver's error, or `AMICU-POLICY-008` if the key value cannot be
/// delivered safely (empty, a comma, whitespace, non-printable characters).
pub async fn resolve_launch_secrets(
    plan: &LaunchPlan,
    resolver: &dyn SecretResolver,
) -> Result<Vec<ResolvedSecret>, AdapterError> {
    let mut secrets = Vec::with_capacity(plan.secret_refs.len() + 1);
    for reference in &plan.secret_refs {
        let value = resolver.resolve(reference).await?;
        secrets.push(ResolvedSecret {
            name: reference.name.clone(),
            value,
        });
    }
    if let Some(reference) = &plan.api_key {
        let value = resolver.resolve(reference).await?;
        if let Err(why) = amicu_runtime::exposure::validate_api_key_value(&value) {
            return Err(AdapterError::permanent(
                catalog::POLICY_ENDPOINT_API_KEY_INVALID,
                format!("endpoint API key secret '{}' {why}", reference.name),
            ));
        }
        secrets.push(ResolvedSecret {
            name: plan.runtime.api_key_env_var().to_owned(),
            value,
        });
    }
    Ok(secrets)
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
    runner: Box<dyn CommandRunner>,
    ready: ReadyWait,
}

/// How long [`ServiceSupervisor::start`] waits for the endpoint to answer its
/// readiness probe, and how often it asks. The default reuses the exposure
/// check's local window: the service may still be loading multi-GB weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyWait {
    /// Give up (stop the service, fail) after this long. Zero skips the wait.
    pub timeout: Duration,
    /// Pause between probes.
    pub interval: Duration,
}

impl ReadyWait {
    /// The default local window, shared with the exposure check.
    pub const LOCAL: Self = Self {
        timeout: CheckTiming::LOCAL.accept_timeout,
        interval: CheckTiming::LOCAL.interval,
    };
}

impl<M: ServiceManager, P: HealthProbe> ServiceSupervisor<M, P> {
    /// Build over an injected service manager and health probe.
    pub fn new(manager: M, probe: P) -> Self {
        Self {
            manager,
            probe,
            runner: Box::new(HostRunner),
            ready: ReadyWait::LOCAL,
        }
    }

    /// Override how the runtime binary is searched for on `PATH` (tests).
    #[must_use]
    pub fn with_runner(mut self, runner: Box<dyn CommandRunner>) -> Self {
        self.runner = runner;
        self
    }

    /// Override the readiness window.
    #[must_use]
    pub fn with_ready_wait(mut self, ready: ReadyWait) -> Self {
        self.ready = ready;
        self
    }

    /// `argv` with `argv[0]` resolved to an absolute path. launchd and systemd
    /// run with a minimal `PATH` (no Homebrew), so a bare `llama-server` /
    /// `vllm` must be located here, on the CLI's own `PATH`.
    fn resolve_program(&self, mut argv: Vec<String>) -> Result<Vec<String>, LlamaCppError> {
        let Some(program) = argv.first() else {
            return Ok(argv);
        };
        if Path::new(program).is_absolute() {
            return Ok(argv);
        }
        let found = self
            .runner
            .which(program)
            .and_then(|p| std::path::absolute(p).ok())
            .ok_or_else(|| LlamaCppError::BinaryNotOnPath {
                hint: install_hint(program).to_owned(),
                name: program.clone(),
            })?;
        argv[0] = found.to_string_lossy().into_owned();
        Ok(argv)
    }

    /// Block until the endpoint is ready. On a crash or timeout the service is
    /// stopped (fail closed) and the error names where its logs are.
    async fn wait_ready(&self, label: &str, endpoint: &str) -> Result<(), LlamaCppError> {
        if self.ready.timeout.is_zero() {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + self.ready.timeout;
        let reason = loop {
            if self.probe.is_ready(endpoint).await {
                return Ok(());
            }
            if matches!(self.manager.state(label), Ok(ServiceState::Failed)) {
                break "the service exited abnormally".to_owned();
            }
            if tokio::time::Instant::now() >= deadline {
                break format!(
                    "{endpoint} was not ready within {}s",
                    self.ready.timeout.as_secs()
                );
            }
            tokio::time::sleep(self.ready.interval).await;
        };
        let logs = self.manager.log_location(label);
        Err(LlamaCppError::Supervisor(match self.manager.stop(label) {
            Ok(()) => format!("{reason}; the service was stopped; see logs at {logs}"),
            Err(e) => format!(
                "{reason}; stopping it FAILED ({e}): stop {label} manually; see logs at {logs}"
            ),
        }))
    }
}

/// How to install a runtime binary, for the not-found error.
fn install_hint(program: &str) -> &'static str {
    match program {
        "llama-server" => "`brew install llama.cpp` (macOS) or see github.com/ggml-org/llama.cpp",
        "vllm" => "`pip install vllm`",
        _ => "your package manager",
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
        let settings_env = match &plan.settings {
            Runtime::LlamaCpp(s) => &s.env,
            Runtime::Vllm(s) => &s.env,
        };
        // A settings `env` key that is also a resolved secret's name is
        // ambiguous - refuse to guess which one the caller meant, rather than
        // letting one silently shadow the other (amicu-planning#150).
        if let Some(s) = secrets.iter().find(|s| settings_env.contains_key(&s.name)) {
            return Err(LlamaCppError::Supervisor(format!(
                "env var '{}' is set both by the deployment's `env` block and a resolved secret",
                s.name
            )));
        }
        // Merge order: settings.env first, then resolved secrets - never
        // logged, and (per the collision check above) secrets never actually
        // overwrite a settings value, only add distinct names.
        let mut env: Vec<(String, String)> = settings_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Secrets enter the process environment only - never argv or logs.
        for s in secrets {
            let value = s.value.expose_secret_str().map_err(|_| {
                LlamaCppError::Supervisor(format!("secret '{}' is not valid utf-8", s.name))
            })?;
            env.push((s.name.clone(), value.to_owned()));
        }
        let label = format!("io.amicu.llama.{}", plan.deployment);
        let spec = ServiceSpec {
            label: label.clone(),
            argv: self.resolve_program(argv)?,
            env,
        };
        self.manager.start(&spec)?;
        let endpoint = endpoint_for(plan);
        self.wait_ready(&label, &endpoint).await?;
        Ok(RuntimeHandle {
            id: label,
            deployment: plan.deployment.clone(),
            endpoint,
            // Every flag set renders `--host 127.0.0.1` explicitly.
            exposure: EndpointExposure::Loopback,
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
