//! The process-supervision seam and its launchd/systemd implementations.
//!
//! The adapter delegates its apply/runtime-time methods to a [`ProcessSupervisor`],
//! keeping launch/health/stop mechanics out of the runtime contract.
//! [`ServiceSupervisor`] is the real implementation, generic over a
//! [`ServiceManager`] (launchd or systemd) and a [`HealthProbe`]; both ports are
//! injectable so unit tests run without a real service manager or network.
//! [`LaunchdSupervisor`] and [`SystemdSupervisor`] pin the host wiring.
//!
//! A [`SidecarPlanner`] can add a second, secret-free job next to each
//! instance: the opt-in telemetry agent. Its label derives from the
//! instance's ([`sidecar_label`]), and so does its definition file, so a stop
//! handle recorded for the instance also covers the sidecar: every stop stops
//! the sidecar first (when its definition is on disk), then the instance.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    HostExec, LaunchdManager, ServiceManager, ServicePaths, ServiceSpec, ServiceState,
    SystemdManager,
};
use crate::service_windows::TaskSchedulerManager;

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

    /// Stop the instance, deleting its service definition at the recorded
    /// path (`None`: recompute it, for records that predate the field).
    async fn stop_recorded(
        &self,
        handle: &RuntimeHandle,
        _definition: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        self.stop(handle).await
    }

    /// [`stop_recorded`](Self::stop_recorded), also deleting the recorded
    /// secrets environment file (systemd). `None`: derive it.
    async fn stop_recorded_with_env(
        &self,
        handle: &RuntimeHandle,
        definition: Option<&Path>,
        _env: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        self.stop_recorded(handle, definition).await
    }

    /// A one-line advisory to show after a host start (see
    /// [`ServiceManager::boot_hint`](crate::service::ServiceManager::boot_hint)).
    fn boot_hint(&self) -> Option<String> {
        None
    }

    /// Who the host service manager acts as (backend, uid, launchd domain),
    /// when this supervisor is host-based. Recorded with started instances.
    fn host_context(&self) -> Option<crate::service::HostContext> {
        None
    }

    /// The uid that owns an existing instance's service definition, when it
    /// can be told (launchd plist owner).
    fn instance_owner(&self, _label: &str) -> Option<u32> {
        None
    }

    /// Where the instance `label` keeps its definition (which holds secrets)
    /// and logs on this host, so they can be recorded in state before the
    /// start.
    fn service_paths(&self, _label: &str) -> Option<ServicePaths> {
        None
    }

    /// Read-only: is the instance `label` (started in launchd `domain`, if
    /// recorded) running? `Down` only when confirmed gone.
    fn presence(
        &self,
        _label: &str,
        _domain: Option<&str>,
    ) -> Result<crate::service::Presence, LlamaCppError> {
        Ok(crate::service::Presence::Unknown)
    }

    /// Every telemetry sidecar job this host has a definition for, from any
    /// project, with whether it is running.
    fn sidecars(&self) -> Vec<(String, crate::service::Presence)> {
        Vec::new()
    }

    /// Stop every telemetry sidecar job this host has a definition for, from
    /// any project (consent is per user). Returns the labels stopped.
    async fn stop_all_sidecars(&self) -> Result<Vec<String>, LlamaCppError> {
        Ok(Vec::new())
    }

    /// Run `spec` as the telemetry sidecar of `main_label`, a deployment that
    /// runs in the cloud and so has no host instance of its own. Replaces a
    /// previous sidecar of the same label. Unlike an instance's sidecar it
    /// may carry the provider credentials it reads metrics with.
    async fn start_cloud_sidecar(
        &self,
        _main_label: &str,
        _spec: &ServiceSpec,
    ) -> Result<(), LlamaCppError> {
        Ok(())
    }

    /// Stop the sidecar of `main_label`, if one was ever started, and delete
    /// its definition.
    async fn stop_sidecar_of(&self, _main_label: &str) -> Result<(), LlamaCppError> {
        Ok(())
    }
}

/// Decides whether a host instance gets a sidecar job (the telemetry agent)
/// and renders it. The sidecar carries no secrets.
pub trait SidecarPlanner: Send + Sync {
    /// The sidecar to run beside `plan`'s instance, labelled
    /// [`sidecar_label`]`(main_label)`, or `None` for no sidecar.
    fn sidecar(&self, plan: &LaunchPlan, main_label: &str) -> Option<ServiceSpec>;
}

const LLAMA_LABEL_PREFIX: &str = "io.amicu.llama.";
const SIDECAR_LABEL_PREFIX: &str = "io.amicu.telemetry.";

/// The sidecar label for the instance `main_label`:
/// `io.amicu.llama.<rest>` becomes `io.amicu.telemetry.<rest>`.
#[must_use]
pub fn sidecar_label(main_label: &str) -> String {
    match main_label.strip_prefix(LLAMA_LABEL_PREFIX) {
        Some(rest) => format!("{SIDECAR_LABEL_PREFIX}{rest}"),
        None => format!("{main_label}.telemetry"),
    }
}

/// The instance label a sidecar label belongs to, if it is one.
#[must_use]
pub fn sidecar_main_label(label: &str) -> Option<String> {
    label
        .strip_prefix(SIDECAR_LABEL_PREFIX)
        .filter(|rest| !rest.is_empty())
        .map(|rest| format!("{LLAMA_LABEL_PREFIX}{rest}"))
}

/// The real supervisor: a [`ServiceManager`] runs the process, a [`HealthProbe`]
/// checks endpoint readiness, and the two combine into the health tri-state
/// surfaced through [`status`](ProcessSupervisor::status).
pub struct ServiceSupervisor<M, P> {
    manager: M,
    probe: P,
    runner: Box<dyn CommandRunner>,
    ready: ReadyWait,
    drain: Duration,
    sidecar: Option<Arc<dyn SidecarPlanner>>,
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
            drain: Duration::from_secs(10),
            sidecar: None,
        }
    }

    /// Start a sidecar job (the telemetry agent) beside each instance when
    /// `planner` asks for one.
    #[must_use]
    pub fn with_sidecar(mut self, planner: Option<Arc<dyn SidecarPlanner>>) -> Self {
        self.sidecar = planner;
        self
    }

    /// Where the sidecar of `main_label` keeps its definition: next to the
    /// instance's recorded definition, else where the manager would put it.
    fn sidecar_definition(
        &self,
        main_label: &str,
        main_definition: Option<&Path>,
    ) -> Option<PathBuf> {
        let label = sidecar_label(main_label);
        match main_definition {
            Some(p) => match p.extension().and_then(|e| e.to_str()) {
                Some(ext @ ("plist" | "service")) => {
                    Some(p.with_file_name(format!("{label}.{ext}")))
                }
                _ => self.manager.paths(&label).map(|p| p.definition),
            },
            None => self.manager.paths(&label).map(|p| p.definition),
        }
    }

    /// Every sidecar definition on disk: `(label, path)`. Sidecars live next
    /// to their instances, in the manager's definition directory.
    fn sidecar_definitions(&self) -> Vec<(String, PathBuf)> {
        let Some(probe) = self.manager.paths(&sidecar_label("io.amicu.llama.x")) else {
            return Vec::new();
        };
        let (Some(dir), Some(ext)) = (
            probe.definition.parent(),
            probe.definition.extension().and_then(|e| e.to_str()),
        ) else {
            return Vec::new();
        };
        let suffix = format!(".{ext}");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut found: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let label = name.strip_suffix(&suffix)?;
                sidecar_main_label(label)?;
                Some((label.to_owned(), e.path()))
            })
            .collect();
        found.sort();
        found
    }

    /// Stop the sidecar of `main_label`, if one was ever started: its
    /// definition is written before it starts and removed only after a
    /// confirmed stop, so a definition on disk is the record that it may run.
    async fn stop_sidecar(
        &self,
        main_label: &str,
        main_definition: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        let Some(definition) = self.sidecar_definition(main_label, main_definition) else {
            return Ok(());
        };
        if std::fs::symlink_metadata(&definition).is_err() {
            return Ok(());
        }
        let label = sidecar_label(main_label);
        self.manager
            .stop_recorded(&label, Some(&definition))
            .await
            .map_err(|e| {
                LlamaCppError::Supervisor(format!(
                    "stopping the telemetry agent {label} failed: {e}"
                ))
            })
    }

    /// Override how the runtime binary is searched for on `PATH` (tests).
    #[must_use]
    pub fn with_runner(mut self, runner: Box<dyn CommandRunner>) -> Self {
        self.runner = runner;
        self
    }

    /// Override how long an update waits for the old instance to stop
    /// answering before starting the new one (zero skips the wait).
    #[must_use]
    pub fn with_drain(mut self, drain: Duration) -> Self {
        self.drain = drain;
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

    /// Wait (bounded) for a previous instance to stop answering, so the new
    /// one's readiness probe cannot hit the dying old server. If something is
    /// still answering at the deadline it is not ours to stop: fail rather than
    /// let that process make a dead service look ready.
    async fn wait_released(&self, label: &str, endpoint: &str) -> Result<(), LlamaCppError> {
        let deadline = tokio::time::Instant::now() + self.drain;
        while self.probe.is_ready(endpoint).await {
            if tokio::time::Instant::now() >= deadline {
                let port = endpoint.rsplit(':').next().unwrap_or(endpoint);
                return Err(LlamaCppError::Supervisor(format!(
                    "port {port} is already serving /health; stop whatever is listening there \
                     (not released by `{label}`)"
                )));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(())
    }

    /// Block until the endpoint is ready. On a crash or timeout the service is
    /// stopped (fail closed) and the error names where its logs are.
    ///
    /// The service counts as exited when the manager reports `Failed`, or
    /// `Inactive` on two consecutive reads: a launchd job may briefly show no
    /// PID right after load. (A crashing systemd unit restarts via
    /// `Restart=on-failure` and reads `Failed` once its start limit is hit.)
    async fn wait_ready(&self, label: &str, endpoint: &str) -> Result<(), LlamaCppError> {
        if self.ready.timeout.is_zero() {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + self.ready.timeout;
        let mut inactive_reads = 0u32;
        let (reason, exited) = loop {
            if self.probe.is_ready(endpoint).await {
                return Ok(());
            }
            match self.manager.state(label) {
                Ok(ServiceState::Failed) => {
                    break ("the service exited abnormally".to_owned(), true);
                }
                Ok(ServiceState::Inactive) => {
                    inactive_reads += 1;
                    if inactive_reads >= 2 {
                        break ("the service exited".to_owned(), true);
                    }
                }
                _ => inactive_reads = 0,
            }
            if tokio::time::Instant::now() >= deadline {
                break (
                    format!(
                        "{endpoint} was not ready within {}s",
                        self.ready.timeout.as_secs()
                    ),
                    false,
                );
            }
            tokio::time::sleep(self.ready.interval).await;
        };
        let logs = self.manager.log_location(label);
        let stopped = self.manager.stop(label).await;
        Err(LlamaCppError::Supervisor(match stopped {
            Ok(()) => format!("{reason}; the service was stopped; see logs at {logs}"),
            // An exited (collected) unit is "not loaded": its stop error is expected.
            Err(_) if exited => format!("{reason}; see logs at {logs}"),
            Err(e) => format!(
                "{reason}; stopping it FAILED ({e}): stop {label} manually; see logs at {logs}"
            ),
        }))
    }
}

/// How to install a runtime binary, for the not-found error.
fn install_hint(program: &str) -> &'static str {
    match program {
        "llama-server" if cfg!(windows) => {
            "`winget install llama.cpp`, or download a release zip from github.com/ggml-org/llama.cpp"
        }
        "llama-server" => "`brew install llama.cpp` (macOS) or see github.com/ggml-org/llama.cpp",
        "vllm" => "`pip install vllm`",
        _ => "your package manager",
    }
}

/// The launchd (macOS) supervisor: a LaunchAgent plist driven by `launchctl`.
pub type LaunchdSupervisor = ServiceSupervisor<LaunchdManager<HostExec>, HttpHealthProbe>;

/// The systemd (Linux) supervisor: a persistent `--user` unit driven by `systemctl`.
pub type SystemdSupervisor = ServiceSupervisor<SystemdManager<HostExec>, HttpHealthProbe>;

/// The Task Scheduler (Windows) supervisor: a per-user logon task driven by `schtasks`.
pub type TaskSchedulerSupervisor =
    ServiceSupervisor<TaskSchedulerManager<HostExec>, HttpHealthProbe>;

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

impl TaskSchedulerSupervisor {
    /// Host-wired Task Scheduler supervisor.
    #[must_use]
    pub fn host() -> Self {
        Self::new(TaskSchedulerManager::host(), HttpHealthProbe::new())
    }
}

/// The supervisor for the host's service manager: launchd on macOS, Task
/// Scheduler on Windows, systemd elsewhere.
#[must_use]
pub fn host_supervisor() -> Box<dyn ProcessSupervisor> {
    #[cfg(target_os = "macos")]
    {
        Box::new(LaunchdSupervisor::host())
    }
    #[cfg(windows)]
    {
        Box::new(TaskSchedulerSupervisor::host())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Box::new(SystemdSupervisor::host())
    }
}

/// [`host_supervisor`] that also starts `sidecar`'s job (the telemetry
/// agent) beside each instance. Stops cover the sidecar either way.
///
/// On Windows no sidecar is started yet. TODO: start the agent as a second
/// per-user Task Scheduler task (the agent's consent, data-dir and hardware
/// probe paths are Unix-only today); stops already cover a sidecar there.
#[must_use]
pub fn host_supervisor_with_sidecar(
    sidecar: Option<Arc<dyn SidecarPlanner>>,
) -> Box<dyn ProcessSupervisor> {
    #[cfg(target_os = "macos")]
    {
        Box::new(LaunchdSupervisor::host().with_sidecar(sidecar))
    }
    #[cfg(windows)]
    {
        let _ = sidecar;
        Box::new(TaskSchedulerSupervisor::host())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Box::new(SystemdSupervisor::host().with_sidecar(sidecar))
    }
}

fn endpoint_for(plan: &LaunchPlan) -> String {
    let port = match &plan.settings {
        Runtime::LlamaCpp(s) => s.port,
        Runtime::Vllm(s) => s.port,
    };
    format!("http://127.0.0.1:{port}")
}

/// The legacy launchd label / systemd unit: not scoped to a project, so two
/// projects with a deployment of the same name collide. Only instances
/// started before project-scoped labels carry it; new ones use
/// [`project_host_label`].
#[must_use]
pub fn host_label(deployment: &str) -> String {
    format!("io.amicu.llama.{deployment}")
}

/// The launchd label / systemd unit for `deployment` in the project
/// identified by `project_id`. Deterministic, so a stop handle can be derived
/// from state alone, and distinct per project.
#[must_use]
pub fn project_host_label(project_id: &str, deployment: &str) -> String {
    format!("io.amicu.llama.{project_id}.{deployment}")
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
        // letting one silently shadow the other.
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
        let label = plan
            .service_label
            .clone()
            .unwrap_or_else(|| host_label(plan.deployment.as_str()));
        let spec = ServiceSpec {
            label: label.clone(),
            argv: self.resolve_program(argv)?,
            env,
        };
        let endpoint = endpoint_for(plan);
        // Update: a previous sidecar is stopped for good (this start decides
        // afresh whether to run one), then the previous instance is released
        // and its port allowed to go quiet.
        let definition = self.manager.paths(&label).map(|p| p.definition);
        self.stop_sidecar(&label, definition.as_deref()).await?;
        self.manager.release(&label).await?;
        if !self.drain.is_zero() {
            self.wait_released(&label, &endpoint).await?;
        }
        self.manager.start(&spec)?;
        self.wait_ready(&label, &endpoint).await?;
        // The sidecar starts only once the instance serves, and its failure
        // never fails the deployment (`start` removes a definition it could
        // not start, so nothing is left behind).
        if let Some(sidecar) = self.sidecar.as_ref().and_then(|p| p.sidecar(plan, &label))
            && let Err(e) = self.manager.start(&sidecar)
        {
            let _ = writeln!(
                std::io::stderr(),
                "warning: the telemetry agent for {label} did not start ({e}); the deployment \
                 itself is running"
            );
        }
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
        self.stop_sidecar(&handle.id, None).await?;
        self.manager.stop(&handle.id).await
    }

    async fn stop_recorded(
        &self,
        handle: &RuntimeHandle,
        definition: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        self.stop_sidecar(&handle.id, definition).await?;
        self.manager.stop_recorded(&handle.id, definition).await
    }

    async fn stop_recorded_with_env(
        &self,
        handle: &RuntimeHandle,
        definition: Option<&Path>,
        env: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        self.stop_sidecar(&handle.id, definition).await?;
        self.manager
            .stop_recorded_with_env(&handle.id, definition, env)
            .await
    }

    fn host_context(&self) -> Option<crate::service::HostContext> {
        self.manager.context()
    }

    fn boot_hint(&self) -> Option<String> {
        self.manager.boot_hint()
    }

    fn instance_owner(&self, label: &str) -> Option<u32> {
        self.manager.instance_owner(label)
    }

    fn service_paths(&self, label: &str) -> Option<ServicePaths> {
        self.manager.paths(label)
    }

    fn presence(
        &self,
        label: &str,
        domain: Option<&str>,
    ) -> Result<crate::service::Presence, LlamaCppError> {
        self.manager.presence(label, domain)
    }

    fn sidecars(&self) -> Vec<(String, crate::service::Presence)> {
        let domain = self.manager.context().and_then(|c| c.domain);
        self.sidecar_definitions()
            .into_iter()
            .map(|(label, _)| {
                let presence = self
                    .manager
                    .presence(&label, domain.as_deref())
                    .unwrap_or(crate::service::Presence::Unknown);
                (label, presence)
            })
            .collect()
    }

    async fn stop_all_sidecars(&self) -> Result<Vec<String>, LlamaCppError> {
        let mut stopped = Vec::new();
        for (label, definition) in self.sidecar_definitions() {
            self.manager
                .stop_recorded(&label, Some(&definition))
                .await
                .map_err(|e| {
                    LlamaCppError::Supervisor(format!(
                        "stopping the telemetry agent {label} failed: {e}"
                    ))
                })?;
            stopped.push(label);
        }
        Ok(stopped)
    }

    async fn start_cloud_sidecar(
        &self,
        main_label: &str,
        spec: &ServiceSpec,
    ) -> Result<(), LlamaCppError> {
        self.stop_sidecar(main_label, None).await?;
        self.manager.start(spec)
    }

    async fn stop_sidecar_of(&self, main_label: &str) -> Result<(), LlamaCppError> {
        self.stop_sidecar(main_label, None).await
    }
}
