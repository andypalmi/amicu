//! Service-manager operations behind launchd (macOS) and systemd (Linux); the
//! Windows Task Scheduler manager lives in [`crate::service_windows`].
//!
//! The [`ServiceManager`] trait is the injectable seam the supervisor drives;
//! real impls shell out through a [`CommandExec`] (itself injectable) and write
//! service definitions to disk, so unit tests stay hermetic. Secrets are passed
//! through the process environment only - never into argv or a log.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use amicu_runtime::LogOptions;

use crate::error::LlamaCppError;

/// A service to define and run: its manager label, the argv to exec, and the
/// environment (resolved secrets) to inject into the process.
#[derive(Clone)]
pub struct ServiceSpec {
    /// Manager label / unit name, e.g. `io.amicu.llama.<deployment>`.
    pub label: String,
    /// Program plus arguments; `argv[0]` is the program.
    pub argv: Vec<String>,
    /// Environment injected into the process (secret name/value pairs).
    pub env: Vec<(String, String)>,
}

/// Redacted: env values may be secrets (the endpoint API key), so `Debug`
/// shows the variable names only.
impl std::fmt::Debug for ServiceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceSpec")
            .field("label", &self.label)
            .field("argv", &self.argv)
            .field(
                "env",
                &self.env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The coarse lifecycle state a service manager reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Loaded and running.
    Active,
    /// Not loaded, or stopped cleanly.
    Inactive,
    /// Loaded but exited abnormally / crashed.
    Failed,
}

/// What a read-only probe of a recorded host instance found. `Down` is only
/// ever returned when the service manager CONFIRMS the instance is not
/// running; anything uncertain is `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Loaded and running.
    Running,
    /// Confirmed not loaded, or loaded and not running.
    Down,
    /// Could not be told (unknown state, no recorded domain, ...).
    Unknown,
}

/// Which host service manager runs amicu's instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBackend {
    /// launchd (macOS).
    Launchd,
    /// systemd user units (Linux).
    Systemd,
    /// Per-user Task Scheduler tasks (Windows).
    TaskScheduler,
}

/// Who the host service manager acts as. Recorded with every started
/// instance so a later stop can refuse when it would act as someone else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostContext {
    pub backend: HostBackend,
    /// The effective uid the manager acts for (launchd, systemd; `None` on
    /// Windows, which has no uid).
    pub uid: Option<u32>,
    /// Task Scheduler only: the SID of the user the tasks run as.
    pub sid: Option<String>,
    /// launchd only: the domain jobs are bootstrapped into (`gui/<uid>` or
    /// `user/<uid>`).
    pub domain: Option<String>,
}

/// The files a service leaves on the host: its definition (which holds the
/// injected environment, so it is a secret-bearing file) and its log files.
/// Recorded in state before the service starts so a later stop or purge does
/// not depend on recomputing them from `$HOME` / the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePaths {
    /// The launchd plist / systemd unit file.
    pub definition: PathBuf,
    /// Log files the manager writes (empty for systemd: the journal).
    pub logs: Vec<PathBuf>,
    /// systemd only: the 0600 `EnvironmentFile` holding the injected
    /// environment (secrets); `None` for launchd (they are in the plist).
    pub env: Option<PathBuf>,
}

/// Load/start, query, stop, and read logs for a supervised service.
#[async_trait]
pub trait ServiceManager: Send + Sync {
    /// Where this manager keeps `label`'s definition and logs, if it can tell.
    fn paths(&self, _label: &str) -> Option<ServicePaths> {
        None
    }
    /// The backend, uid and domain this manager acts as, if it can tell.
    fn context(&self) -> Option<HostContext> {
        None
    }
    /// The uid that owns the definition of an existing instance (a launchd
    /// plist), if it can be told. Used to refuse stopping someone else's job.
    fn instance_owner(&self, _label: &str) -> Option<u32> {
        None
    }
    /// Best-effort stop of any previous instance with this label, so an update
    /// can start the new one (a loaded launchd job would keep the old
    /// definition; a running systemd unit would not pick up the new one).
    /// launchd waits for the job to unload (bootout is asynchronous); launchd
    /// and systemd ignore errors (normally there is nothing to release, and
    /// `start` reports a job that lingers). An `Err` (Task Scheduler: the old
    /// instance is not confirmed gone) aborts the start.
    async fn release(&self, _label: &str) -> Result<(), LlamaCppError> {
        Ok(())
    }
    /// Read-only probe of an instance recorded as `label` (and, for launchd,
    /// started in `domain`). Never acts as a different domain than recorded.
    /// `Err` means the manager could not answer.
    fn presence(&self, _label: &str, _domain: Option<&str>) -> Result<Presence, LlamaCppError> {
        Ok(Presence::Unknown)
    }
    /// Write the service definition and start it.
    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError>;
    /// The service's current lifecycle state.
    fn state(&self, label: &str) -> Result<ServiceState, LlamaCppError>;
    /// Stop and unload the service.
    async fn stop(&self, label: &str) -> Result<(), LlamaCppError>;
    /// [`stop`](Self::stop), deleting the service definition at `definition`
    /// (the path recorded in state when it was written) instead of the one
    /// recomputed from `$HOME`. `None`: recompute (legacy records).
    async fn stop_recorded(
        &self,
        label: &str,
        _definition: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        self.stop(label).await
    }
    /// [`stop_recorded`](Self::stop_recorded), also deleting the secrets
    /// environment file recorded in state at `env` (systemd). `None`: derive it
    /// (records that predate the field).
    async fn stop_recorded_with_env(
        &self,
        label: &str,
        definition: Option<&Path>,
        _env: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        self.stop_recorded(label, definition).await
    }
    /// A one-line advisory to show after a successful start, when this host
    /// would not bring the service back after a reboot as is (systemd user
    /// lingering off). Read-only; `None` when there is nothing to say or it
    /// cannot be told.
    fn boot_hint(&self) -> Option<String> {
        None
    }
    /// Read the service's captured log lines.
    fn logs(&self, label: &str, options: LogOptions) -> Result<Vec<String>, LlamaCppError>;
    /// Where a human finds the service's logs (file paths or a command), for
    /// error messages. Never contains the environment.
    fn log_location(&self, label: &str) -> String;
}

/// The captured result of running a command.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// Whether the process exited zero.
    pub success: bool,
    /// Combined stdout.
    pub stdout: String,
    /// Combined stderr.
    pub stderr: String,
    /// The exit code, when the process exited normally.
    pub code: Option<i32>,
}

/// Runs external commands. Injectable so managers are testable without a real
/// `launchctl`/`systemctl`/`journalctl`.
pub trait CommandExec: Send + Sync {
    /// Run `program args...`, capturing its output.
    fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, LlamaCppError>;
}

static ALLOW_SERVICE_COMMANDS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Opt in to running real service commands (`launchctl`, `systemctl`,
/// `loginctl`, `journalctl`, and on Windows `schtasks`, `powershell`,
/// `taskkill`, `whoami`, `icacls`). Off by default, so any test in any crate that
/// reaches a real [`HostExec`] panics instead of touching the developer's
/// services, regardless of test order. Only the `amicu` binary calls this.
pub fn allow_service_commands() {
    ALLOW_SERVICE_COMMANDS.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The real executor: spawns the process and captures its output.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostExec;

impl CommandExec for HostExec {
    fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, LlamaCppError> {
        assert!(
            ALLOW_SERVICE_COMMANDS.load(std::sync::atomic::Ordering::SeqCst)
                || !matches!(
                    program,
                    "launchctl"
                        | "systemctl"
                        | "loginctl"
                        | "journalctl"
                        | "schtasks"
                        | "powershell"
                        | "taskkill"
                        | "whoami"
                        | "icacls"
                ),
            "refusing to run the real `{program} {}`: service commands are only enabled in the \
             amicu binary; inject a fake service manager",
            args.join(" ")
        );
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|e| LlamaCppError::Supervisor(format!("{program}: {e}")))?;
        Ok(ExecOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code(),
        })
    }
}

/// The effective uid, from `id -u`.
fn effective_uid(exec: &dyn CommandExec) -> Result<u32, LlamaCppError> {
    let out = exec.exec("/usr/bin/id", &["-u"])?;
    out.stdout.trim().parse().map_err(|_| {
        LlamaCppError::Supervisor(format!(
            "cannot determine the effective uid from `id -u`: {:?}",
            out.stdout.trim()
        ))
    })
}

pub(crate) fn tail<T>(mut lines: Vec<T>, options: LogOptions) -> Vec<T> {
    if let Some(n) = options.tail {
        let keep = (n as usize).min(lines.len());
        lines = lines.split_off(lines.len() - keep);
    }
    lines
}

// ---- launchd ----------------------------------------------------------------

/// launchd-backed manager: a per-instance LaunchAgent plist, loaded/unloaded via
/// `launchctl`, with stdout/stderr captured to files under `log_dir`.
#[derive(Debug, Clone)]
pub struct LaunchdManager<E> {
    exec: E,
    agents_dir: PathBuf,
    log_dir: PathBuf,
    /// How long to wait for launchd to finish an (asynchronous) `bootout`.
    settle_timeout: Duration,
}

/// Default bound on waiting for a `bootout` to complete. llama-server frees
/// its model on SIGTERM, which can take several seconds for a large one.
const LAUNCHD_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a stop wait must last before the one-time "waiting" note prints;
/// `bootout` is async, so most stops finish well inside this.
pub(crate) const WAIT_NOTE_AFTER: Duration = Duration::from_secs(1);

impl<E: CommandExec> LaunchdManager<E> {
    /// Build over an executor and the LaunchAgents / log directories.
    pub fn new(exec: E, agents_dir: impl Into<PathBuf>, log_dir: impl Into<PathBuf>) -> Self {
        Self {
            exec,
            agents_dir: agents_dir.into(),
            log_dir: log_dir.into(),
            settle_timeout: LAUNCHD_SETTLE_TIMEOUT,
        }
    }

    /// Bound how long to wait for `bootout` to finish (tests use a short one).
    #[must_use]
    pub fn with_settle_timeout(mut self, timeout: Duration) -> Self {
        self.settle_timeout = timeout;
        self
    }

    /// `launchctl bootout` returns before the job is gone: the job stays
    /// listed (state `SIGTERMed`, or a transient `spawn scheduled` / `exited`
    /// / throttled one) until its process has exited. Poll `launchctl print`
    /// with a short backoff, asynchronously, until the job is unloaded. Any
    /// state in which launchd still lists the job counts as loaded, never as
    /// stopped. Prints a one-line progress note (once) when the wait has lasted over a second.
    /// `Ok(true)` once gone, `Ok(false)` if still loaded after the timeout.
    async fn wait_unloaded(&self, domain: &str, label: &str) -> Result<bool, LlamaCppError> {
        let started = Instant::now();
        let mut delay = Duration::from_millis(50);
        let mut announced = false;
        loop {
            if !self.is_loaded(domain, label)? {
                return Ok(true);
            }
            if !announced && started.elapsed() >= WAIT_NOTE_AFTER {
                announced = true;
                let _ = writeln!(
                    std::io::stderr(),
                    "waiting for {label} to stop (up to {}s)...",
                    self.settle_timeout.as_secs()
                );
            }
            let left = self.settle_timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Ok(false);
            }
            tokio::time::sleep(delay.min(left)).await;
            delay = (delay * 2).min(Duration::from_millis(500));
        }
    }

    fn plist_path(&self, label: &str) -> PathBuf {
        self.agents_dir.join(format!("{label}.plist"))
    }

    /// The launchd domain jobs live in: `gui/<uid>` when the user has a GUI
    /// session, else `user/<uid>`.
    fn domain(&self) -> Result<(u32, String), LlamaCppError> {
        let uid = effective_uid(&self.exec)?;
        let gui = format!("gui/{uid}");
        if self.exec.exec("launchctl", &["print", &gui])?.success {
            Ok((uid, gui))
        } else {
            Ok((uid, format!("user/{uid}")))
        }
    }

    /// Whether launchd currently has the job loaded in `domain`.
    fn is_loaded(&self, domain: &str, label: &str) -> Result<bool, LlamaCppError> {
        let target = format!("{domain}/{label}");
        let out = self.exec.exec("launchctl", &["print", &target])?;
        if out.success {
            Ok(true)
        } else if out.code == Some(113) {
            // 113: "Could not find service" - the job is not loaded.
            Ok(false)
        } else {
            Err(LlamaCppError::Supervisor(format!(
                "launchctl print {target} failed unexpectedly (exit {:?}): {}",
                out.code,
                out.stderr.trim()
            )))
        }
    }

    fn stdout_path(&self, label: &str) -> PathBuf {
        self.log_dir.join(format!("{label}.out.log"))
    }

    fn stderr_path(&self, label: &str) -> PathBuf {
        self.log_dir.join(format!("{label}.err.log"))
    }
}

/// The default host launchd manager, rooted at the user's `~/Library`.
impl LaunchdManager<HostExec> {
    /// Construct with host defaults (`~/Library/LaunchAgents`, `~/Library/Logs/amicu`).
    #[must_use]
    pub fn host() -> Self {
        // Unset HOME leaves the dirs empty; `start` then refuses to write.
        let home = home_dir().unwrap_or_default();
        Self::new(
            HostExec,
            home.join("Library/LaunchAgents"),
            home.join("Library/Logs/amicu"),
        )
    }
}

/// The job's own `state = ...` value from `launchctl print` output: the line
/// at the job's top level, not the same key nested in a sub-block (an
/// endpoint, a dispatch-state entry) that says nothing about the job.
fn job_state(print_output: &str) -> Option<&str> {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut body: Vec<&str> = print_output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    // The `<domain>/<label> = {` header and its closing brace are not the job's
    // own properties.
    if body.first().is_some_and(|l| l.trim_end().ends_with('{')) {
        body.remove(0);
    }
    body.retain(|l| l.trim() != "}");
    let top = body.iter().map(|l| indent(l)).min()?;
    body.iter()
        .filter(|l| indent(l) == top)
        .find_map(|l| l.trim().strip_prefix("state = "))
}

/// What a listed job's top-level state says about whether it is running.
/// `not running`, `exited`, and `SIGTERMed`/`SIGKILLed` (right after a
/// `bootout` the job stays listed while its process is torn down) are down.
/// `spawn scheduled` / `waiting` / throttled mean launchd is about to (re)start
/// the job: not running now, but not confirmed down either, so `Unknown`
/// (never reported stopped, so nothing is dropped and no orphan can appear).
fn job_presence(print_output: &str) -> Presence {
    match job_state(print_output) {
        Some("not running" | "exited" | "SIGTERMed" | "SIGKILLed") => Presence::Down,
        Some(s) if s == "spawn scheduled" || s == "waiting" || s.contains("throttl") => {
            Presence::Unknown
        }
        _ => Presence::Running,
    }
}

#[async_trait]
impl<E: CommandExec> ServiceManager for LaunchdManager<E> {
    fn paths(&self, label: &str) -> Option<ServicePaths> {
        Some(ServicePaths {
            definition: self.plist_path(label),
            logs: vec![self.stdout_path(label), self.stderr_path(label)],
            env: None,
        })
    }

    fn instance_owner(&self, label: &str) -> Option<u32> {
        #[cfg(unix)]
        {
            std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(self.plist_path(label)).ok()?)
                .into()
        }
        #[cfg(not(unix))]
        {
            let _ = label;
            None
        }
    }

    fn context(&self) -> Option<HostContext> {
        let (uid, domain) = self.domain().ok()?;
        Some(HostContext {
            backend: HostBackend::Launchd,
            uid: Some(uid),
            sid: None,
            domain: Some(domain),
        })
    }

    fn presence(&self, label: &str, domain: Option<&str>) -> Result<Presence, LlamaCppError> {
        // Without the domain the job was started in, `launchctl` could answer
        // for a different one: do not guess.
        let Some(domain) = domain else {
            return Ok(Presence::Unknown);
        };
        let target = format!("{domain}/{label}");
        let out = self.exec.exec("launchctl", &["print", &target])?;
        if out.success {
            return Ok(job_presence(&out.stdout));
        }
        if out.code == Some(113) {
            // 113: "Could not find service".
            return Ok(Presence::Down);
        }
        Err(LlamaCppError::Supervisor(format!(
            "launchctl print {target} failed unexpectedly (exit {:?}): {}",
            out.code,
            out.stderr.trim()
        )))
    }

    async fn release(&self, label: &str) -> Result<(), LlamaCppError> {
        let domain = self.domain().ok().map(|(_, d)| d);
        if let Some(domain) = &domain {
            let _ = self
                .exec
                .exec("launchctl", &["bootout", &format!("{domain}/{label}")]);
            // Bootout is asynchronous: wait for the label to be released so the
            // next bootstrap does not race it (error 5). Best effort: if the
            // job lingers, fall through to remove and let `start` report it.
            if matches!(self.wait_unloaded(domain, label).await, Ok(true)) {
                return Ok(());
            }
        }
        // By label too, even when the job is loaded under another name.
        let _ = self.exec.exec("launchctl", &["remove", label]);
        if let Some(domain) = &domain {
            let _ = self.wait_unloaded(domain, label).await;
        }
        Ok(())
    }

    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError> {
        require_absolute(&self.agents_dir)?;
        // Everything that can fail before the secret is on disk happens first,
        // so a failure here never leaves the plist behind.
        ensure_dir(&self.log_dir)?;
        write_private(
            &self.plist_path(&spec.label),
            &self.agents_dir,
            plist_xml(
                spec,
                &self.stdout_path(&spec.label),
                &self.stderr_path(&spec.label),
            )
            .as_bytes(),
        )?;
        let plist = self.plist_path(&spec.label);
        let (_, domain) = self.domain().inspect_err(|_| remove_env_file(&plist))?;
        let out = match self.exec.exec(
            "launchctl",
            &["bootstrap", &domain, &plist.to_string_lossy()],
        ) {
            Ok(out) => out,
            Err(e) => {
                remove_env_file(&plist);
                return Err(e);
            }
        };
        if !out.success {
            // The plist holds the environment (secrets): never leave it behind
            // for a service that did not start.
            remove_env_file(&plist);
            return Err(LlamaCppError::Supervisor(format!(
                "launchctl bootstrap failed: {}",
                out.stderr.trim()
            )));
        }
        Ok(())
    }

    fn state(&self, label: &str) -> Result<ServiceState, LlamaCppError> {
        let out = self.exec.exec("launchctl", &["list", label])?;
        if !out.success {
            // Not loaded.
            return Ok(ServiceState::Inactive);
        }
        if out.stdout.contains("\"PID\"") {
            return Ok(ServiceState::Active);
        }
        let crashed = out
            .stdout
            .lines()
            .find_map(|l| l.trim().strip_prefix("\"LastExitStatus\" = "))
            .and_then(|v| v.trim_end_matches(';').trim().parse::<i64>().ok())
            .is_some_and(|code| code != 0);
        Ok(if crashed {
            ServiceState::Failed
        } else {
            ServiceState::Inactive
        })
    }

    async fn stop(&self, label: &str) -> Result<(), LlamaCppError> {
        self.stop_recorded(label, None).await
    }

    async fn stop_recorded(
        &self,
        label: &str,
        definition: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        let plist = definition.map_or_else(|| self.plist_path(label), Path::to_path_buf);
        let (_, domain) = self.domain()?;
        let target = format!("{domain}/{label}");
        // `bootout` can exit 0 while the job stays loaded ("Boot-out failed:
        // 5: Input/output error"), so never trust its status: confirm with
        // `launchctl print` that the job is gone.
        let out = self.exec.exec("launchctl", &["bootout", &target])?;
        let mut detail = out.stderr.trim().to_owned();
        // Bootout is asynchronous: the job stays listed until its process has
        // exited, so wait for it rather than checking once.
        if !self.wait_unloaded(&domain, label).await? {
            // Fall back to removing by label, as `release` does.
            let out = self.exec.exec("launchctl", &["remove", label])?;
            if !out.stderr.trim().is_empty() {
                out.stderr.trim().clone_into(&mut detail);
            }
            if !self.wait_unloaded(&domain, label).await? {
                return Err(LlamaCppError::Supervisor(format!(
                    "launchd job {target} is still loaded after bootout and remove: {detail}"
                )));
            }
        }
        // Confirmed gone. Remove the per-instance plist so its
        // EnvironmentVariables don't linger on disk; a missing file is fine,
        // any other error is surfaced.
        match std::fs::remove_file(&plist) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(LlamaCppError::Supervisor(format!(
                    "remove {}: {e}",
                    plist.display()
                )));
            }
        }
        Ok(())
    }

    fn logs(&self, label: &str, options: LogOptions) -> Result<Vec<String>, LlamaCppError> {
        let text = std::fs::read_to_string(self.stdout_path(label)).unwrap_or_default();
        let lines = text.lines().map(str::to_owned).collect();
        Ok(tail(lines, options))
    }

    fn log_location(&self, label: &str) -> String {
        format!(
            "{} and {}",
            self.stdout_path(label).display(),
            self.stderr_path(label).display()
        )
    }
}

/// Render the LaunchAgent plist for `spec`.
fn plist_xml(spec: &ServiceSpec, stdout: &Path, stderr: &Path) -> String {
    use std::fmt::Write as _;
    let mut args = String::new();
    for arg in &spec.argv {
        let _ = writeln!(args, "    <string>{}</string>", xml_escape(arg));
    }
    let mut env = String::new();
    if !spec.env.is_empty() {
        env.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (k, v) in &spec.env {
            let _ = writeln!(
                env,
                "    <key>{}</key><string>{}</string>",
                xml_escape(k),
                xml_escape(v)
            );
        }
        env.push_str("  </dict>\n");
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n\
  <key>StandardOutPath</key><string>{out}</string>\n\
  <key>StandardErrorPath</key><string>{err}</string>\n\
  <key>RunAtLoad</key><true/>\n\
{env}</dict>\n</plist>\n",
        label = xml_escape(&spec.label),
        out = xml_escape(&stdout.to_string_lossy()),
        err = xml_escape(&stderr.to_string_lossy()),
    )
}

pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---- systemd ----------------------------------------------------------------

/// systemd-backed manager: a persistent `--user` unit file written to
/// `~/.config/systemd/user`, enabled (`WantedBy=default.target`) and started
/// with `systemctl --user enable --now`, queried with `systemctl --user`, logs
/// read via `journalctl --user`.
///
/// This matches the launchd plist (`RunAtLoad`): a deployment `apply` created
/// comes back after a reboot, and `Restart=on-failure` restarts a crashed
/// server. Without `loginctl enable-linger` a user unit starts at the user's
/// first login rather than at boot ([`ServiceManager::boot_hint`] reports
/// that); amicu never enables lingering itself.
///
/// Env (including secrets) goes in a per-unit 0600 `EnvironmentFile` under the
/// persistent env dir (not tmpfs: it must exist again after a reboot), so the
/// unit file itself holds no secrets and values never appear on any argv. Stop
/// disables and stops the unit and, only once that is confirmed, removes both
/// files.
#[derive(Debug, Clone)]
pub struct SystemdManager<E> {
    exec: E,
    unit_dir: PathBuf,
    env_dir: PathBuf,
    /// Where transient-era units kept their env file (`$XDG_RUNTIME_DIR/amicu`,
    /// tmpfs); cleaned up best effort on stop.
    legacy_env_dir: Option<PathBuf>,
}

impl<E: CommandExec> SystemdManager<E> {
    /// Build over an executor, the directory holding unit files, and the
    /// directory holding per-unit env files.
    pub fn new(exec: E, unit_dir: impl Into<PathBuf>, env_dir: impl Into<PathBuf>) -> Self {
        Self {
            exec,
            unit_dir: unit_dir.into(),
            env_dir: env_dir.into(),
            legacy_env_dir: None,
        }
    }

    /// Also clean up a transient-era env file in `dir` when stopping.
    #[must_use]
    pub fn with_legacy_env_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.legacy_env_dir = Some(dir.into());
        self
    }

    fn unit_path(&self, label: &str) -> PathBuf {
        self.unit_dir.join(format!("{label}.service"))
    }

    fn env_path(&self, label: &str) -> PathBuf {
        self.env_dir.join(format!("{label}.env"))
    }

    /// Best-effort `systemctl --user daemon-reload`, so systemd forgets a
    /// removed unit file / reads a rewritten one.
    fn daemon_reload(&self) {
        let _ = self.exec.exec("systemctl", &["--user", "daemon-reload"]);
    }

    /// Undo a start that did not complete: disable and stop whatever got
    /// loaded and, only once systemd confirms it is not running (the disable
    /// succeeded or the unit is not found), remove both files (the env file
    /// holds secrets). Returns the error to report: `cause`, or one saying the
    /// unit may still be loaded, in which case the files are kept.
    fn abandon(&self, label: &str, cause: &str) -> LlamaCppError {
        let unit = format!("{label}.service");
        let disabled = self
            .exec
            .exec("systemctl", &["--user", "disable", "--now", &unit])
            .is_ok_and(|o| o.success);
        if !disabled && !self.not_found(label).unwrap_or(false) {
            return LlamaCppError::Supervisor(format!(
                "{cause}; the unit {unit} may still be loaded, so its files were kept: stop it \
                 with `systemctl --user disable --now {unit}`"
            ));
        }
        remove_env_file(&self.unit_path(label));
        remove_env_file(&self.env_path(label));
        self.daemon_reload();
        LlamaCppError::Supervisor(cause.to_owned())
    }

    /// The `EnvironmentFile=` an installed unit names, accepted only when it
    /// is exactly this unit's own env file (`<label>.env`, clean absolute).
    fn env_file_named_by(unit_file: &Path, label: &str) -> Option<PathBuf> {
        let text = std::fs::read_to_string(unit_file).ok()?;
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix("EnvironmentFile="))?;
        let path = PathBuf::from(line.replace("%%", "%"));
        let clean = path.is_absolute()
            && !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        // Also in the directory amicu writes env files to, so a tampered unit
        // can never point the delete at an unrelated `<label>.env`.
        let in_dir = path
            .parent()
            .is_some_and(|p| p.ends_with(".local/state/amicu/env"));
        (clean
            && in_dir
            && path.file_name().and_then(|n| n.to_str()) == Some(&format!("{label}.env")))
        .then_some(path)
    }

    /// Whether systemd confirms it has no unit called `label` loaded
    /// (checked by property, not by parsing stderr).
    fn not_found(&self, label: &str) -> Result<bool, LlamaCppError> {
        let load = self.exec.exec(
            "systemctl",
            &["--user", "show", "-p", "LoadState", "--value", label],
        )?;
        Ok(load.success && load.stdout.trim() == "not-found")
    }
}

impl SystemdManager<HostExec> {
    /// Construct with the host executor: unit files in
    /// `~/.config/systemd/user`, env files in `~/.local/state/amicu/env`
    /// (fixed locations, so a recorded path can be validated against them).
    #[must_use]
    pub fn host() -> Self {
        // Unset HOME leaves the dirs empty; `start` then refuses to write.
        let home = home_dir().unwrap_or_default();
        let manager = Self::new(
            HostExec,
            home.join(".config/systemd/user"),
            home.join(".local/state/amicu/env"),
        );
        match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
            Some(run) => manager.with_legacy_env_dir(PathBuf::from(run).join("amicu")),
            None => manager,
        }
    }
}

/// Render `spec.env` as a systemd `EnvironmentFile`: `KEY="value"` lines with
/// `\` and `"` escaped (and `$`/backtick are literal in EnvironmentFile).
/// Newlines are written raw: inside double quotes systemd follows POSIX rules,
/// which would keep a `\n` escape as a literal backslash sequence.
/// Keys must be plain identifiers.
fn environment_file(env: &[(String, String)]) -> Result<String, LlamaCppError> {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (k, v) in env {
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(LlamaCppError::Supervisor(format!(
                "invalid environment variable name '{k}'"
            )));
        }
        let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = writeln!(out, "{k}=\"{escaped}\"");
    }
    Ok(out)
}

/// One `ExecStart=` word: double-quoted, with the characters systemd would
/// otherwise interpret escaped (`\`, `"`, newline, and the `%`/`$` specifier
/// and variable introducers).
fn exec_word(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render the persistent user unit for `spec`. `env_file` is the 0600
/// `EnvironmentFile` holding the injected environment, when there is one.
fn unit_file(spec: &ServiceSpec, env_file: Option<&Path>) -> String {
    use std::fmt::Write as _;
    let exec = spec
        .argv
        .iter()
        .map(|a| exec_word(a))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = format!(
        "[Unit]\nDescription=amicu {}\n\n[Service]\nType=simple\nExecStart={exec}\n",
        spec.label
    );
    if let Some(path) = env_file {
        // Specifiers (`%`) apply here too; the path is amicu's own.
        let _ = writeln!(
            out,
            "EnvironmentFile={}",
            path.to_string_lossy().replace('%', "%%")
        );
    }
    out.push_str("Restart=on-failure\n\n[Install]\nWantedBy=default.target\n");
    out
}

/// A label becomes a file name and a unit name: refuse anything that is not
/// one plain component, so a tampered record cannot reach another file.
pub(crate) fn check_label(label: &str) -> Result<(), LlamaCppError> {
    let bad = label.is_empty()
        || label.starts_with('.')
        || label
            .chars()
            .any(|c| c == '/' || c == '\\' || c == '%' || c.is_whitespace() || c.is_control());
    if bad {
        return Err(LlamaCppError::Supervisor(format!(
            "'{label}' is not a valid service name"
        )));
    }
    Ok(())
}

#[async_trait]
impl<E: CommandExec> ServiceManager for SystemdManager<E> {
    fn paths(&self, label: &str) -> Option<ServicePaths> {
        Some(ServicePaths {
            definition: self.unit_path(label),
            logs: Vec::new(),
            env: Some(self.env_path(label)),
        })
    }

    fn context(&self) -> Option<HostContext> {
        Some(HostContext {
            backend: HostBackend::Systemd,
            uid: Some(effective_uid(&self.exec).ok()?),
            sid: None,
            domain: None,
        })
    }

    fn boot_hint(&self) -> Option<String> {
        let uid = effective_uid(&self.exec).ok()?;
        let out = self
            .exec
            .exec("loginctl", &["show-user", &uid.to_string(), "-p", "Linger"])
            .ok()?;
        (out.success && out.stdout.trim() == "Linger=no").then(|| {
            "note: user lingering is off, so systemd starts this deployment at your next login, \
             not at boot. To have it come back after a reboot without logging in, run \
             `sudo loginctl enable-linger $USER` (amicu does not do this for you)."
                .to_owned()
        })
    }

    fn presence(&self, label: &str, _domain: Option<&str>) -> Result<Presence, LlamaCppError> {
        let out = self.exec.exec(
            "systemctl",
            &["--user", "show", "-p", "LoadState,ActiveState", label],
        )?;
        if !out.success {
            return Err(LlamaCppError::Supervisor(format!(
                "systemctl show {label} failed: {}",
                out.stderr.trim()
            )));
        }
        let prop = |name: &str| {
            out.stdout
                .lines()
                .find_map(|l| l.trim().strip_prefix(name)?.strip_prefix('='))
                .map(str::trim)
        };
        Ok(match (prop("LoadState"), prop("ActiveState")) {
            (Some("not-found"), _) | (Some(_), Some("inactive" | "failed")) => Presence::Down,
            (Some(_), Some("active" | "reloading")) => Presence::Running,
            // activating, deactivating, anything else or unparsed.
            _ => Presence::Unknown,
        })
    }

    async fn release(&self, label: &str) -> Result<(), LlamaCppError> {
        // Stop only: `start` rewrites the unit file and reloads, so an update
        // never has to remove the definition first.
        let _ = self.exec.exec("systemctl", &["--user", "stop", label]);
        Ok(())
    }

    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError> {
        if spec.argv.is_empty() {
            return Err(LlamaCppError::Supervisor("empty argv".to_owned()));
        }
        check_label(&spec.label)?;
        require_absolute(&self.unit_dir)?;
        let env_path = self.env_path(&spec.label);
        let unit_path = self.unit_path(&spec.label);
        let has_env = !spec.env.is_empty();
        // Everything that can fail before a secret is on disk happens first.
        let env_body = if has_env {
            require_absolute(&self.env_dir)?;
            Some(environment_file(&spec.env)?)
        } else {
            None
        };
        let unit_body = unit_file(spec, has_env.then_some(env_path.as_path()));
        // The env file (secrets) is written first, so the unit never refers to
        // a file that is not there yet; a failure removes whatever was written.
        if let Some(body) = &env_body {
            write_private(&env_path, &self.env_dir, body.as_bytes())?;
        }
        if let Err(e) = write_private(&unit_path, &self.unit_dir, unit_body.as_bytes()) {
            remove_env_file(&env_path);
            return Err(e);
        }
        let unit = format!("{}.service", spec.label);
        for args in [
            vec!["--user", "daemon-reload"],
            vec!["--user", "enable", "--now", unit.as_str()],
        ] {
            let out = match self.exec.exec("systemctl", &args) {
                Ok(out) => out,
                Err(e) => return Err(self.abandon(&spec.label, &e.to_string())),
            };
            if !out.success {
                return Err(self.abandon(
                    &spec.label,
                    &format!(
                        "systemctl {} failed: {}",
                        args[1..].join(" "),
                        out.stderr.trim()
                    ),
                ));
            }
        }
        Ok(())
    }

    fn state(&self, label: &str) -> Result<ServiceState, LlamaCppError> {
        let out = self
            .exec
            .exec("systemctl", &["--user", "is-active", label])?;
        Ok(match out.stdout.trim() {
            "active" | "activating" => ServiceState::Active,
            "failed" => ServiceState::Failed,
            _ => ServiceState::Inactive,
        })
    }

    async fn stop(&self, label: &str) -> Result<(), LlamaCppError> {
        self.stop_recorded(label, None).await
    }

    async fn stop_recorded(
        &self,
        label: &str,
        definition: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        self.stop_recorded_with_env(label, definition, None).await
    }

    async fn stop_recorded_with_env(
        &self,
        label: &str,
        definition: Option<&Path>,
        env: Option<&Path>,
    ) -> Result<(), LlamaCppError> {
        check_label(label)?;
        let unit = format!("{label}.service");
        // Disable and stop in one go. A unit with no unit file to disable (a
        // legacy transient unit, or one already removed) falls back to a plain
        // stop. The files stay until the stop is confirmed: a unit that may
        // still run must remain findable and restartable by amicu.
        let out = self
            .exec
            .exec("systemctl", &["--user", "disable", "--now", &unit])?;
        if !out.success {
            let stop = self.exec.exec("systemctl", &["--user", "stop", label])?;
            if !stop.success && !self.not_found(label)? {
                return Err(LlamaCppError::Supervisor(format!(
                    "systemctl stop failed: {}",
                    stop.stderr.trim()
                )));
            }
        }
        // Confirmed stopped: drop the unit file (the recorded one, when it is
        // the unit; a legacy record names the env file) and the secrets file.
        let unit_path = match definition {
            Some(p) if p.extension().is_some_and(|e| e == "service") => p.to_path_buf(),
            _ => self.unit_path(label),
        };
        // The secrets file goes FIRST and a failure to delete it is an error
        // that leaves the unit file (and so the record) in place: the record
        // must never be dropped while the secrets are still on disk. The path
        // is the recorded one, else the one the installed unit names, else the
        // computed one.
        let mut env_files: Vec<PathBuf> = Vec::new();
        if let Some(p) = env {
            env_files.push(p.to_path_buf());
        }
        if let Some(p) = Self::env_file_named_by(&unit_path, label) {
            env_files.push(p);
        }
        env_files.push(self.env_path(label));
        // A legacy (transient-era) record names the env file as its definition.
        if let Some(p) = definition.filter(|p| p.extension().is_some_and(|e| e == "env")) {
            env_files.push(p.to_path_buf());
        }
        env_files.dedup();
        for path in &env_files {
            remove_file_strict(path)?;
        }
        // Transient-era units kept it on tmpfs; best effort, as before.
        if let Some(dir) = &self.legacy_env_dir {
            remove_env_file(&dir.join(format!("{label}.env")));
        }
        remove_file_strict(&unit_path)?;
        self.daemon_reload();
        Ok(())
    }

    fn logs(&self, label: &str, options: LogOptions) -> Result<Vec<String>, LlamaCppError> {
        let unit = format!("--unit={label}");
        let mut args = vec!["--user", &unit, "--no-pager"];
        let tail_arg;
        if let Some(n) = options.tail {
            tail_arg = format!("--lines={n}");
            args.push(&tail_arg);
        }
        let out = self.exec.exec("journalctl", &args)?;
        if !out.success {
            return Err(LlamaCppError::Supervisor(format!(
                "journalctl failed: {}",
                out.stderr.trim()
            )));
        }
        Ok(out.stdout.lines().map(str::to_owned).collect())
    }

    fn log_location(&self, label: &str) -> String {
        format!("`journalctl --user --unit={label}`")
    }
}

// ---- fs helpers -----------------------------------------------------------

/// Best-effort removal of a unit's env file; a missing file is fine.
pub(crate) fn remove_env_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Remove `path`; a missing file is fine, any other error is surfaced.
pub(crate) fn remove_file_strict(path: &Path) -> Result<(), LlamaCppError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(LlamaCppError::Supervisor(format!(
            "remove {}: {e}",
            path.display()
        ))),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Refuse a relative or empty directory: it means `HOME` was unset, and secrets
/// must never be written relative to the cwd.
pub(crate) fn require_absolute(dir: &Path) -> Result<(), LlamaCppError> {
    if dir.is_absolute() {
        Ok(())
    } else {
        Err(LlamaCppError::Supervisor(
            "cannot locate a private directory for service files: HOME is not set".to_owned(),
        ))
    }
}

/// Create `dir` (and parents), the leaf owner-only (0700) on unix.
pub(crate) fn ensure_dir(dir: &Path) -> Result<(), LlamaCppError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| LlamaCppError::Supervisor(format!("create {}: {e}", dir.display())))
}

/// Write `bytes` to `path` (creating `dir`), owner-readable only - the plist and
/// env files hold secrets, so they must not be world-readable.
pub(crate) fn write_private(path: &Path, dir: &Path, bytes: &[u8]) -> Result<(), LlamaCppError> {
    use std::io::Write as _;
    ensure_dir(dir)?;
    // Never reuse an existing file or symlink (its mode/owner/target are not
    // ours): remove it, then create exclusively (`O_EXCL` also refuses to follow
    // a symlink planted in between) with 0600 so there is no readable window.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(LlamaCppError::Supervisor(format!(
                "remove {}: {e}",
                path.display()
            )));
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| LlamaCppError::Supervisor(format!("create {}: {e}", path.display())))?;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        // Never leave a partially written secrets file behind.
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(LlamaCppError::Supervisor(format!(
            "write {}: {e}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod env_file_tests {
    use super::*;
    use crate::HostExec;

    fn unit_naming(tag: &str, env: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("amicu-envnamed-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let unit = dir.join("x.service");
        std::fs::write(&unit, format!("[Service]\nEnvironmentFile={env}\n")).unwrap();
        unit
    }

    #[cfg(unix)]
    #[test]
    fn env_file_named_by_only_accepts_the_amicu_env_dir() {
        let named = SystemdManager::<HostExec>::env_file_named_by;
        let ok = "/home/u/.local/state/amicu/env/io.amicu.x.env";
        let unit = unit_naming("ok", ok);
        assert_eq!(named(&unit, "io.amicu.x"), Some(PathBuf::from(ok)));

        // Right file name, wrong directory: never an accepted delete target.
        let unit = unit_naming("elsewhere", "/home/u/important/io.amicu.x.env");
        assert_eq!(named(&unit, "io.amicu.x"), None);
        // Right directory, another unit's file name.
        let unit = unit_naming("other", "/home/u/.local/state/amicu/env/io.amicu.y.env");
        assert_eq!(named(&unit, "io.amicu.x"), None);
    }
}
