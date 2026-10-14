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
//! NOTHING IS COLLECTED OR SENT YET. This module only records consent so the
//! setting exists before any collection ships; it makes no network calls.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::OutputFormat;
use crate::error::CliError;
use crate::write_json;

const NOT_COLLECTING: &str =
    "No telemetry data is collected or sent yet; this setting only records your choice.";

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
            "cannot locate a user config directory (set HOME or XDG_CONFIG_HOME)".to_owned(),
        )
    })
}

/// `amicu telemetry enable`.
pub(crate) fn enable(out: &mut dyn Write) -> Result<u8, CliError> {
    set(&store_or_err()?, true, &real_env, out)
}

/// `amicu telemetry disable`.
pub(crate) fn disable(out: &mut dyn Write) -> Result<u8, CliError> {
    set(&store_or_err()?, false, &real_env, out)
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
    writeln!(out, "{NOT_COLLECTING}").map_err(io_err)?;
    Ok(0)
}

#[derive(Serialize)]
struct StatusReport {
    version: &'static str,
    enabled: bool,
    source: SourceKind,
    config_path: Option<String>,
    collecting: bool,
    note: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

/// `amicu telemetry status`.
pub(crate) fn status(output: OutputFormat, out: &mut dyn Write) -> Result<u8, CliError> {
    let store = ConfigStore::host();
    status_with(store.as_ref(), &real_env, output, out)
}

fn status_with(
    store: Option<&ConfigStore>,
    env: EnvGet<'_>,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    // A corrupt or unreadable file must not make `status` fail: degrade to the
    // default (off) and say so. Env overrides still win via `effective`.
    let mut warning = None;
    let stored = match store.map(ConfigStore::load) {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            warning = Some(format!("telemetry config is unreadable: {e}"));
            None
        }
        None => None,
    };
    let mut eff = effective(env, stored);
    if warning.is_some() && eff.source == SourceKind::Default {
        eff.source = SourceKind::ConfigUnreadable;
    }
    if output.is_json() {
        write_json(
            out,
            &StatusReport {
                version: "v1",
                enabled: eff.enabled,
                source: eff.source,
                config_path: store.map(|s| s.path().display().to_string()),
                collecting: false,
                note: NOT_COLLECTING,
                warning,
            },
        )?;
        return Ok(0);
    }
    writeln!(
        out,
        "telemetry: {}\nsource:    {}",
        if eff.enabled { "enabled" } else { "disabled" },
        eff.source.describe()
    )
    .map_err(io_err)?;
    if let Some(s) = store {
        writeln!(out, "config:    {}", s.path().display()).map_err(io_err)?;
    }
    if let Some(w) = &warning {
        writeln!(out, "warning:   {w}").map_err(io_err)?;
    }
    writeln!(out, "\n{NOT_COLLECTING}").map_err(io_err)?;
    Ok(0)
}

// ---- first-run consent prompt -----------------------------------------------

const PROMPT: &str = "\
Help improve amicu? (one-time question)

If you opt in, amicu will send aggregate performance metrics from your
deployments: latency, throughput, hardware and model identifiers.
It never sends prompts, completions, secrets or IP addresses.

You can change this any time with `amicu telemetry enable|disable`
(or set AMICU_TELEMETRY=0/1; DO_NOT_TRACK=1 always turns it off).
Nothing is collected or sent yet; this only records your choice.

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

    #[test]
    fn enable_disable_status_round_trip() {
        let (store, dir) = temp_store("rt");
        let env = env_of(&[]);
        let mut out = Vec::new();
        set(&store, true, &env, &mut out).unwrap();
        assert_eq!(store.load().unwrap(), Some(true));
        let mut s = Vec::new();
        status_with(Some(&store), &env, OutputFormat::Human, &mut s).unwrap();
        let s = String::from_utf8(s).unwrap();
        assert!(s.contains("telemetry: enabled"));
        assert!(s.contains("user config file"));
        assert!(s.contains("No telemetry data is collected or sent yet"));

        set(&store, false, &env, &mut Vec::new()).unwrap();
        assert_eq!(store.load().unwrap(), Some(false));
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
            status_with(Some(&store), &env, OutputFormat::Human, &mut human).unwrap(),
            0
        );
        let human = String::from_utf8(human).unwrap();
        assert!(human.contains("telemetry: disabled"));
        assert!(human.contains("config unreadable (treated as not set)"));
        assert!(human.contains("warning:"));

        let mut buf = Vec::new();
        status_with(Some(&store), &env, OutputFormat::Json, &mut buf).unwrap();
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
        let env = env_of(&[("DO_NOT_TRACK", "1")]);
        let mut buf = Vec::new();
        status_with(Some(&store), &env, OutputFormat::Json, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["source"], "do_not_track");
        assert_eq!(v["collecting"], false);

        let mut out = Vec::new();
        set(&store, true, &env, &mut out).unwrap();
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
