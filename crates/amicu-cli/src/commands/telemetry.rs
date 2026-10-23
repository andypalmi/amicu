//! `amicu telemetry enable|disable|status` and the one-time consent prompt
//!
//! Telemetry is opt-in and OFF by default. The choice lives in a user-level
//! file (`$XDG_CONFIG_HOME/amicu/telemetry.json`, else
//! `~/.config/amicu/telemetry.json`), never in project state, so it follows the
//! person rather than the repository.
//!
//! Precedence, strongest first:
//! 1. `DO_NOT_TRACK=1` (any truthy value) forces off.
//! 2. `AMICU_TELEMETRY=0|1` (also false/true/off/on/no/yes).
//! 3. The persisted choice.
//! 4. Default: off, not yet asked.
//!
//! This module only records consent and makes no network calls. When the
//! effective setting is on, `apply` starts the telemetry agent
//! (`commands::telemetry_agent`) beside each local deployment, and as a job of
//! its own for each Modal or RunPod one; the agent
//! re-reads this setting before every scrape and upload.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::OutputFormat;
use crate::error::CliError;
use crate::write_json;

const COLLECTING: &str = "Telemetry agents on this machine collect 5-minute performance rollups, \
buffer them locally and send them to the amicu telemetry endpoint (https://api.amicu.dev). \
Aggregates only: never prompts, completions, IPs, hostnames, paths, usernames or keys.";

const ON_NO_AGENTS: &str = "Telemetry is on, but no deployment on this machine runs an agent yet. A \
deployment gets one the next time `amicu apply` starts or updates it.";

const NOT_COLLECTING: &str = "No telemetry data is collected or sent.";

/// The telemetry agents on this host, as `status` reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct AgentSummary {
    /// Agent jobs whose service definition is installed.
    pub installed: usize,
    /// Of those, confirmed running by the service manager.
    pub running: usize,
    /// Deployments with buffered, unsent rollups.
    pub queued: usize,
}

impl AgentSummary {
    /// Read the host: installed agent jobs and queue files. Read-only.
    pub(crate) fn host() -> Self {
        let agents = amicu_llamacpp::host_supervisor().sidecars();
        let queued = crate::commands::telemetry_agent::data_dir()
            .and_then(|d| std::fs::read_dir(crate::commands::telemetry_agent::queue_dir(&d)).ok())
            .map_or(0, |entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                    .count()
            });
        Self {
            installed: agents.len(),
            running: agents
                .iter()
                .filter(|(_, p)| *p == amicu_llamacpp::Presence::Running)
                .count(),
            queued,
        }
    }

    fn any(self) -> bool {
        self.installed > 0 || self.queued > 0
    }
}

/// Env vars that mark a non-interactive CI environment.
const CI_VARS: &[&str] = &[
    "CI",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "BUILDKITE",
    "CIRCLECI",
    "TF_BUILD",
    "JENKINS_URL",
    "TEAMCITY_VERSION",
];

/// Where the effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceKind {
    DoNotTrack,
    EnvAmicuTelemetry,
    Config,
    /// The config file exists but cannot be read or parsed; treated as not set.
    ConfigUnreadable,
    Default,
}

impl SourceKind {
    fn describe(self) -> &'static str {
        match self {
            SourceKind::DoNotTrack => "DO_NOT_TRACK environment variable",
            SourceKind::EnvAmicuTelemetry => "AMICU_TELEMETRY environment variable",
            SourceKind::Config => "user config file",
            SourceKind::ConfigUnreadable => "config unreadable (treated as not set)",
            SourceKind::Default => "default (not asked yet; off)",
        }
    }
}

/// The effective setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Effective {
    pub enabled: bool,
    pub source: SourceKind,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    telemetry: Option<bool>,
}

/// Reads environment variables; injectable for tests.
pub(crate) type EnvGet<'a> = &'a dyn Fn(&str) -> Option<String>;

fn real_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// The persisted choice, `None` when never decided.
pub(crate) struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The per-user location, or `None` when no home/config dir is known.
    pub(crate) fn host() -> Option<Self> {
        #[cfg(windows)]
        let base = std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)?;
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".config"))
            })?;
        Some(Self::new(base.join("amicu").join("telemetry.json")))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> Result<Option<bool>, CliError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(serde_json::from_str::<Stored>(&text)?.telemetry),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CliError::io(
                format!("cannot read {}", self.path.display()),
                e,
            )),
        }
    }

    pub(crate) fn save(&self, enabled: bool) -> Result<(), CliError> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| CliError::io(format!("cannot create {}", dir.display()), e))?;
        }
        let text = serde_json::to_string_pretty(&Stored {
            telemetry: Some(enabled),
        })?;
        std::fs::write(&self.path, format!("{text}\n"))
            .map_err(|e| CliError::io(format!("cannot write {}", self.path.display()), e))
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Resolve the effective setting. Pure over its inputs.
pub(crate) fn effective(env: EnvGet<'_>, stored: Option<bool>) -> Effective {
    if env("DO_NOT_TRACK").is_some_and(|v| parse_bool(&v) == Some(true)) {
        return Effective {
            enabled: false,
            source: SourceKind::DoNotTrack,
        };
    }
    if let Some(v) = env("AMICU_TELEMETRY").and_then(|v| parse_bool(&v)) {
        return Effective {
            enabled: v,
            source: SourceKind::EnvAmicuTelemetry,
        };
    }
    match stored {
        Some(enabled) => Effective {
            enabled,
            source: SourceKind::Config,
        },
        None => Effective {
            enabled: false,
            source: SourceKind::Default,
        },
    }
}

fn io_err(e: std::io::Error) -> CliError {
    CliError::io("write failed", e)
}

fn store_or_err() -> Result<ConfigStore, CliError> {
    ConfigStore::host().ok_or_else(|| {
        CliError::Message(
            if cfg!(windows) {
                "cannot locate a user config directory (set APPDATA)"
            } else {
                "cannot locate a user config directory (set HOME or XDG_CONFIG_HOME)"
            }
            .to_owned(),
        )
    })
}

/// `amicu telemetry enable`.
pub(crate) fn enable(out: &mut dyn Write) -> Result<u8, CliError> {
    set(&store_or_err()?, true, &real_env, out)
}

/// `amicu telemetry disable`: save the choice, then stop every telemetry
/// agent on this machine and delete every buffered rollup, right away.
pub(crate) fn disable(out: &mut dyn Write) -> Result<u8, CliError> {
    let host = amicu_llamacpp::host_supervisor();
    let data = crate::commands::telemetry_agent::data_dir();
    disable_with(
        &store_or_err()?,
        &real_env,
        &*host,
        &|| clear_queues(data.as_deref()),
        out,
    )
}

fn disable_with(
    store: &ConfigStore,
    env: EnvGet<'_>,
    host: &dyn amicu_llamacpp::ProcessSupervisor,
    clear: &dyn Fn(),
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    set(store, false, env, out)?;
    let stopped = crate::commands::build_runtime()?.block_on(host.stop_all_sidecars());
    // Buffered rollups that were never sent are deleted, not kept.
    clear();
    match stopped {
        Ok(labels) => {
            writeln!(out, "Stopped {} telemetry agent(s).", labels.len()).map_err(io_err)?;
            Ok(0)
        }
        Err(e) => {
            writeln!(
                out,
                "error: {e}. Run `amicu telemetry disable` again; until then that agent sees \
                 the saved opt-out within a minute and sends nothing."
            )
            .map_err(io_err)?;
            Ok(1)
        }
    }
}

fn set(
    store: &ConfigStore,
    enabled: bool,
    env: EnvGet<'_>,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    store.save(enabled)?;
    writeln!(
        out,
        "telemetry {} (saved to {})",
        if enabled { "enabled" } else { "disabled" },
        store.path().display()
    )
    .map_err(io_err)?;
    let eff = effective(env, Some(enabled));
    if eff.source != SourceKind::Config && eff.enabled != enabled {
        writeln!(
            out,
            "note: the {} overrides this setting; effective telemetry is {}.",
            eff.source.describe(),
            if eff.enabled { "on" } else { "off" }
        )
        .map_err(io_err)?;
    }
    if eff.enabled {
        writeln!(out, "{ON_NO_AGENTS}").map_err(io_err)?;
    }
    Ok(0)
}

/// Delete every agent's buffered, unsent rollups. Best effort.
fn clear_queues(data: Option<&Path>) {
    if let Some(data) = data {
        let _ = std::fs::remove_dir_all(crate::commands::telemetry_agent::queue_dir(data));
    }
}

/// The env override set in this shell, as `NAME=value`.
fn env_override(env: EnvGet<'_>) -> Option<String> {
    ["DO_NOT_TRACK", "AMICU_TELEMETRY"]
        .iter()
        .find_map(|k| env(k).filter(|v| !v.is_empty()).map(|v| format!("{k}={v}")))
}

#[derive(Serialize)]
struct StatusReport {
    version: &'static str,
    enabled: bool,
    source: SourceKind,
    config_path: Option<String>,
    /// The choice saved in the config file (`null`: never asked).
    stored: Option<bool>,
    /// A `DO_NOT_TRACK` / `AMICU_TELEMETRY` override set in this shell.
    env_override: Option<String>,
    agents: AgentSummary,
    /// Whether agents on this machine are collecting now. They read only the
    /// config file, not this shell's environment.
    collecting: bool,
    note: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

/// `amicu telemetry status`.
pub(crate) fn status(output: OutputFormat, out: &mut dyn Write) -> Result<u8, CliError> {
    let store = ConfigStore::host();
    status_with(store.as_ref(), &real_env, AgentSummary::host(), output, out)
}

fn status_with(
    store: Option<&ConfigStore>,
    env: EnvGet<'_>,
    agents: AgentSummary,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    // A corrupt or unreadable file must not make `status` fail: degrade to the
    // default (off) and say so. Env overrides still win via `effective`.
    let mut warnings = Vec::new();
    let stored = match store.map(ConfigStore::load) {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            warnings.push(format!("telemetry config is unreadable: {e}"));
            None
        }
        None => None,
    };
    let mut eff = effective(env, stored);
    if !warnings.is_empty() && eff.source == SourceKind::Default {
        eff.source = SourceKind::ConfigUnreadable;
    }
    // Agents see only the file (their service has no override), so they
    // collect exactly when an agent exists and the file says yes.
    let collecting = agents.installed > 0 && stored == Some(true);
    let note = if collecting {
        COLLECTING
    } else if eff.enabled {
        ON_NO_AGENTS
    } else {
        NOT_COLLECTING
    };
    if !eff.enabled && agents.any() {
        warnings.push(format!(
            "telemetry is off here, but {} agent(s) are still installed and {} deployment(s) \
             have buffered data{}; run `amicu telemetry disable` (or any `amicu apply`) to \
             stop them and delete the data",
            agents.installed,
            agents.queued,
            if collecting {
                " (they read only the config file, which says on, so they still collect and send)"
            } else {
                ""
            }
        ));
    }
    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    let override_here = env_override(env);
    if output.is_json() {
        write_json(
            out,
            &StatusReport {
                version: "v1",
                enabled: eff.enabled,
                source: eff.source,
                config_path: store.map(|s| s.path().display().to_string()),
                stored,
                env_override: override_here,
                agents,
                collecting,
                note,
                warning,
            },
        )?;
        return Ok(0);
    }
    let onoff = |v: bool| if v { "on" } else { "off" };
    writeln!(
        out,
        "telemetry: {}\nsource:    {}\nstored:    {}\noverride:  {}\nagents:    {} installed, {} running; {} with buffered data",
        if eff.enabled { "enabled" } else { "disabled" },
        eff.source.describe(),
        stored.map_or("not set", onoff),
        override_here.as_deref().unwrap_or("none in this shell"),
        agents.installed,
        agents.running,
        agents.queued,
    )
    .map_err(io_err)?;
    if let Some(s) = store {
        writeln!(out, "config:    {}", s.path().display()).map_err(io_err)?;
    }
    if let Some(w) = &warning {
        writeln!(out, "warning:   {w}").map_err(io_err)?;
    }
    writeln!(out, "\n{note}").map_err(io_err)?;
    Ok(0)
}

// ---- first-run consent prompt -----------------------------------------------

const PROMPT: &str = "\
Help improve amicu? (one-time question)

If you opt in, your local deployments will send aggregate performance
metrics to api.amicu.dev about once an hour: latency, throughput, hardware
and model identifiers. It never sends prompts, completions, secrets or IP
addresses.

You can change this any time with `amicu telemetry enable|disable`
(or set AMICU_TELEMETRY=0/1; DO_NOT_TRACK=1 always turns it off).

Share anonymous performance metrics? [y/N] ";

fn is_ci(env: EnvGet<'_>) -> bool {
    CI_VARS
        .iter()
        .any(|k| env(k).is_some_and(|v| !v.is_empty() && v != "0" && v != "false"))
}

/// A prompt needs a person: stdin, stdout and stderr (where the question is
/// written) all TTYs, and not JSON output mode.
pub(crate) fn is_interactive(ttys: [bool; 3], json: bool) -> bool {
    ttys.iter().all(|t| *t) && !json
}

/// Ask once, on the first interactive run, then persist the answer.
///
/// Returns whether the prompt was shown. Never asks when: stdin, stdout or
/// stderr is not a TTY, JSON output mode, in CI, an env override is set, or a choice is already stored.
/// The default (empty answer, EOF, anything but yes) is No, and No is stored so
/// the question is never repeated. Prompt goes to `out` (stderr in the host
/// wrapper) so `--output json` on stdout stays clean.
pub(crate) fn maybe_prompt(
    store: &ConfigStore,
    env: EnvGet<'_>,
    interactive: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> bool {
    if !interactive
        || is_ci(env)
        || env("AMICU_TELEMETRY").is_some()
        || env("DO_NOT_TRACK").is_some()
    {
        return false;
    }
    // An unreadable/corrupt file counts as "asked" so we never nag or clobber.
    match store.load() {
        Ok(None) => {}
        _ => return false,
    }
    let _ = write!(out, "{PROMPT}");
    let _ = out.flush();
    let mut line = String::new();
    let _ = input.read_line(&mut line);
    let yes = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    match store.save(yes) {
        Ok(()) => {
            let _ = writeln!(
                out,
                "Telemetry {}. Change it with `amicu telemetry {}`.",
                if yes { "enabled" } else { "disabled" },
                if yes { "disable" } else { "enable" }
            );
        }
        Err(e) => {
            let _ = writeln!(out, "warning: could not save your choice: {e}");
        }
    }
    true
}

/// Host wrapper used by `init` and `apply`. Best effort; never fails a command.
pub(crate) fn consent_prompt(output: OutputFormat) {
    let Some(store) = ConfigStore::host() else {
        return;
    };
    let interactive = is_interactive(
        [
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        ],
        output.is_json(),
    );
    maybe_prompt(
        &store,
        &real_env,
        interactive,
        &mut std::io::stdin().lock(),
        &mut std::io::stderr(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn temp_store(tag: &str) -> (ConfigStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "amicu-telemetry-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (ConfigStore::new(dir.join("amicu/telemetry.json")), dir)
    }

    #[test]
    fn interactive_needs_all_ttys_and_no_json() {
        assert!(is_interactive([true; 3], false));
        assert!(!is_interactive([false, true, true], false));
        assert!(!is_interactive([true, false, true], false));
        assert!(!is_interactive([true, true, false], false));
        assert!(!is_interactive([true; 3], true));
    }

    #[test]
    fn default_is_off_and_unasked() {
        let e = effective(&env_of(&[]), None);
        assert_eq!(
            e,
            Effective {
                enabled: false,
                source: SourceKind::Default
            }
        );
    }

    #[test]
    fn precedence_do_not_track_then_env_then_config() {
        let on = Some(true);
        assert_eq!(
            effective(
                &env_of(&[("DO_NOT_TRACK", "1"), ("AMICU_TELEMETRY", "1")]),
                on
            ),
            Effective {
                enabled: false,
                source: SourceKind::DoNotTrack
            }
        );
        assert_eq!(
            effective(&env_of(&[("AMICU_TELEMETRY", "0")]), on),
            Effective {
                enabled: false,
                source: SourceKind::EnvAmicuTelemetry
            }
        );
        assert_eq!(
            effective(&env_of(&[("AMICU_TELEMETRY", "1")]), Some(false)),
            Effective {
                enabled: true,
                source: SourceKind::EnvAmicuTelemetry
            }
        );
        assert_eq!(
            effective(&env_of(&[("DO_NOT_TRACK", "0")]), on),
            Effective {
                enabled: true,
                source: SourceKind::Config
            }
        );
        assert_eq!(
            effective(&env_of(&[("AMICU_TELEMETRY", "garbage")]), on).source,
            SourceKind::Config
        );
    }

    const NO_AGENTS: AgentSummary = AgentSummary {
        installed: 0,
        running: 0,
        queued: 0,
    };
    const TWO_AGENTS: AgentSummary = AgentSummary {
        installed: 2,
        running: 2,
        queued: 1,
    };

    fn status_text(store: &ConfigStore, env: &[(&str, &str)], agents: AgentSummary) -> String {
        let mut s = Vec::new();
        status_with(
            Some(store),
            &env_of(env),
            agents,
            OutputFormat::Human,
            &mut s,
        )
        .unwrap();
        String::from_utf8(s).unwrap()
    }

    fn status_json(
        store: &ConfigStore,
        env: &[(&str, &str)],
        agents: AgentSummary,
    ) -> serde_json::Value {
        let mut buf = Vec::new();
        status_with(
            Some(store),
            &env_of(env),
            agents,
            OutputFormat::Json,
            &mut buf,
        )
        .unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    #[test]
    fn enable_disable_status_round_trip() {
        let (store, dir) = temp_store("rt");
        let env = env_of(&[]);
        let mut out = Vec::new();
        set(&store, true, &env, &mut out).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("next time `amicu apply`")
        );
        assert_eq!(store.load().unwrap(), Some(true));
        let s = status_text(&store, &[], TWO_AGENTS);
        assert!(s.contains("telemetry: enabled"));
        assert!(s.contains("user config file"));
        assert!(s.contains("stored:    on"));
        assert!(s.contains("override:  none in this shell"));
        assert!(s.contains("2 installed, 2 running; 1 with buffered data"));
        assert!(s.contains("collect 5-minute performance rollups"));
        // On, but nothing deployed with an agent yet: say exactly that.
        let s = status_text(&store, &[], NO_AGENTS);
        assert!(
            s.contains("no deployment on this machine runs an agent yet"),
            "{s}"
        );
        assert!(!s.contains("collect 5-minute"));

        set(&store, false, &env, &mut Vec::new()).unwrap();
        assert_eq!(store.load().unwrap(), Some(false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn status_never_claims_nothing_is_collected_while_agents_run() {
        let (store, dir) = temp_store("truthful");
        store.save(true).unwrap();
        // Off in this shell, but the agents read only the file: they collect.
        let v = status_json(&store, &[("DO_NOT_TRACK", "1")], TWO_AGENTS);
        assert_eq!(v["enabled"], false);
        assert_eq!(v["stored"], true);
        assert_eq!(v["env_override"], "DO_NOT_TRACK=1");
        assert_eq!(v["agents"]["running"], 2);
        assert_eq!(v["collecting"], true);
        assert_ne!(v["note"], NOT_COLLECTING);
        let w = v["warning"].as_str().unwrap();
        assert!(
            w.contains("2 agent(s) are still installed") && w.contains("still collect"),
            "{w}"
        );
        // Opted out in the file but agents not yet stopped: warned, not collecting.
        store.save(false).unwrap();
        let v = status_json(&store, &[], TWO_AGENTS);
        assert_eq!(v["collecting"], false);
        assert!(
            v["warning"]
                .as_str()
                .unwrap()
                .contains("amicu telemetry disable")
        );
        // Off and nothing installed: the plain statement is true.
        let v = status_json(&store, &[], NO_AGENTS);
        assert_eq!(v["note"], NOT_COLLECTING);
        assert!(v["warning"].is_null());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A host whose telemetry agents can be stopped (or refuse to).
    struct AgentHost {
        stopped: std::sync::Mutex<u32>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl amicu_llamacpp::ProcessSupervisor for AgentHost {
        async fn start(
            &self,
            _: &amicu_runtime::LaunchPlan,
            _: &[amicu_llamacpp::ResolvedSecret],
        ) -> Result<amicu_runtime::RuntimeHandle, amicu_llamacpp::LlamaCppError> {
            unreachable!()
        }
        async fn status(
            &self,
            _: &amicu_runtime::RuntimeHandle,
        ) -> Result<amicu_runtime::RuntimeStatus, amicu_llamacpp::LlamaCppError> {
            unreachable!()
        }
        async fn metrics(
            &self,
            _: &amicu_runtime::RuntimeHandle,
        ) -> Result<amicu_runtime::RuntimeMetrics, amicu_llamacpp::LlamaCppError> {
            unreachable!()
        }
        async fn logs(
            &self,
            _: &amicu_runtime::RuntimeHandle,
            _: amicu_runtime::LogOptions,
        ) -> Result<amicu_runtime::LogStream, amicu_llamacpp::LlamaCppError> {
            unreachable!()
        }
        async fn stop(
            &self,
            _: &amicu_runtime::RuntimeHandle,
        ) -> Result<(), amicu_llamacpp::LlamaCppError> {
            unreachable!()
        }
        async fn stop_all_sidecars(&self) -> Result<Vec<String>, amicu_llamacpp::LlamaCppError> {
            *self.stopped.lock().unwrap() += 1;
            if self.fail {
                Err(amicu_llamacpp::LlamaCppError::Supervisor(
                    "still loaded".into(),
                ))
            } else {
                Ok(vec!["io.amicu.telemetry.a1b2c3d4.chat".into()])
            }
        }
    }

    #[test]
    fn disable_stops_every_agent_and_deletes_the_queues_right_away() {
        let (store, dir) = temp_store("disable");
        store.save(true).unwrap();
        let host = AgentHost {
            stopped: std::sync::Mutex::new(0),
            fail: false,
        };
        let cleared = std::sync::atomic::AtomicBool::new(false);
        let mut out = Vec::new();
        let code = disable_with(
            &store,
            &env_of(&[]),
            &host,
            &|| cleared.store(true, std::sync::atomic::Ordering::SeqCst),
            &mut out,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(store.load().unwrap(), Some(false));
        assert_eq!(*host.stopped.lock().unwrap(), 1);
        assert!(cleared.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("Stopped 1 telemetry agent(s).")
        );
        // A stop that fails is reported and the command fails.
        let stuck = AgentHost {
            stopped: std::sync::Mutex::new(0),
            fail: true,
        };
        let mut out = Vec::new();
        assert_eq!(
            disable_with(&store, &env_of(&[]), &stuck, &|| {}, &mut out).unwrap(),
            1
        );
        assert!(String::from_utf8(out).unwrap().contains("still loaded"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn opting_out_deletes_buffered_rollups() {
        let (_, dir) = temp_store("queues");
        let q = crate::commands::telemetry_agent::queue_dir(&dir);
        std::fs::create_dir_all(&q).unwrap();
        std::fs::write(q.join("d.jsonl"), "{}\n").unwrap();
        clear_queues(Some(&dir));
        assert!(!q.exists());
        clear_queues(Some(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn status_degrades_on_corrupt_config() {
        let (store, dir) = temp_store("corrupt");
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), "{not json").unwrap();
        let env = env_of(&[]);

        let mut human = Vec::new();
        assert_eq!(
            status_with(
                Some(&store),
                &env,
                NO_AGENTS,
                OutputFormat::Human,
                &mut human
            )
            .unwrap(),
            0
        );
        let human = String::from_utf8(human).unwrap();
        assert!(human.contains("telemetry: disabled"));
        assert!(human.contains("config unreadable (treated as not set)"));
        assert!(human.contains("warning:"));

        let mut buf = Vec::new();
        status_with(Some(&store), &env, NO_AGENTS, OutputFormat::Json, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["source"], "config_unreadable");
        assert!(v["warning"].is_string());

        // The consent prompt still treats a corrupt file as already asked.
        assert!(!ask(&store, &[], true, "y").0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn status_json_and_env_override_note() {
        let (store, dir) = temp_store("json");
        store.save(true).unwrap();
        let v = status_json(&store, &[("DO_NOT_TRACK", "1")], NO_AGENTS);
        assert_eq!(v["enabled"], false);
        assert_eq!(v["source"], "do_not_track");
        assert_eq!(v["collecting"], false);
        assert_eq!(v["note"], NOT_COLLECTING);

        let v = status_json(&store, &[], TWO_AGENTS);
        assert_eq!(
            v["collecting"], true,
            "opted in with agents: collected and buffered"
        );
        assert_eq!(v["note"], COLLECTING);

        let mut out = Vec::new();
        set(&store, true, &env_of(&[("DO_NOT_TRACK", "1")]), &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("overrides"));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn ask(
        store: &ConfigStore,
        env: &[(&str, &str)],
        interactive: bool,
        answer: &str,
    ) -> (bool, String) {
        let mut out = Vec::new();
        let shown = maybe_prompt(
            store,
            &env_of(env),
            interactive,
            &mut answer.as_bytes(),
            &mut out,
        );
        (shown, String::from_utf8(out).unwrap())
    }

    #[test]
    fn prompt_default_is_no_and_asks_once() {
        let (store, dir) = temp_store("once");
        let (shown, text) = ask(&store, &[], true, "\n");
        assert!(shown);
        assert!(text.contains("latency, throughput"));
        assert!(text.contains("never sends prompts, completions, secrets or IP"));
        assert!(text.contains("amicu telemetry enable|disable"));
        assert_eq!(store.load().unwrap(), Some(false));
        let (shown_again, _) = ask(&store, &[], true, "y\n");
        assert!(!shown_again);
        assert_eq!(store.load().unwrap(), Some(false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prompt_yes_is_persisted() {
        let (store, dir) = temp_store("yes");
        assert!(ask(&store, &[], true, "Yes\n").0);
        assert_eq!(store.load().unwrap(), Some(true));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prompt_eof_means_no() {
        let (store, dir) = temp_store("eof");
        assert!(ask(&store, &[], true, "").0);
        assert_eq!(store.load().unwrap(), Some(false));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prompt_never_shown_when_non_interactive_ci_or_env_set() {
        let (store, dir) = temp_store("never");
        assert!(!ask(&store, &[], false, "y\n").0, "non-TTY");
        for ci in ["CI", "GITHUB_ACTIONS", "GITLAB_CI"] {
            assert!(!ask(&store, &[(ci, "true")], true, "y\n").0, "{ci}");
        }
        assert!(!ask(&store, &[("AMICU_TELEMETRY", "0")], true, "y\n").0);
        assert!(!ask(&store, &[("AMICU_TELEMETRY", "1")], true, "y\n").0);
        assert!(!ask(&store, &[("DO_NOT_TRACK", "1")], true, "y\n").0);
        assert_eq!(store.load().unwrap(), None, "nothing persisted");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prompt_skips_when_config_unreadable() {
        let (store, dir) = temp_store("corrupt");
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), "not json").unwrap();
        assert!(!ask(&store, &[], true, "y\n").0);
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), "not json");
        let _ = std::fs::remove_dir_all(dir);
    }
}
