//! Service-manager operations behind launchd (macOS) and systemd (Linux).
//!
//! The [`ServiceManager`] trait is the injectable seam the supervisor drives;
//! real impls shell out through a [`CommandExec`] (itself injectable) and write
//! service definitions to disk, so unit tests stay hermetic. Secrets are passed
//! through the process environment only - never into argv or a log.

use std::path::{Path, PathBuf};
use std::process::Command;

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
        // Unset HOME leaves the dirs empty; `start` then refuses to write.
        let home = home_dir().unwrap_or_default();
        Self::new(
            HostExec,
            home.join("Library/LaunchAgents"),
            home.join("Library/Logs/amicu"),
        )
    }
}

impl<E: CommandExec> ServiceManager for LaunchdManager<E> {
    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError> {
        require_absolute(&self.agents_dir)?;
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
        let out = match self
            .exec
            .exec("launchctl", &["load", "-w", &plist.to_string_lossy()])
        {
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
/// Env (including secrets) goes in a per-unit 0600 `EnvironmentFile` that is
/// removed on stop, so values never appear on `systemd-run`'s argv
/// (`/proc/<pid>/cmdline`) and no unit file holds plaintext secrets.
#[derive(Debug, Clone)]
pub struct SystemdManager<E> {
    exec: E,
    env_dir: PathBuf,
}

impl<E: CommandExec> SystemdManager<E> {
    /// Build over an executor and the directory holding per-unit env files.
    pub fn new(exec: E, env_dir: impl Into<PathBuf>) -> Self {
        Self {
            exec,
            env_dir: env_dir.into(),
        }
    }

    fn env_path(&self, label: &str) -> PathBuf {
        self.env_dir.join(format!("{label}.env"))
    }
}

impl SystemdManager<HostExec> {
    /// Construct with the host executor; env files live in
    /// `$XDG_RUNTIME_DIR/amicu` (tmpfs) or `~/.local/state/amicu/env`.
    #[must_use]
    pub fn host() -> Self {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map_or_else(
                || {
                    // Unset HOME leaves the dir empty; `start` then refuses to
                    // write secrets relative to the cwd.
                    home_dir().map_or_else(PathBuf::new, |h| h.join(".local/state/amicu/env"))
                },
                |d| PathBuf::from(d).join("amicu"),
            );
        Self::new(HostExec, dir)
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

impl<E: CommandExec> ServiceManager for SystemdManager<E> {
    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError> {
        let unit = format!("--unit={}", spec.label);
        let mut args = vec!["--user", "--collect", &unit];
        // Env goes through a 0600 EnvironmentFile, not `--setenv`, whose values
        // would sit on systemd-run's argv (/proc/<pid>/cmdline).
        let env_path = self.env_path(&spec.label);
        let property;
        if !spec.env.is_empty() {
            require_absolute(&self.env_dir)?;
            let body = environment_file(&spec.env)?;
            write_private(&env_path, &self.env_dir, body.as_bytes())?;
            property = format!("--property=EnvironmentFile={}", env_path.display());
            args.push(&property);
        }
        // The rest is the program + its arguments.
        let command: Vec<&str> = spec.argv.iter().map(String::as_str).collect();
        if command.is_empty() {
            return Err(LlamaCppError::Supervisor("empty argv".to_owned()));
        }
        args.extend(command);
        let out = match self.exec.exec("systemd-run", &args) {
            Ok(out) => out,
            Err(e) => {
                remove_env_file(&env_path);
                return Err(e);
            }
        };
        if !out.success {
            remove_env_file(&env_path);
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
        let out = self.exec.exec("systemctl", &["--user", "stop", label]);
        // Always drop the secrets file, even when stop fails (a crashed
        // `--collect` unit is "not loaded" and its stop errors).
        remove_env_file(&self.env_path(label));
        let out = out?;
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

// ---- fs helpers -----------------------------------------------------------

/// Best-effort removal of a unit's env file; a missing file is fine.
fn remove_env_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Refuse a relative or empty directory: it means `HOME` was unset, and secrets
/// must never be written relative to the cwd.
fn require_absolute(dir: &Path) -> Result<(), LlamaCppError> {
    if dir.is_absolute() {
        Ok(())
    } else {
        Err(LlamaCppError::Supervisor(
            "cannot locate a private directory for service files: HOME is not set".to_owned(),
        ))
    }
}

/// Create `dir` (and parents), the leaf owner-only (0700) on unix.
fn ensure_dir(dir: &Path) -> Result<(), LlamaCppError> {
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
fn write_private(path: &Path, dir: &Path, bytes: &[u8]) -> Result<(), LlamaCppError> {
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
