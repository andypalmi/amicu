//! Windows service manager: one per-user Task Scheduler task per instance.
//!
//! Each instance is a task `amicu\<label>` with a logon trigger for the
//! current user (an interactive token at least privilege, no stored password,
//! so no admin rights are needed), registered from an XML definition with
//! `schtasks /Create /XML`, started with `/Run` and torn down with
//! `/Change /DISABLE`, `/End` and `/Delete`.
//!
//! The task runs a generated PowerShell launcher, not `llama-server.exe`
//! directly. PowerShell rather than a `.cmd` file because the environment has
//! to be loaded from a file without it ever being parsed as code: a `.cmd`
//! `set` line expands `%`, `^` and `!` inside a secret, while the launcher
//! reads `NAME=<base64>` lines as data and sets them with .NET calls. Every
//! literal in the script is a PowerShell single-quoted string (no expansion at
//! all), and the server's arguments are passed as one pre-built Windows
//! command line, quoted by the `CommandLineToArgvW` rules, so the argv the
//! server sees is exactly the one rendered here. Secrets live only in the
//! owner-only env file, never in the task XML or the launcher.
//!
//! The launcher records the server's pid (and start time) next to the task, so
//! a stop can confirm the process is gone: `schtasks /End` is asynchronous and
//! does not reliably take the server (a child of the launcher) with it.
//!
//! Generic over [`CommandExec`] and not `cfg`-gated, so it is unit-tested on
//! every platform with a fake executor.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use amicu_runtime::LogOptions;

use crate::error::LlamaCppError;
use crate::service::{
    CommandExec, HostBackend, HostContext, HostExec, Presence, ServiceManager, ServicePaths,
    ServiceSpec, ServiceState, WAIT_NOTE_AFTER, check_label, ensure_dir, remove_env_file,
    remove_file_strict, require_absolute, tail, write_private, xml_escape,
};

/// The Task Scheduler folder amicu registers its tasks in.
pub const TASK_FOLDER: &str = "amicu";

/// Default bound on waiting for a stopped task's server process to exit.
const TASK_SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

/// `RestartOnFailure`: how many times, and how far apart, Task Scheduler
/// restarts a launcher that exited with an error (the minimum interval it
/// accepts is one minute).
const RESTART_COUNT: u32 = 3;
const RESTART_INTERVAL: &str = "PT1M";

/// Task Scheduler result codes that do not mean the last run failed: success,
/// running, has not run yet, and terminated by the user (`/End`).
const BENIGN_RESULTS: [i64; 4] = [0, 0x41301, 0x41303, 0x41306];

/// Lists every task in `\amicu\` as `TASK<TAB>name<TAB>state<TAB>result`
/// lines, then an end marker. `State` is the `ScheduledTasks` enum name
/// (`Ready`, `Running`, `Queued`, `Disabled`), which unlike `schtasks /Query`
/// output is not localized. Any failure exits non-zero, so a missing marker is
/// never read as "no tasks".
const LIST_TASKS_SCRIPT: &str = "$ErrorActionPreference = 'Stop'; \
Get-ScheduledTask | Where-Object { $_.TaskPath -eq '\\amicu\\' } | ForEach-Object { \
$i = Get-ScheduledTaskInfo -InputObject $_; \
\"TASK`t$($_.TaskName)`t$($_.State)`t$($i.LastTaskResult)\" }; 'END'";

/// One task in the `\amicu\` folder, as listed by [`list_tasks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    /// The task name (the instance label), without the folder.
    pub name: String,
    /// `Ready`, `Running`, `Queued`, `Disabled` (or anything newer).
    pub state: String,
    /// `LastTaskResult`, when it parsed.
    pub last_result: Option<i64>,
}

impl TaskInfo {
    fn is_running(&self) -> bool {
        matches!(self.state.as_str(), "Running" | "Queued")
    }

    fn is_idle(&self) -> bool {
        matches!(self.state.as_str(), "Ready" | "Disabled")
    }
}

/// List the tasks amicu registered (the `\amicu\` folder) for the current
/// user. Read-only.
///
/// # Errors
/// When PowerShell cannot be run or the listing does not complete.
pub fn list_tasks(exec: &dyn CommandExec) -> Result<Vec<TaskInfo>, LlamaCppError> {
    let out = exec.exec("powershell", &powershell_args(LIST_TASKS_SCRIPT))?;
    let complete = out.stdout.lines().any(|l| l.trim() == "END");
    if !out.success || !complete {
        return Err(LlamaCppError::Supervisor(format!(
            "listing the Task Scheduler folder \\{TASK_FOLDER}\\ failed (exit {:?}): {}",
            out.code,
            out.stderr.trim()
        )));
    }
    Ok(out
        .stdout
        .lines()
        .filter_map(|l| {
            let mut cols = l.trim_end_matches('\r').split('\t');
            if cols.next()? != "TASK" {
                return None;
            }
            let name = cols.next()?.to_owned();
            let state = cols.next()?.trim().to_owned();
            let last_result = cols.next().and_then(|r| r.trim().parse().ok());
            Some(TaskInfo {
                name,
                state,
                last_result,
            })
        })
        .collect())
}

fn powershell_args(script: &str) -> [&str; 6] {
    [
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ]
}

/// The full task name (`amicu\<label>`) for `/TN`.
#[must_use]
pub fn task_name(label: &str) -> String {
    format!("{TASK_FOLDER}\\{label}")
}

/// Task Scheduler-backed manager. Definitions (task XML, launcher, pid file)
/// go in `task_dir`, the owner-only env file in `env_dir`, logs in `log_dir`.
#[derive(Debug, Clone)]
pub struct TaskSchedulerManager<E> {
    exec: E,
    task_dir: PathBuf,
    env_dir: PathBuf,
    log_dir: PathBuf,
    /// How long a stop waits for the server process to exit.
    settle_timeout: Duration,
}

impl<E: CommandExec> TaskSchedulerManager<E> {
    /// Build over an executor and the definition / env / log directories.
    pub fn new(
        exec: E,
        task_dir: impl Into<PathBuf>,
        env_dir: impl Into<PathBuf>,
        log_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            exec,
            task_dir: task_dir.into(),
            env_dir: env_dir.into(),
            log_dir: log_dir.into(),
            settle_timeout: TASK_SETTLE_TIMEOUT,
        }
    }

    /// Bound how long a stop waits for the process to exit (tests use a short one).
    #[must_use]
    pub fn with_settle_timeout(mut self, timeout: Duration) -> Self {
        self.settle_timeout = timeout;
        self
    }

    fn xml_path(&self, label: &str) -> PathBuf {
        self.task_dir.join(format!("{label}.xml"))
    }

    fn launcher_path(&self, label: &str) -> PathBuf {
        self.task_dir.join(format!("{label}.ps1"))
    }

    fn pid_path(&self, label: &str) -> PathBuf {
        self.task_dir.join(format!("{label}.pid"))
    }

    fn env_path(&self, label: &str) -> PathBuf {
        self.env_dir.join(format!("{label}.env"))
    }

    fn stdout_path(&self, label: &str) -> PathBuf {
        self.log_dir.join(format!("{label}.out.log"))
    }

    fn stderr_path(&self, label: &str) -> PathBuf {
        self.log_dir.join(format!("{label}.err.log"))
    }

    /// The current user's SID, from `whoami /user /fo csv /nh`
    /// (`"host\user","S-1-5-21-..."`).
    fn user_sid(&self) -> Result<String, LlamaCppError> {
        let out = self.exec.exec("whoami", &["/user", "/fo", "csv", "/nh"])?;
        let sid = out
            .stdout
            .lines()
            .find_map(|l| csv_fields(l).get(1).cloned())
            .filter(|s| {
                s.starts_with("S-1-") && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            });
        match sid {
            Some(sid) if out.success => Ok(sid),
            _ => Err(LlamaCppError::Supervisor(format!(
                "cannot determine the current user's SID from `whoami /user`: {:?}",
                out.stdout.trim()
            ))),
        }
    }

    /// Restrict `path` to its owner: drop inherited entries and grant only
    /// the user `sid` full control (`(OI)(CI)` on a directory, so files
    /// created in it inherit just that entry).
    fn restrict_to_owner(&self, path: &Path, sid: &str, dir: bool) -> Result<(), LlamaCppError> {
        let grant = if dir {
            format!("*{sid}:(OI)(CI)F")
        } else {
            format!("*{sid}:F")
        };
        let target = path.to_string_lossy();
        let out = self
            .exec
            .exec("icacls", &[&target, "/inheritance:r", "/grant:r", &grant])?;
        if out.success {
            Ok(())
        } else {
            Err(LlamaCppError::Supervisor(format!(
                "icacls {target} failed: {}",
                out.stderr.trim()
            )))
        }
    }

    /// This instance's task, `None` when it is not registered.
    fn task(&self, label: &str) -> Result<Option<TaskInfo>, LlamaCppError> {
        Ok(list_tasks(&self.exec)?
            .into_iter()
            .find(|t| t.name.eq_ignore_ascii_case(label)))
    }

    /// The server process the launcher recorded, if it is still alive: its
    /// pid, when that pid still runs a process with the recorded start time
    /// (a reused pid is some other process, never ours to kill).
    fn live_pid(&self, label: &str) -> Result<Option<u32>, LlamaCppError> {
        let path = self.pid_path(label);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(LlamaCppError::Supervisor(format!(
                    "read {}: {e}",
                    path.display()
                )));
            }
        };
        let bad = || LlamaCppError::Supervisor(format!("{} is not a pid record", path.display()));
        let (pid, started) = text.trim().split_once('\t').ok_or_else(bad)?;
        let pid: u32 = pid.trim().parse().map_err(|_| bad())?;
        let started = started.trim();
        let script = format!(
            "$ErrorActionPreference = 'Stop'; \
             $p = Get-Process -Id {pid} -ErrorAction SilentlyContinue; \
             if ($p) {{ \"PROC`t$($p.StartTime.ToFileTimeUtc())\" }} else {{ 'NONE' }}"
        );
        let out = self.powershell(&script)?;
        let answer = out
            .stdout
            .lines()
            .map(str::trim)
            .find(|l| *l == "NONE" || l.starts_with("PROC\t"));
        match answer {
            Some("NONE") if out.success => Ok(None),
            Some(line) if out.success => {
                Ok((line.trim_start_matches("PROC\t") == started).then_some(pid))
            }
            _ => Err(LlamaCppError::Supervisor(format!(
                "cannot tell whether process {pid} is running (exit {:?}): {}",
                out.code,
                out.stderr.trim()
            ))),
        }
    }

    /// Run a PowerShell `script` (no profile, non-interactive).
    fn powershell(&self, script: &str) -> Result<crate::service::ExecOutput, LlamaCppError> {
        self.exec.exec("powershell", &powershell_args(script))
    }

    /// Ask the task and its server to stop, without waiting: disable the task
    /// first (so neither its restart-on-failure nor a logon brings it back
    /// meanwhile), end it, and kill the recorded server process tree.
    fn request_stop(&self, label: &str) {
        let name = task_name(label);
        let _ = self
            .exec
            .exec("schtasks", &["/Change", "/TN", &name, "/DISABLE"]);
        let _ = self.exec.exec("schtasks", &["/End", "/TN", &name]);
    }

    /// One check: is the instance confirmed gone (the task not running AND
    /// the server process gone)? A live server is killed again on the way.
    /// An unrecognized task state is never "gone".
    fn check_gone(&self, label: &str) -> Result<bool, LlamaCppError> {
        if let Some(pid) = self.live_pid(label)? {
            let pid = pid.to_string();
            let _ = self.exec.exec("taskkill", &["/PID", &pid, "/T", "/F"]);
            return Ok(false);
        }
        Ok(match self.task(label)? {
            None => true,
            Some(t) => t.is_idle(),
        })
    }

    /// Stop the instance and wait (bounded, with backoff) until it is
    /// confirmed gone. `Ok(false)` if it still runs after the timeout.
    async fn halt(&self, label: &str) -> Result<bool, LlamaCppError> {
        self.request_stop(label);
        let started = Instant::now();
        let mut delay = Duration::from_millis(50);
        let mut announced = false;
        loop {
            if self.check_gone(label)? {
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

    /// [`halt`](Self::halt) for the synchronous start path.
    fn halt_blocking(&self, label: &str) -> Result<bool, LlamaCppError> {
        self.request_stop(label);
        let started = Instant::now();
        let mut delay = Duration::from_millis(50);
        loop {
            if self.check_gone(label)? {
                return Ok(true);
            }
            let left = self.settle_timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Ok(false);
            }
            std::thread::sleep(delay.min(left));
            delay = (delay * 2).min(Duration::from_millis(500));
        }
    }

    /// Unregister the (stopped) task. Fine when it is already gone.
    fn delete_task(&self, label: &str) -> Result<(), LlamaCppError> {
        let name = task_name(label);
        let out = self
            .exec
            .exec("schtasks", &["/Delete", "/TN", &name, "/F"])?;
        if out.success || self.task(label)?.is_none() {
            return Ok(());
        }
        Err(LlamaCppError::Supervisor(format!(
            "schtasks /Delete /TN {name} failed: {}",
            out.stderr.trim()
        )))
    }

    /// Undo a start that did not complete. Only once the instance is
    /// confirmed gone are the task and files removed; otherwise they are kept
    /// and the error says so.
    fn abandon(&self, label: &str, cause: &str) -> LlamaCppError {
        let name = task_name(label);
        let confirmed = matches!(self.halt_blocking(label), Ok(true));
        if !confirmed || self.delete_task(label).is_err() {
            return LlamaCppError::Supervisor(format!(
                "{cause}; the task {name} may still be registered or running, so its files were \
                 kept: stop it with `schtasks /End /TN {name}` and `schtasks /Delete /TN {name} /F`"
            ));
        }
        for path in [
            self.env_path(label),
            self.pid_path(label),
            self.launcher_path(label),
            self.xml_path(label),
        ] {
            remove_env_file(&path);
        }
        LlamaCppError::Supervisor(cause.to_owned())
    }
}

impl TaskSchedulerManager<HostExec> {
    /// Construct with host defaults under `%LOCALAPPDATA%\amicu`: `tasks`
    /// (task XML, launcher, pid file), `env` (secrets) and `logs`.
    #[must_use]
    pub fn host() -> Self {
        // Unset LOCALAPPDATA leaves the dirs relative; `start` then refuses to write.
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("amicu");
        Self::new(
            HostExec,
            base.join("tasks"),
            base.join("env"),
            base.join("logs"),
        )
    }
}

/// Task names may not contain these, beyond what [`check_label`] refuses.
fn check_task_label(label: &str) -> Result<(), LlamaCppError> {
    check_label(label)?;
    if label.chars().any(|c| ":*?\"<>|'".contains(c)) || label.ends_with('.') {
        return Err(LlamaCppError::Supervisor(format!(
            "'{label}' is not a valid Task Scheduler task name"
        )));
    }
    Ok(())
}

#[async_trait]
impl<E: CommandExec> ServiceManager for TaskSchedulerManager<E> {
    fn paths(&self, label: &str) -> Option<ServicePaths> {
        Some(ServicePaths {
            definition: self.xml_path(label),
            logs: vec![self.stdout_path(label), self.stderr_path(label)],
            env: Some(self.env_path(label)),
        })
    }

    fn context(&self) -> Option<HostContext> {
        Some(HostContext {
            backend: HostBackend::TaskScheduler,
            uid: None,
            sid: Some(self.user_sid().ok()?),
            domain: None,
        })
    }

    async fn release(&self, label: &str) {
        // Stop only: `start` re-registers the task (`/F`) and rewrites its
        // files. Best effort; `start` refuses a port that is still served.
        if check_task_label(label).is_ok() {
            let _ = self.halt(label).await;
        }
    }

    fn presence(&self, label: &str, _domain: Option<&str>) -> Result<Presence, LlamaCppError> {
        check_task_label(label)?;
        if self.live_pid(label)?.is_some() {
            return Ok(Presence::Running);
        }
        Ok(match self.task(label)? {
            None => Presence::Down,
            Some(t) if t.is_running() => Presence::Running,
            Some(t) if t.is_idle() => Presence::Down,
            Some(_) => Presence::Unknown,
        })
    }

    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError> {
        if spec.argv.is_empty() {
            return Err(LlamaCppError::Supervisor("empty argv".to_owned()));
        }
        check_task_label(&spec.label)?;
        for dir in [&self.task_dir, &self.env_dir, &self.log_dir] {
            require_absolute(dir)?;
        }
        let label = &spec.label;
        // Everything that can fail before a secret is on disk happens first.
        let sid = self.user_sid()?;
        let env_body = env_file(&spec.env)?;
        let env_path = self.env_path(label);
        let launcher = launcher_script(
            spec,
            &env_path,
            &self.pid_path(label),
            &self.stdout_path(label),
            &self.stderr_path(label),
        );
        let xml = task_xml(label, &sid, &self.launcher_path(label));
        ensure_dir(&self.log_dir)?;
        ensure_dir(&self.task_dir)?;
        ensure_dir(&self.env_dir)?;
        self.restrict_to_owner(&self.env_dir, &sid, true)?;
        // A pid left by an earlier run must not be taken for this one's.
        remove_file_strict(&self.pid_path(label))?;
        write_private(&env_path, &self.env_dir, env_body.as_bytes())?;
        let written = self
            .restrict_to_owner(&env_path, &sid, false)
            .and_then(|()| {
                write_private(
                    &self.launcher_path(label),
                    &self.task_dir,
                    &utf8_bom(&launcher),
                )
            })
            .and_then(|()| write_private(&self.xml_path(label), &self.task_dir, &utf16_bom(&xml)));
        if let Err(e) = written {
            remove_env_file(&env_path);
            remove_env_file(&self.launcher_path(label));
            remove_env_file(&self.xml_path(label));
            return Err(e);
        }
        let name = task_name(label);
        let xml_path = self.xml_path(label).to_string_lossy().into_owned();
        for args in [
            vec![
                "/Create",
                "/XML",
                xml_path.as_str(),
                "/TN",
                name.as_str(),
                "/F",
            ],
            vec!["/Run", "/TN", name.as_str()],
        ] {
            let out = match self.exec.exec("schtasks", &args) {
                Ok(out) => out,
                Err(e) => return Err(self.abandon(label, &e.to_string())),
            };
            if !out.success {
                return Err(self.abandon(
                    label,
                    &format!("schtasks {} failed: {}", args[0], out.stderr.trim()),
                ));
            }
        }
        Ok(())
    }

    fn state(&self, label: &str) -> Result<ServiceState, LlamaCppError> {
        check_task_label(label)?;
        if self.live_pid(label)?.is_some() {
            return Ok(ServiceState::Active);
        }
        Ok(match self.task(label)? {
            None => ServiceState::Inactive,
            Some(t) if t.is_idle() => {
                if t.last_result.is_some_and(|r| !BENIGN_RESULTS.contains(&r)) {
                    ServiceState::Failed
                } else {
                    ServiceState::Inactive
                }
            }
            // Running, queued, or a state this build does not know: not
            // reported stopped.
            Some(_) => ServiceState::Active,
        })
    }

    async fn stop(&self, label: &str) -> Result<(), LlamaCppError> {
        self.stop_recorded_with_env(label, None, None).await
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
        check_task_label(label)?;
        let name = task_name(label);
        // The task and every file stay until the stop is confirmed: an
        // instance that may still run must remain findable by amicu.
        if !self.halt(label).await? {
            return Err(LlamaCppError::Supervisor(format!(
                "task {name} or its server process is still running {}s after `schtasks /End` \
                 and `taskkill`; nothing was removed",
                self.settle_timeout.as_secs()
            )));
        }
        self.delete_task(label)?;
        // The secrets file goes first; a failure to delete anything is an
        // error that keeps the record.
        let mut env_files: Vec<PathBuf> = env.map(Path::to_path_buf).into_iter().collect();
        env_files.push(self.env_path(label));
        env_files.dedup();
        for path in &env_files {
            remove_file_strict(path)?;
        }
        remove_file_strict(&self.pid_path(label))?;
        remove_file_strict(&self.launcher_path(label))?;
        let xml = definition.map_or_else(|| self.xml_path(label), Path::to_path_buf);
        remove_file_strict(&xml)?;
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

// ---- rendering ----------------------------------------------------------------

/// The task definition (Task Scheduler schema 1.2): a logon trigger and an
/// interactive, least-privilege principal for the user `sid` (no stored
/// password), no execution time limit, a bounded restart on failure, and one
/// action running the launcher through PowerShell.
fn task_xml(label: &str, sid: &str, launcher: &Path) -> String {
    let sid = xml_escape(sid);
    // `-File` takes the next argument verbatim; a Windows path cannot hold a
    // `"`, so quoting it is enough.
    let args = xml_escape(&format!(
        "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File \"{}\"",
        launcher.display()
    ));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>amicu deployment {label}</Description>
    <URI>\{TASK_FOLDER}\{label}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{sid}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>{RESTART_INTERVAL}</Interval>
      <Count>{RESTART_COUNT}</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>powershell.exe</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        label = xml_escape(label),
    )
}

/// The launcher the task runs: load the env file (`NAME=<base64>` lines, read
/// as data), start the server with its output redirected to the log files,
/// record its pid and start time (together they identify the process even
/// after the pid is reused), and exit with its exit code.
fn launcher_script(
    spec: &ServiceSpec,
    env: &Path,
    pid: &Path,
    stdout: &Path,
    stderr: &Path,
) -> String {
    let program = &spec.argv[0];
    let arguments = if spec.argv.len() > 1 {
        let line = spec.argv[1..]
            .iter()
            .map(|a| windows_arg(a))
            .collect::<Vec<_>>()
            .join(" ");
        format!(" -ArgumentList {}", ps_quote(&line))
    } else {
        String::new()
    };
    format!(
        r#"# amicu launcher for {label}. Generated; rewritten on every apply.
$ErrorActionPreference = 'Stop'
foreach ($line in [System.IO.File]::ReadAllLines({env})) {{
  if ($line -eq '') {{ continue }}
  $i = $line.IndexOf('=')
  $value = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($line.Substring($i + 1)))
  [System.Environment]::SetEnvironmentVariable($line.Substring(0, $i), $value, 'Process')
}}
$p = Start-Process -FilePath {program}{arguments} -NoNewWindow -PassThru -RedirectStandardOutput {out} -RedirectStandardError {err}
$null = $p.Handle
try {{ [System.IO.File]::WriteAllText({pid}, "$($p.Id)`t$($p.StartTime.ToFileTimeUtc())") }} catch {{ $p.Kill(); exit 1 }}
$p.WaitForExit()
exit $p.ExitCode
"#,
        label = spec.label,
        env = ps_quote(&env.to_string_lossy()),
        program = ps_quote(program),
        out = ps_quote(&stdout.to_string_lossy()),
        err = ps_quote(&stderr.to_string_lossy()),
        pid = ps_quote(&pid.to_string_lossy()),
    )
}

/// The env file the launcher reads: `NAME=<base64 of the UTF-8 value>` lines,
/// so a value can hold any character (newlines, quotes, `%`) without escaping.
fn env_file(env: &[(String, String)]) -> Result<String, LlamaCppError> {
    let mut out = String::new();
    for (k, v) in env {
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(LlamaCppError::Supervisor(format!(
                "invalid environment variable name '{k}'"
            )));
        }
        out.push_str(k);
        out.push('=');
        out.push_str(&base64(v.as_bytes()));
        out.push_str("\r\n");
    }
    Ok(out)
}

/// A PowerShell single-quoted string literal: nothing inside is expanded. Each
/// single quote is doubled, including the typographic ones PowerShell also
/// treats as quotes.
#[must_use]
pub fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// One argument of a Windows command line, quoted so `CommandLineToArgvW`
/// (and the MSVC runtime) parse it back to exactly `arg`.
#[must_use]
pub fn windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// The fields of one CSV line as `whoami` prints it (every field
/// double-quoted, `""` for a literal quote).
fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.trim_end_matches(['\r', '\n']).chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

/// UTF-16LE with a byte-order mark: the encoding `schtasks /XML` reads
/// reliably.
fn utf16_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// UTF-8 with a byte-order mark: Windows PowerShell reads a script without
/// one in the ANSI code page, which would mangle a non-ASCII path.
fn utf8_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xEF, 0xBB, 0xBF];
    out.extend_from_slice(s.as_bytes());
    out
}

/// Standard base64 (with padding).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "service_windows_tests.rs"]
mod tests;
