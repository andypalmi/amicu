//! Service-manager operations behind launchd (macOS) and systemd (Linux).
//!
//! The [`ServiceManager`] trait is the injectable seam the supervisor drives;
//! real impls shell out through a [`CommandExec`] (itself injectable) and write
//! service definitions to disk, so unit tests stay hermetic. Secrets are passed
//! through the process environment only — never into argv or a log.

use std::path::{Path, PathBuf};
use std::process::Command;

use amicu_runtime::LogOptions;

use crate::error::LlamaCppError;

/// A service to define and run: its manager label, the argv to exec, and the
/// environment (resolved secrets) to inject into the process.
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    /// Manager label / unit name, e.g. `io.amicu.llama.<deployment>`.
    pub label: String,
    /// Program plus arguments; `argv[0]` is the program.
    pub argv: Vec<String>,
    /// Environment injected into the process (secret name/value pairs).
    pub env: Vec<(String, String)>,
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

/// Load/start, query, stop, and read logs for a supervised service.
pub trait ServiceManager: Send + Sync {
    /// Write the service definition and start it.
    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError>;
    /// The service's current lifecycle state.
    fn state(&self, label: &str) -> Result<ServiceState, LlamaCppError>;
    /// Stop and unload the service.
    fn stop(&self, label: &str) -> Result<(), LlamaCppError>;
    /// Read the service's captured log lines.
    fn logs(&self, label: &str, options: LogOptions) -> Result<Vec<String>, LlamaCppError>;
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
}

/// Runs external commands. Injectable so managers are testable without a real
/// `launchctl`/`systemctl`/`journalctl`.
pub trait CommandExec: Send + Sync {
    /// Run `program args...`, capturing its output.
    fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, LlamaCppError>;
}

/// The real executor: spawns the process and captures its output.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostExec;

impl CommandExec for HostExec {
    fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, LlamaCppError> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|e| LlamaCppError::Supervisor(format!("{program}: {e}")))?;
        Ok(ExecOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn tail<T>(mut lines: Vec<T>, options: LogOptions) -> Vec<T> {
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
}

impl<E: CommandExec> LaunchdManager<E> {
    /// Build over an executor and the LaunchAgents / log directories.
    pub fn new(exec: E, agents_dir: impl Into<PathBuf>, log_dir: impl Into<PathBuf>) -> Self {
        Self {
            exec,
            agents_dir: agents_dir.into(),
            log_dir: log_dir.into(),
        }
    }

    fn plist_path(&self, label: &str) -> PathBuf {
        self.agents_dir.join(format!("{label}.plist"))
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
        let home = home_dir();
        Self::new(
            HostExec,
            home.join("Library/LaunchAgents"),
            home.join("Library/Logs/amicu"),
        )
    }
}

impl<E: CommandExec> ServiceManager for LaunchdManager<E> {
    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError> {
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
        ensure_dir(&self.log_dir)?;
        let plist = self.plist_path(&spec.label);
        let out = self
            .exec
            .exec("launchctl", &["load", "-w", &plist.to_string_lossy()])?;
        if !out.success {
            return Err(LlamaCppError::Supervisor(format!(
                "launchctl load failed: {}",
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

    fn stop(&self, label: &str) -> Result<(), LlamaCppError> {
        let plist = self.plist_path(label);
        let out = self
            .exec
            .exec("launchctl", &["unload", "-w", &plist.to_string_lossy()])?;
        if !out.success {
            return Err(LlamaCppError::Supervisor(format!(
                "launchctl unload failed: {}",
                out.stderr.trim()
            )));
        }
        // Remove the per-instance plist so its EnvironmentVariables don't linger
        // on disk; a missing file is fine, any other error is surfaced.
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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---- systemd ----------------------------------------------------------------

/// systemd-backed manager: a transient `--user` unit started with `systemd-run`,
/// queried with `systemctl --user`, logs read via `journalctl --user`.
///
/// Secrets are passed with `--setenv`, entering the process environment; no unit
/// file with plaintext secrets is written to disk.
#[derive(Debug, Clone)]
pub struct SystemdManager<E> {
    exec: E,
}

impl<E: CommandExec> SystemdManager<E> {
    /// Build over an executor.
    pub fn new(exec: E) -> Self {
        Self { exec }
    }
}

impl SystemdManager<HostExec> {
    /// Construct with the host executor.
    #[must_use]
    pub fn host() -> Self {
        Self::new(HostExec)
    }
}

impl<E: CommandExec> ServiceManager for SystemdManager<E> {
    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError> {
        let unit = format!("--unit={}", spec.label);
        let mut args = vec!["--user", "--collect", &unit];
        // --setenv values are visible on systemd-run's own argv (/proc/<pid>/cmdline)
        // to the same user for the invocation's lifetime; acceptable as this is a
        // --user unit. A 0600 EnvironmentFile would avoid even that same-user window.
        let setenvs: Vec<String> = spec
            .env
            .iter()
            .map(|(k, v)| format!("--setenv={k}={v}"))
            .collect();
        args.extend(setenvs.iter().map(String::as_str));
        // The rest is the program + its arguments.
        let command: Vec<&str> = spec.argv.iter().map(String::as_str).collect();
        if command.is_empty() {
            return Err(LlamaCppError::Supervisor("empty argv".to_owned()));
        }
        args.extend(command);
        let out = self.exec.exec("systemd-run", &args)?;
        if !out.success {
            return Err(LlamaCppError::Supervisor(format!(
                "systemd-run failed: {}",
                out.stderr.trim()
            )));
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

    fn stop(&self, label: &str) -> Result<(), LlamaCppError> {
        let out = self.exec.exec("systemctl", &["--user", "stop", label])?;
        if !out.success {
            return Err(LlamaCppError::Supervisor(format!(
                "systemctl stop failed: {}",
                out.stderr.trim()
            )));
        }
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
}

// ---- fs helpers -------------------------------------------------------------

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn ensure_dir(dir: &Path) -> Result<(), LlamaCppError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| LlamaCppError::Supervisor(format!("create {}: {e}", dir.display())))
}

/// Write `bytes` to `path` (creating `dir`), owner-readable only — the plist can
/// hold `EnvironmentVariables`, so it must not be world-readable.
fn write_private(path: &Path, dir: &Path, bytes: &[u8]) -> Result<(), LlamaCppError> {
    use std::io::Write as _;
    ensure_dir(dir)?;
    // Create 0600 atomically so there is no world/group-readable window; truncate
    // to overwrite any existing plist. On non-unix, mode() is unavailable but the
    // rest of this module is unix-only, so a plain create is acceptable there.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| LlamaCppError::Supervisor(format!("create {}: {e}", path.display())))?;
    file.write_all(bytes)
        .map_err(|e| LlamaCppError::Supervisor(format!("write {}: {e}", path.display())))?;
    Ok(())
}
