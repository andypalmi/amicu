//! `amicu orphans [--output json]` - list amicu-owned resources that this
//! project's state does not record (amicu-planning#213).
//!
//! Strictly read-only: it enumerates Modal apps (`amicu-` prefix), launchd
//! jobs/plists (`io.amicu.` prefix) and systemd `--user` units, marks each as
//! tracked or not by comparing against what `.amicu/state/state.json`
//! references (every stop record and every endpoint), and prints the exact
//! manual command that stops it. It never stops, imports or edits anything.
//!
//! Host labels are either project-scoped (`io.amicu.llama.<8hex>.<dep>`) or
//! legacy (`io.amicu.llama.<dep>`). Only an untracked label scoped to *this*
//! project is reported as a likely orphan; every other untracked resource
//! (another project's scoped label, a legacy label, a Modal app) is only
//! "not tracked by this project" and may belong to another project. Modal
//! apps are matched by app id and by name.
//!
//! Every external lookup goes through an injectable seam ([`CommandExec`] for
//! `launchctl`/`systemctl`, [`ModalApps`] for Modal, a directory path for the
//! plist scan) so tests never touch the real host.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use amicu_llamacpp::{CommandExec, HostExec};
use amicu_modal::{Credentials, ModalClient, ModalError};
use serde::Serialize;

use crate::OutputFormat;
use crate::commands::build_runtime;
use crate::error::CliError;
use crate::project::{LocalState, Project};
use crate::write_json;

const MODAL_PREFIX: &str = "amicu-";
const LAUNCHD_PREFIX: &str = "io.amicu.";

const LLAMA_PREFIX: &str = "io.amicu.llama.";

const SCOPE_NOTE: &str = "Not tracked by this project does not mean orphaned: resources of other \
projects on this machine or Modal workspace appear here too. Only labels scoped to this project \
are reported as likely orphans. Check before stopping.";

/// Whose a resource most plausibly is, judged from its name alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Ownership {
    /// A label scoped to this project's id: nothing else should own it.
    ThisProject,
    /// A label scoped to a different project id.
    OtherProject,
    /// A legacy unscoped label (`io.amicu.llama.<dep>`): any project's.
    Legacy,
    /// Not decidable from the name (Modal apps, plain units).
    Unknown,
}

impl Ownership {
    fn describe(self) -> &'static str {
        match self {
            Ownership::ThisProject => "scoped to this project, likely an orphan",
            Ownership::OtherProject => "scoped to another project, may belong to it",
            Ownership::Legacy => "legacy unscoped label, may belong to another project",
            Ownership::Unknown => "may belong to another project",
        }
    }
}

/// Classify a host label (a `.service` suffix is ignored).
fn ownership_of_label(label: &str, project_id: Option<&str>) -> Ownership {
    let label = label.strip_suffix(".service").unwrap_or(label);
    let Some(rest) = label.strip_prefix(LLAMA_PREFIX) else {
        return Ownership::Unknown;
    };
    match rest.split_once('.') {
        Some((scope, dep))
            if !dep.is_empty()
                && scope.len() == 8
                && scope
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
        {
            if project_id == Some(scope) {
                Ownership::ThisProject
            } else {
                Ownership::OtherProject
            }
        }
        _ => Ownership::Legacy,
    }
}

/// Which resource family an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    ModalApp,
    LaunchdJob,
    SystemdUnit,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::ModalApp => "modal app",
            Kind::LaunchdJob => "launchd job",
            Kind::SystemdUnit => "systemd unit",
        }
    }
}

/// One amicu-owned resource found on the host or workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Found {
    pub kind: Kind,
    pub identity: String,
    /// Backend-reported state (`deployed`, `running`, `plist only`, ...).
    pub state: String,
    /// Whether this project's state references it (stop record or endpoint).
    pub tracked: bool,
    /// Whose it most plausibly is (meaningful when not tracked).
    pub ownership: Ownership,
    /// The exact command that stops it by hand.
    pub stop_command: String,
}

/// Whether one backend could be queried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Source {
    pub name: &'static str,
    /// `ok`, `skipped` (not applicable here) or `error`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The full report (`--output json` shape).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OrphansReport {
    pub version: &'static str,
    pub sources: Vec<Source>,
    pub resources: Vec<Found>,
    pub note: &'static str,
}

/// A Modal app as far as discovery cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModalApp {
    pub id: String,
    pub name: String,
    pub state: String,
}

/// Injectable read-only Modal app lookup.
pub(crate) trait ModalApps {
    fn list(&self) -> Result<Vec<ModalApp>, String>;
}

/// The real lookup: `ModalClient::list_apps` (`AppList`), read-only.
struct RealModal;

impl ModalApps for RealModal {
    fn list(&self) -> Result<Vec<ModalApp>, String> {
        let run = || -> Result<Vec<ModalApp>, CliError> {
            let credentials = Credentials::from_env().map_err(ModalError::from)?;
            let rt = build_runtime()?;
            let apps = rt.block_on(async {
                let mut client = ModalClient::connect(credentials).await?;
                client.list_apps().await
            })?;
            Ok(apps
                .into_iter()
                .map(|a| ModalApp {
                    id: a.app_id,
                    name: a.name,
                    state: a.state,
                })
                .collect())
        };
        run().map_err(|e| e.to_string())
    }
}

/// The host platform, injectable for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    MacOs,
    Linux,
    Other,
}

impl Platform {
    fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Platform::MacOs,
            "linux" => Platform::Linux,
            _ => Platform::Other,
        }
    }
}

/// Everything discovery reads from, all injectable.
pub(crate) struct Env<'a> {
    pub platform: Platform,
    pub exec: &'a dyn CommandExec,
    pub modal: &'a dyn ModalApps,
    /// `~/Library/LaunchAgents` (macOS plist scan).
    pub agents_dir: Option<PathBuf>,
}

/// Identities this project's state references, per backend.
#[derive(Debug, Default)]
pub(crate) struct Tracked {
    project_id: Option<String>,
    modal_names: BTreeSet<String>,
    modal_ids: BTreeSet<String>,
    labels: BTreeSet<String>,
}

impl Tracked {
    /// Everything state references: all stop records (Modal name and app id,
    /// host label) and all endpoint handles (whose opaque id may be a label,
    /// a Modal app id or a Modal app name, so it counts for each).
    pub(crate) fn from_state(state: Option<&LocalState>) -> Self {
        let mut t = Self::default();
        let Some(state) = state else { return t };
        t.project_id.clone_from(&state.project_id);
        for rec in &state.stops {
            if rec.backend.is_modal() {
                t.modal_names.insert(rec.identity.clone());
                t.modal_ids.extend(rec.app_id.clone());
            } else {
                t.insert_label(&rec.identity);
            }
        }
        for ep in &state.endpoints {
            t.modal_names.insert(ep.id.clone());
            t.modal_ids.insert(ep.id.clone());
            t.insert_label(&ep.id);
        }
        t
    }

    fn insert_label(&mut self, label: &str) {
        self.labels
            .insert(label.strip_suffix(".service").unwrap_or(label).to_owned());
    }

    fn has_label(&self, label: &str) -> bool {
        self.labels
            .contains(label.strip_suffix(".service").unwrap_or(label))
    }
}

/// Discover untracked (and tracked) amicu resources. Never mutates anything.
pub(crate) fn discover(env: &Env<'_>, tracked: &Tracked) -> OrphansReport {
    let mut sources = Vec::new();
    let mut resources = Vec::new();

    match env.modal.list() {
        Ok(apps) => {
            sources.push(source("modal", "ok", None));
            for app in apps {
                // Stopped apps no longer run or bill; empty names are
                // ephemeral/anonymous apps that are not amicu's.
                if !app.name.starts_with(MODAL_PREFIX) || app.state == "stopped" {
                    continue;
                }
                resources.push(Found {
                    kind: Kind::ModalApp,
                    tracked: tracked.modal_names.contains(&app.name)
                        || tracked.modal_ids.contains(&app.id),
                    ownership: Ownership::Unknown,
                    stop_command: format!("amicu modal stop {}", shell_quote(&app.name)),
                    identity: app.name,
                    state: app.state,
                });
            }
        }
        Err(e) => sources.push(source("modal", "error", Some(e))),
    }

    match env.platform {
        Platform::MacOs => {
            match launchd(env, tracked) {
                Ok(found) => {
                    sources.push(source("launchd", "ok", None));
                    resources.extend(found);
                }
                Err(e) => sources.push(source("launchd", "error", Some(e))),
            }
            sources.push(source("systemd", "skipped", Some("not Linux".to_owned())));
        }
        Platform::Linux => {
            sources.push(source("launchd", "skipped", Some("not macOS".to_owned())));
            match systemd(env, tracked) {
                Ok(found) => {
                    sources.push(source("systemd", "ok", None));
                    resources.extend(found);
                }
                Err(e) => sources.push(source("systemd", "error", Some(e))),
            }
        }
        Platform::Other => {
            let why = "unsupported platform".to_owned();
            sources.push(source("launchd", "skipped", Some(why.clone())));
            sources.push(source("systemd", "skipped", Some(why)));
        }
    }

    resources.sort_by(|a, b| {
        (a.tracked, a.ownership, a.kind, &a.identity).cmp(&(
            b.tracked,
            b.ownership,
            b.kind,
            &b.identity,
        ))
    });
    OrphansReport {
        version: "v1",
        sources,
        resources,
        note: SCOPE_NOTE,
    }
}

fn source(name: &'static str, status: &'static str, detail: Option<String>) -> Source {
    Source {
        name,
        status,
        detail,
    }
}

/// POSIX single-quote escape for a value interpolated into a printed shell
/// command. Plain tokens stay bare for readability; everything else is wrapped
/// in `'...'` with each `'` written as `'\''`.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/@:+=-".contains(c));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn plist_path(dir: Option<&Path>, label: &str) -> String {
    dir.map_or_else(
        || format!("~/Library/LaunchAgents/{label}.plist"),
        |d| d.join(format!("{label}.plist")).display().to_string(),
    )
}

fn launchd(env: &Env<'_>, tracked: &Tracked) -> Result<Vec<Found>, String> {
    // label -> state
    let mut labels: BTreeMap<String, String> = BTreeMap::new();

    let out = env
        .exec
        .exec("launchctl", &["list"])
        .map_err(|e| e.to_string())?;
    if !out.success {
        return Err(format!("launchctl list failed: {}", out.stderr.trim()));
    }
    // `PID<TAB>Status<TAB>Label`, PID is `-` when not running.
    for line in out.stdout.lines() {
        let mut cols = line.split_whitespace();
        let (Some(pid), Some(_status), Some(label)) = (cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        if label.starts_with(LAUNCHD_PREFIX) {
            let state = if pid == "-" {
                "loaded, not running".to_owned()
            } else {
                format!("running (pid {pid})")
            };
            labels.insert(label.to_owned(), state);
        }
    }

    // Plists on disk that are not currently loaded.
    if let Some(dir) = &env.agents_dir
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(label) = name.strip_suffix(".plist")
                && label.starts_with(LAUNCHD_PREFIX)
            {
                labels
                    .entry(label.to_owned())
                    .or_insert_with(|| "plist only (not loaded)".to_owned());
            }
        }
    }

    Ok(labels
        .into_iter()
        .map(|(label, state)| {
            let plist = plist_path(env.agents_dir.as_deref(), &label);
            Found {
                kind: Kind::LaunchdJob,
                tracked: tracked.has_label(&label),
                ownership: ownership_of_label(&label, tracked.project_id.as_deref()),
                stop_command: {
                    let q = shell_quote(&plist);
                    format!("launchctl unload -w {q} && rm {q}")
                },
                identity: label,
                state,
            }
        })
        .collect())
}

fn systemd(env: &Env<'_>, tracked: &Tracked) -> Result<Vec<Found>, String> {
    // Amicu's own units are `io.amicu.llama.<id>`; `amicu*` covers any plainly
    // named unit.
    let out = env
        .exec
        .exec(
            "systemctl",
            &[
                "--user",
                "list-units",
                "io.amicu.*",
                "amicu*",
                "--all",
                "--no-legend",
                "--plain",
                "--no-pager",
            ],
        )
        .map_err(|e| e.to_string())?;
    if !out.success {
        return Err(format!(
            "systemctl list-units failed: {}",
            out.stderr.trim()
        ));
    }
    let mut found = Vec::new();
    for line in out.stdout.lines() {
        // UNIT LOAD ACTIVE SUB DESCRIPTION...
        let cols: Vec<&str> = line.split_whitespace().collect();
        let Some(unit) = cols.first() else { continue };
        if !(unit.starts_with("amicu") || unit.starts_with(LAUNCHD_PREFIX)) {
            continue;
        }
        let state = match (cols.get(2), cols.get(3)) {
            (Some(active), Some(sub)) => format!("{active} ({sub})"),
            _ => "unknown".to_owned(),
        };
        let label = unit.strip_suffix(".service").unwrap_or(unit);
        found.push(Found {
            kind: Kind::SystemdUnit,
            tracked: tracked.has_label(label),
            ownership: ownership_of_label(label, tracked.project_id.as_deref()),
            stop_command: format!("systemctl --user stop {}", shell_quote(unit)),
            identity: (*unit).to_owned(),
            state,
        });
    }
    Ok(found)
}

/// Run discovery against the real host and render the result.
pub(crate) fn run(
    project: &Project,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let state = project.load_state()?;
    let tracked = Tracked::from_state(state.as_ref());
    let agents_dir = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join("Library/LaunchAgents"));
    let env = Env {
        platform: Platform::current(),
        exec: &HostExec,
        modal: &RealModal,
        agents_dir,
    };
    render(&discover(&env, &tracked), output, out)
}

fn render(
    report: &OrphansReport,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    if output.is_json() {
        write_json(out, report)?;
        return Ok(0);
    }
    let io = |r: std::io::Result<()>| r.map_err(|e| CliError::io("write failed", e));
    for s in &report.sources {
        if let ("error", Some(d)) = (s.status, &s.detail) {
            io(writeln!(out, "warning: could not query {}: {d}", s.name))?;
        }
    }
    let untracked: Vec<&Found> = report.resources.iter().filter(|r| !r.tracked).collect();
    let tracked_n = report.resources.len() - untracked.len();
    if untracked.is_empty() {
        io(writeln!(out, "No untracked amicu resources found."))?;
    } else {
        io(writeln!(
            out,
            "{} amicu resource(s) not tracked by this project (no stop record or endpoint in state):\n",
            untracked.len()
        ))?;
        for r in &untracked {
            io(writeln!(
                out,
                "  {:<12} {}  [{}]",
                r.kind.label(),
                r.identity,
                r.state
            ))?;
            io(writeln!(out, "               {}", r.ownership.describe()))?;
            io(writeln!(out, "               stop: {}", r.stop_command))?;
        }
    }
    if tracked_n > 0 {
        io(writeln!(
            out,
            "\n{tracked_n} tracked by this project (not listed)."
        ))?;
    }
    io(writeln!(out, "\nNote: {}", report.note))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stops::StopRecord;
    use amicu_llamacpp::{ExecOutput, LlamaCppError};
    use amicu_runtime::RuntimeHandle;
    use std::sync::Mutex;

    #[test]
    fn shell_quote_leaves_plain_and_escapes_unsafe() {
        assert_eq!(shell_quote("io.amicu.llama.x"), "io.amicu.llama.x");
        assert_eq!(shell_quote("/home/a/x.plist"), "/home/a/x.plist");
        assert_eq!(shell_quote("/Users/a b/x.plist"), "'/Users/a b/x.plist'");
        assert_eq!(shell_quote("x;touch y"), "'x;touch y'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    struct FakeExec {
        calls: Mutex<Vec<String>>,
        launchctl: Result<String, String>,
        systemctl: Result<String, String>,
    }

    impl FakeExec {
        fn new(launchctl: &str, systemctl: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                launchctl: Ok(launchctl.to_owned()),
                systemctl: Ok(systemctl.to_owned()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandExec for FakeExec {
        fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, LlamaCppError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            let r = match program {
                "launchctl" => &self.launchctl,
                "systemctl" => &self.systemctl,
                other => panic!("unexpected program {other}"),
            };
            Ok(match r {
                Ok(stdout) => ExecOutput {
                    success: true,
                    stdout: stdout.clone(),
                    stderr: String::new(),
                    code: Some(0),
                },
                Err(stderr) => ExecOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: stderr.clone(),
                    code: Some(1),
                },
            })
        }
    }

    struct FakeModal(Result<Vec<ModalApp>, String>);

    impl ModalApps for FakeModal {
        fn list(&self) -> Result<Vec<ModalApp>, String> {
            self.0.clone()
        }
    }

    fn app(name: &str, state: &str) -> ModalApp {
        app_id(name, &format!("ap-{name}"), state)
    }

    fn app_id(name: &str, id: &str, state: &str) -> ModalApp {
        ModalApp {
            id: id.to_owned(),
            name: name.to_owned(),
            state: state.to_owned(),
        }
    }

    const PID: &str = "a1b2c3d4";

    fn state_with(stops: Vec<StopRecord>, endpoints: Vec<RuntimeHandle>) -> LocalState {
        LocalState {
            version: LocalState::VERSION.to_owned(),
            project_id: Some(PID.to_owned()),
            graph: amicu_core::graph::Graph::new(),
            endpoints,
            pending: Vec::new(),
            stops,
            fingerprints: BTreeMap::default(),
        }
    }

    fn host_stop(label: &str) -> StopRecord {
        let mut r = StopRecord::host(&"x".into(), None);
        r.identity = label.to_owned();
        r
    }

    fn modal_stop(name: &str, app_id: Option<&str>) -> StopRecord {
        let mut r = StopRecord::modal(&"x".into(), app_id.map(str::to_owned), None);
        r.identity = name.to_owned();
        r
    }

    fn endpoint(id: &str) -> RuntimeHandle {
        RuntimeHandle {
            id: id.to_owned(),
            deployment: "x".into(),
            endpoint: "http://127.0.0.1:1".to_owned(),
            exposure: amicu_runtime::EndpointExposure::Loopback,
        }
    }

    fn tracked_of(stops: Vec<StopRecord>) -> Tracked {
        Tracked::from_state(Some(&state_with(stops, Vec::new())))
    }

    fn env<'a>(
        platform: Platform,
        exec: &'a FakeExec,
        modal: &'a FakeModal,
        agents_dir: Option<PathBuf>,
    ) -> Env<'a> {
        Env {
            platform,
            exec,
            modal,
            agents_dir,
        }
    }

    fn tracked_chat() -> Tracked {
        tracked_of(vec![
            modal_stop("amicu-chat", None),
            host_stop(&format!("io.amicu.llama.{PID}.chat")),
        ])
    }

    #[test]
    fn modal_lists_prefixed_apps_and_marks_tracked() {
        let modal = FakeModal(Ok(vec![
            app("amicu-chat", "deployed"),
            app("amicu-old", "deployed"),
            app("amicu-gone", "stopped"),
            app("other-app", "deployed"),
            app("", "ephemeral"),
        ]));
        let exec = FakeExec::new("", "");
        let tracked = tracked_chat();
        let report = discover(&env(Platform::Other, &exec, &modal, None), &tracked);
        let got: Vec<_> = report
            .resources
            .iter()
            .map(|r| (r.identity.as_str(), r.tracked))
            .collect();
        assert_eq!(got, [("amicu-old", false), ("amicu-chat", true)]);
        assert_eq!(
            report.resources[0].stop_command,
            "amicu modal stop amicu-old"
        );
        assert!(exec.calls().is_empty(), "no commands on Other");
    }

    #[test]
    fn modal_error_is_reported_not_fatal() {
        let modal = FakeModal(Err("no credentials".to_owned()));
        let exec = FakeExec::new("", "");
        let report = discover(
            &env(Platform::Other, &exec, &modal, None),
            &Tracked::default(),
        );
        assert_eq!(report.resources.len(), 0);
        assert_eq!(report.sources[0].status, "error");
        let mut buf = Vec::new();
        render(&report, OutputFormat::Human, &mut buf).unwrap();
        assert!(String::from_utf8(buf).unwrap().contains("no credentials"));
    }

    #[test]
    fn launchd_merges_loaded_jobs_and_plists() {
        let dir = std::env::temp_dir().join(format!("amicu-orphans-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("io.amicu.llama.disk.plist"), "x").unwrap();
        std::fs::write(dir.join(format!("io.amicu.llama.{PID}.chat.plist")), "x").unwrap();
        std::fs::write(dir.join("com.apple.thing.plist"), "x").unwrap();
        let launchctl = format!(
            "PID\tStatus\tLabel\n123\t0\tio.amicu.llama.{PID}.chat\n-\t0\tio.amicu.llama.stale\n77\t0\tcom.apple.x\n"
        );
        let exec = FakeExec::new(&launchctl, "");
        let modal = FakeModal(Ok(vec![]));
        let report = discover(
            &env(Platform::MacOs, &exec, &modal, Some(dir.clone())),
            &tracked_chat(),
        );
        std::fs::remove_dir_all(&dir).unwrap();
        let got: Vec<_> = report
            .resources
            .iter()
            .map(|r| (r.identity.as_str(), r.tracked, r.state.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("io.amicu.llama.disk", false, "plist only (not loaded)"),
                ("io.amicu.llama.stale", false, "loaded, not running"),
                ("io.amicu.llama.a1b2c3d4.chat", true, "running (pid 123)"),
            ]
        );
        assert!(
            report.resources[0]
                .stop_command
                .starts_with("launchctl unload -w ")
        );
        assert_eq!(exec.calls(), ["launchctl list"]);
    }

    #[test]
    fn launchctl_failure_is_a_source_error() {
        let exec = FakeExec {
            calls: Mutex::new(Vec::new()),
            launchctl: Err("boom".to_owned()),
            systemctl: Ok(String::new()),
        };
        let modal = FakeModal(Ok(vec![]));
        let report = discover(
            &env(Platform::MacOs, &exec, &modal, None),
            &Tracked::default(),
        );
        let launchd = report.sources.iter().find(|s| s.name == "launchd").unwrap();
        assert_eq!(launchd.status, "error");
    }

    #[test]
    fn systemd_lists_units_and_marks_tracked() {
        let systemctl = "io.amicu.llama.a1b2c3d4.chat.service loaded active running amicu chat\n\
io.amicu.llama.lost.service loaded failed failed amicu lost\n\
amicu-extra.service loaded inactive dead extra\n\
unrelated.service loaded active running nope\n";
        let exec = FakeExec::new("", systemctl);
        let modal = FakeModal(Ok(vec![]));
        let report = discover(&env(Platform::Linux, &exec, &modal, None), &tracked_chat());
        let got: Vec<_> = report
            .resources
            .iter()
            .map(|r| (r.identity.as_str(), r.tracked, r.state.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("io.amicu.llama.lost.service", false, "failed (failed)"),
                ("amicu-extra.service", false, "inactive (dead)"),
                (
                    "io.amicu.llama.a1b2c3d4.chat.service",
                    true,
                    "active (running)"
                ),
            ]
        );
        assert_eq!(
            report.resources[0].stop_command,
            "systemctl --user stop io.amicu.llama.lost.service"
        );
        assert!(exec.calls()[0].starts_with("systemctl --user list-units"));
    }

    #[test]
    fn output_is_read_only_and_json_has_note() {
        let exec = FakeExec::new("", "");
        let modal = FakeModal(Ok(vec![app("amicu-x", "deployed")]));
        let report = discover(
            &env(Platform::Linux, &exec, &modal, None),
            &Tracked::default(),
        );
        assert!(
            exec.calls().iter().all(|c| c.contains("list")),
            "only list commands may run"
        );
        let mut buf = Vec::new();
        render(&report, OutputFormat::Json, &mut buf).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["version"], "v1");
        assert_eq!(v["resources"][0]["identity"], "amicu-x");
        assert_eq!(v["resources"][0]["kind"], "modal_app");
        assert_eq!(v["resources"][0]["tracked"], false);
        assert!(v["note"].as_str().unwrap().contains("Not tracked"));

        let mut human = Vec::new();
        render(&report, OutputFormat::Human, &mut human).unwrap();
        let human = String::from_utf8(human).unwrap();
        assert!(human.contains("stop: amicu modal stop amicu-x"));
        assert!(human.contains("may belong to another project"));
    }

    #[test]
    fn ownership_classifies_scoped_other_and_legacy_labels() {
        let me = Some(PID);
        let o = |l| ownership_of_label(l, me);
        assert_eq!(o("io.amicu.llama.a1b2c3d4.chat"), Ownership::ThisProject);
        assert_eq!(
            o("io.amicu.llama.a1b2c3d4.chat.service"),
            Ownership::ThisProject
        );
        assert_eq!(o("io.amicu.llama.deadbeef.chat"), Ownership::OtherProject);
        assert_eq!(o("io.amicu.llama.chat"), Ownership::Legacy);
        // An 8-hex legacy deployment id has no deployment part after it.
        assert_eq!(o("io.amicu.llama.deadbeef"), Ownership::Legacy);
        assert_eq!(o("io.amicu.llama.deadbeef.service"), Ownership::Legacy);
        // Uppercase or wrong-length scopes are not project ids.
        assert_eq!(o("io.amicu.llama.A1B2C3D4.chat"), Ownership::Legacy);
        assert_eq!(o("amicu-extra"), Ownership::Unknown);
        // State without a project id (v1/v2): no label is ever "this project's".
        assert_eq!(
            ownership_of_label("io.amicu.llama.a1b2c3d4.chat", None),
            Ownership::OtherProject
        );
    }

    #[test]
    fn modal_apps_match_by_id_and_by_name() {
        let modal = FakeModal(Ok(vec![
            app_id("amicu-by-name", "ap-1", "deployed"),
            app_id("amicu-renamed", "ap-2", "deployed"),
            app_id("amicu-by-endpoint", "ap-3", "deployed"),
            app_id("amicu-stray", "ap-4", "deployed"),
        ]));
        let exec = FakeExec::new("", "");
        let state = state_with(
            vec![
                modal_stop("amicu-by-name", None),
                modal_stop("amicu-old-name", Some("ap-2")),
            ],
            vec![endpoint("ap-3")],
        );
        let report = discover(
            &env(Platform::Other, &exec, &modal, None),
            &Tracked::from_state(Some(&state)),
        );
        let got: Vec<_> = report
            .resources
            .iter()
            .map(|r| (r.identity.as_str(), r.tracked))
            .collect();
        assert_eq!(
            got,
            [
                ("amicu-stray", false),
                ("amicu-by-endpoint", true),
                ("amicu-by-name", true),
                ("amicu-renamed", true),
            ]
        );
        assert_eq!(report.resources[0].ownership, Ownership::Unknown);
    }

    #[test]
    fn only_this_projects_scoped_labels_are_likely_orphans() {
        let systemctl = "io.amicu.llama.a1b2c3d4.mine.service loaded active running a\n\
io.amicu.llama.deadbeef.theirs.service loaded active running b\n\
io.amicu.llama.old.service loaded active running c\n";
        let exec = FakeExec::new("", systemctl);
        let modal = FakeModal(Ok(vec![]));
        let report = discover(
            &env(Platform::Linux, &exec, &modal, None),
            &Tracked::from_state(Some(&state_with(Vec::new(), Vec::new()))),
        );
        let got: Vec<_> = report
            .resources
            .iter()
            .map(|r| (r.identity.as_str(), r.ownership))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "io.amicu.llama.a1b2c3d4.mine.service",
                    Ownership::ThisProject
                ),
                (
                    "io.amicu.llama.deadbeef.theirs.service",
                    Ownership::OtherProject
                ),
                ("io.amicu.llama.old.service", Ownership::Legacy),
            ]
        );
        let mut human = Vec::new();
        render(&report, OutputFormat::Human, &mut human).unwrap();
        let human = String::from_utf8(human).unwrap();
        assert!(human.contains("scoped to this project, likely an orphan"));
        assert!(human.contains("scoped to another project, may belong to it"));
        assert!(human.contains("legacy unscoped label, may belong to another project"));
        let mut json = Vec::new();
        render(&report, OutputFormat::Json, &mut json).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(v["resources"][1]["ownership"], "other_project");
    }

    #[test]
    fn endpoints_and_stop_records_both_count_as_tracked() {
        let t = Tracked::from_state(Some(&state_with(
            vec![host_stop("io.amicu.llama.a1b2c3d4.a")],
            vec![endpoint("io.amicu.llama.a1b2c3d4.b")],
        )));
        assert!(t.has_label("io.amicu.llama.a1b2c3d4.a.service"));
        assert!(t.has_label("io.amicu.llama.a1b2c3d4.b"));
        assert!(!t.has_label("io.amicu.llama.a1b2c3d4.c"));
        assert!(!Tracked::from_state(None).has_label("x"));
    }
}
