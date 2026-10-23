//! `amicu orphans [--output json]` - list amicu-owned resources that this
//! project's state does not record.
//!
//! Strictly read-only: it enumerates Modal apps (`amicu-` prefix), RunPod
//! endpoints and pods carrying an `AMICU_OWNER` marker and `amicu-` RunPod secrets
//! (never the account marker), launchd jobs/plists (`io.amicu.` prefix),
//! systemd `--user` units and Task Scheduler tasks (the `\amicu\` folder,
//! Windows), marks each as
//! tracked or not by comparing against what `.amicu/state/state.json`
//! references (every stop record and every endpoint), and prints the exact
//! manual command that stops it. It never stops, imports or edits anything.
//!
//! Host labels are either project-scoped (`io.amicu.llama.<8hex>.<dep>`) or
//! legacy (`io.amicu.llama.<dep>`). Only an untracked label scoped to *this*
//! project is reported as a likely orphan; every other untracked resource
//! (another project's scoped label, a legacy label, a Modal app) is only
//! "not tracked by this project" and may belong to another project. Modal
//! apps are matched by app id and by name. RunPod endpoints and secrets are
//! named `amicu-<project id>-...`, so they are judged like scoped labels;
//! endpoints and pods are matched by id and by name, secrets by the names
//! recorded in stop records.
//!
//! Every external lookup goes through an injectable seam ([`CommandExec`] for
//! `launchctl`/`systemctl`, [`ModalApps`] for Modal, [`RunPodResources`] for
//! RunPod, a directory path for the plist scan) so tests never touch the real
//! host.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use amicu_core::graph::Graph;
use amicu_llamacpp::{CommandExec, HostExec};
use amicu_modal::{Credentials, ModalClient, ModalError};
use serde::Serialize;

use crate::OutputFormat;
use crate::commands::build_runtime;
use crate::error::CliError;
use crate::gcp_ops::GcpOps;
use crate::project::{LocalState, Project};
use crate::runpod_ops::RunPodOps;
use crate::write_json;

const MODAL_PREFIX: &str = "amicu-";
const LAUNCHD_PREFIX: &str = "io.amicu.";

const LLAMA_PREFIX: &str = "io.amicu.llama.";

const SCOPE_NOTE: &str = "Not tracked by this project does not mean orphaned: resources of other \
projects on this machine, Modal workspace or RunPod account appear here too. Only labels and \
RunPod names scoped to this project are reported as likely orphans. Check before stopping.";

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
    /// Not decidable from the name (Modal apps, plain units, unscoped RunPod
    /// names).
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
    // A telemetry sidecar belongs to whoever its instance belongs to.
    let main = amicu_llamacpp::sidecar_main_label(label);
    let label = main.as_deref().unwrap_or(label);
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

/// Classify a RunPod endpoint or secret name (`amicu-<project id>-...`).
fn ownership_of_runpod_name(name: &str, project_id: Option<&str>) -> Ownership {
    match name
        .strip_prefix(MODAL_PREFIX)
        .and_then(|rest| rest.split_once('-'))
    {
        Some((scope, rest))
            if !rest.is_empty()
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
        _ => Ownership::Unknown,
    }
}

/// Which resource family an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    ModalApp,
    RunpodEndpoint,
    RunpodPod,
    RunpodSecret,
    GceVm,
    GceFirewall,
    GceSecret,
    LaunchdJob,
    SystemdUnit,
    ScheduledTask,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::ModalApp => "modal app",
            Kind::RunpodEndpoint => "runpod endpoint",
            Kind::RunpodPod => "runpod pod",
            Kind::RunpodSecret => "runpod secret",
            Kind::GceVm => "compute engine vm",
            Kind::GceFirewall => "compute engine firewall rule",
            Kind::GceSecret => "secret manager secret",
            Kind::LaunchdJob => "launchd job",
            Kind::SystemdUnit => "systemd unit",
            Kind::ScheduledTask => "scheduled task",
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

/// What RunPod lists for the account the management key acts in.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunPodListing {
    pub endpoints: Vec<amicu_runpod::Endpoint>,
    pub pods: Vec<amicu_runpod::Pod>,
    pub secrets: Vec<amicu_runpod::Secret>,
}

/// Injectable read-only RunPod lookup.
pub(crate) trait RunPodResources {
    fn list(&self) -> Result<RunPodListing, String>;
}

/// Lists through a [`RunPodOps`] management client: only `GET` calls.
pub(crate) struct OpsListing<'a>(pub &'a dyn RunPodOps);

impl RunPodResources for OpsListing<'_> {
    fn list(&self) -> Result<RunPodListing, String> {
        let run = || -> Result<RunPodListing, CliError> {
            let client = self.0.client()?;
            let rt = build_runtime()?;
            rt.block_on(async {
                let endpoints = client.list_endpoints().await.map_err(|e| {
                    crate::runpod_ops::runpod_error("could not list RunPod endpoints", &e)
                })?;
                let pods = client.list_pods().await.map_err(|e| {
                    crate::runpod_ops::runpod_error("could not list RunPod pods", &e)
                })?;
                let secrets = client.list_secrets().await.map_err(|e| {
                    crate::runpod_ops::runpod_error("could not list RunPod secrets", &e)
                })?;
                Ok(RunPodListing {
                    endpoints,
                    pods,
                    secrets,
                })
            })
        };
        run().map_err(|e| e.to_string())
    }
}

/// An amicu-labelled VM in a Google Cloud project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GcpVm {
    pub project: String,
    pub zone: String,
    pub name: String,
    pub status: String,
}

/// An amicu-marked firewall rule in a Google Cloud project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GcpFirewall {
    pub project: String,
    pub name: String,
}

/// An amicu-owned Secret Manager secret in a Google Cloud project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GcpSecret {
    pub project: String,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GcpListing {
    pub vms: Vec<GcpVm>,
    pub firewalls: Vec<GcpFirewall>,
    pub secrets: Vec<GcpSecret>,
}

/// Injectable read-only Google Cloud lookup, per project (and the service
/// account to act as, if any).
pub(crate) trait GcpResources {
    fn list(&self, projects: &[(String, Option<String>)]) -> Result<GcpListing, String>;
}

/// Lists through a [`GcpOps`] client: only `GET` calls.
pub(crate) struct GcpOpsListing<'a>(pub &'a dyn GcpOps);

impl GcpResources for GcpOpsListing<'_> {
    fn list(&self, projects: &[(String, Option<String>)]) -> Result<GcpListing, String> {
        let run = || -> Result<GcpListing, CliError> {
            let rt = build_runtime()?;
            let mut out = GcpListing::default();
            for (project, impersonate) in projects {
                let client = self.0.client(impersonate.as_deref())?;
                let fail = |what: &str, e: amicu_gcp::GcpError| {
                    CliError::Message(format!("could not list {what} in {project}: {e}"))
                };
                let vms = rt
                    .block_on(amicu_gcp::gce::list_owned_instances(&client, project))
                    .map_err(|e| fail("VMs", e))?;
                let fws = rt
                    .block_on(amicu_gcp::gce::list_owned_firewalls(&client, project))
                    .map_err(|e| fail("firewall rules", e))?;
                // A project that delivers secrets as instance metadata may
                // never have enabled Secret Manager: not being allowed to list
                // there means no secrets, not a failed listing.
                let secrets =
                    match rt.block_on(amicu_gcp::gce::list_owned_secrets(&client, project)) {
                        Ok(s) => s,
                        Err(amicu_gcp::GcpError::PermissionDenied { .. }) => Vec::new(),
                        Err(e) => return Err(fail("secrets", e)),
                    };
                out.secrets
                    .extend(secrets.into_iter().map(|(name, _)| GcpSecret {
                        project: project.clone(),
                        name,
                    }));
                out.vms.extend(vms.into_iter().map(|v| GcpVm {
                    project: project.clone(),
                    zone: v.zone,
                    name: v.name,
                    status: v.status,
                }));
                out.firewalls
                    .extend(fws.into_iter().map(|(name, _)| GcpFirewall {
                        project: project.clone(),
                        name,
                    }));
            }
            Ok(out)
        };
        run().map_err(|e| e.to_string())
    }
}

/// The host platform, injectable for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    MacOs,
    Linux,
    Windows,
    Other,
}

impl Platform {
    fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Platform::MacOs,
            "linux" => Platform::Linux,
            "windows" => Platform::Windows,
            _ => Platform::Other,
        }
    }
}

/// Everything discovery reads from, all injectable.
pub(crate) struct Env<'a> {
    pub platform: Platform,
    pub exec: &'a dyn CommandExec,
    pub modal: &'a dyn ModalApps,
    pub runpod: &'a dyn RunPodResources,
    pub gcp: &'a dyn GcpResources,
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
    /// Whether state or config references Modal at all (a Modal stop record or
    /// a `modal` hardware profile). Modal is only queried when it does, so a
    /// local-only project is never told about missing Modal credentials.
    modal_referenced: bool,
    /// Names of RunPod records with no endpoint id yet; a record with an id
    /// tracks that id only, so a same-named duplicate shows as untracked.
    runpod_names: BTreeSet<String>,
    runpod_ids: BTreeSet<String>,
    runpod_secrets: BTreeSet<String>,
    /// Like `modal_referenced`, for RunPod.
    runpod_referenced: bool,
    /// Google projects (and the account to act as) that state or config name.
    gcp_projects: BTreeSet<(String, Option<String>)>,
    /// `(project, name)` of recorded VMs and firewall rules.
    gcp_vms: BTreeSet<(String, String)>,
    gcp_firewalls: BTreeSet<(String, String)>,
    /// `(project, id)` of recorded Secret Manager secrets.
    gcp_secrets: BTreeSet<(String, String)>,
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
                t.modal_referenced = true;
                t.modal_names.insert(rec.identity.clone());
                t.modal_ids.extend(rec.app_id.clone());
            } else if rec.backend.is_runpod() {
                t.runpod_referenced = true;
                match &rec.app_id {
                    Some(id) => t.runpod_ids.insert(id.clone()),
                    None => t.runpod_names.insert(rec.identity.clone()),
                };
                t.runpod_secrets.extend(rec.secrets.iter().cloned());
            } else if let Some(g) = &rec.gcp {
                t.gcp_projects
                    .insert((g.project.clone(), g.impersonate.clone()));
                t.gcp_vms.insert((g.project.clone(), g.instance.clone()));
                t.gcp_firewalls
                    .insert((g.project.clone(), g.firewall.clone()));
                t.gcp_secrets
                    .extend(g.secrets.iter().map(|id| (g.project.clone(), id.clone())));
            } else if rec.backend.is_host() {
                t.insert_label(&rec.identity);
            }
        }
        t.modal_referenced |= uses_modal(&state.graph);
        t.runpod_referenced |= uses_provider(&state.graph, "runpod");
        t.add_gcp_profiles(&state.graph);
        for ep in &state.endpoints {
            // Modal app ids (`ap-...`) mean Modal is in play even with no stop
            // record or Modal profile in the graph.
            if ep.id.starts_with("ap-") {
                t.modal_referenced = true;
            }
            t.modal_names.insert(ep.id.clone());
            t.modal_ids.insert(ep.id.clone());
            t.runpod_ids.insert(ep.id.clone());
            t.insert_label(&ep.id);
        }
        t
    }

    /// Also count a Modal hardware profile in the current config as a reference.
    pub(crate) fn with_config(mut self, config: Option<&Graph>) -> Self {
        self.modal_referenced |= config.is_some_and(uses_modal);
        self.runpod_referenced |= config.is_some_and(|g| uses_provider(g, "runpod"));
        if let Some(g) = config {
            self.add_gcp_profiles(g);
        }
        self
    }

    /// The Google projects the graph's `gcp` profiles name.
    fn add_gcp_profiles(&mut self, graph: &Graph) {
        for s in graph
            .hardware_profiles
            .iter()
            .filter_map(|h| h.gcp.as_ref())
        {
            if let Some(p) = &s.project {
                self.gcp_projects
                    .insert((p.clone(), s.impersonate_service_account.clone()));
            }
        }
    }

    fn insert_label(&mut self, label: &str) {
        self.labels
            .insert(label.strip_suffix(".service").unwrap_or(label).to_owned());
    }

    /// A telemetry sidecar counts as tracked when its instance is.
    fn has_label(&self, label: &str) -> bool {
        let label = label.strip_suffix(".service").unwrap_or(label);
        let main = amicu_llamacpp::sidecar_main_label(label);
        self.labels.contains(main.as_deref().unwrap_or(label))
    }
}

/// Whether any hardware profile in `graph` runs on Modal.
fn uses_modal(graph: &Graph) -> bool {
    uses_provider(graph, "modal")
}

fn uses_provider(graph: &Graph, provider: &str) -> bool {
    graph
        .hardware_profiles
        .iter()
        .any(|h| h.provider.eq_ignore_ascii_case(provider))
}

/// The commands that delete a RunPod resource at `path` (relative to the v2
/// API) with the management key, for the shell the user most likely uses.
fn runpod_delete_command(platform: Platform, path: &str) -> String {
    let url = format!("{}/{path}", amicu_runpod::BASE_URL);
    let var = crate::runpod_ops::API_KEY_ENV_VAR;
    match platform {
        Platform::Windows => format!(
            "curl.exe -X DELETE -H \"Authorization: Bearer $env:{var}\" {}",
            quote_for(platform, &url)
        ),
        _ => format!(
            "curl -X DELETE -H \"Authorization: Bearer ${var}\" {}",
            shell_quote(&url)
        ),
    }
}

/// RunPod endpoints and live pods amicu created (they carry an
/// `AMICU_OWNER` marker) and `amicu-` secrets other than the account marker.
fn runpod(env: &Env<'_>, tracked: &Tracked) -> Result<Vec<Found>, String> {
    let listing = env.runpod.list()?;
    let project_id = tracked.project_id.as_deref();
    let mut found = Vec::new();
    for ep in listing.endpoints.into_iter().filter(|e| e.owner.is_some()) {
        let state = ep.workers.as_ref().map_or_else(
            || "serverless".to_owned(),
            |w| format!("serverless, workers {}-{}", w.min, w.max),
        );
        found.push(Found {
            kind: Kind::RunpodEndpoint,
            tracked: tracked.runpod_ids.contains(&ep.id) || tracked.runpod_names.contains(&ep.name),
            ownership: ownership_of_runpod_name(&ep.name, project_id),
            stop_command: runpod_delete_command(env.platform, &format!("serverless/{}", ep.id)),
            identity: format!("{} ({})", ep.name, ep.id),
            state,
        });
    }
    for pod in listing
        .pods
        .into_iter()
        .filter(|p| p.env.owner.is_some() && p.status != amicu_runpod::PodStatus::Terminated)
    {
        let state = match pod.cost {
            Some(c) if c > 0.0 => format!("pod {}, ${c:.2}/hr", pod.status.as_str()),
            _ => format!("pod {}", pod.status.as_str()),
        };
        found.push(Found {
            kind: Kind::RunpodPod,
            tracked: tracked.runpod_ids.contains(&pod.id)
                || tracked.runpod_names.contains(&pod.name),
            ownership: ownership_of_runpod_name(&pod.name, project_id),
            stop_command: runpod_delete_command(env.platform, &format!("pods/{}", pod.id)),
            identity: format!("{} ({})", pod.name, pod.id),
            state,
        });
    }
    for secret in listing.secrets.into_iter().filter(|s| {
        s.name.starts_with(MODAL_PREFIX)
            && !s
                .name
                .eq_ignore_ascii_case(amicu_runpod::ACCOUNT_MARKER_SECRET)
    }) {
        found.push(Found {
            kind: Kind::RunpodSecret,
            tracked: tracked.runpod_secrets.contains(&secret.name),
            ownership: ownership_of_runpod_name(&secret.name, project_id),
            stop_command: runpod_delete_command(
                env.platform,
                &format!("account/secrets/{}", secret.id),
            ),
            identity: format!("{} ({})", secret.name, secret.id),
            state: "secret".to_owned(),
        });
    }
    Ok(found)
}

/// Compute Engine VMs, firewall rules and Secret Manager secrets amicu created (they carry its owner
/// marker) in the projects state or config names, with the `gcloud` commands
/// that delete them by hand.
fn gcp(env: &Env<'_>, tracked: &Tracked) -> Result<Vec<Found>, String> {
    let projects: Vec<_> = tracked.gcp_projects.iter().cloned().collect();
    let listing = env.gcp.list(&projects)?;
    let project_id = tracked.project_id.as_deref();
    let q = |s: &str| quote_for(env.platform, s);
    let mut found = Vec::new();
    for v in &listing.vms {
        found.push(Found {
            kind: Kind::GceVm,
            identity: format!("{} ({}, {})", v.name, v.project, v.zone),
            state: v.status.to_lowercase(),
            tracked: tracked
                .gcp_vms
                .contains(&(v.project.clone(), v.name.clone())),
            ownership: ownership_of_runpod_name(&v.name, project_id),
            stop_command: format!(
                "gcloud compute instances delete {} --zone {} --project {}",
                q(&v.name),
                q(&v.zone),
                q(&v.project)
            ),
        });
    }
    for r in &listing.firewalls {
        found.push(Found {
            kind: Kind::GceFirewall,
            identity: format!("{} ({})", r.name, r.project),
            state: "firewall rule".to_owned(),
            tracked: tracked
                .gcp_firewalls
                .contains(&(r.project.clone(), r.name.clone())),
            ownership: ownership_of_runpod_name(&r.name, project_id),
            stop_command: format!(
                "gcloud compute firewall-rules delete {} --project {}",
                q(&r.name),
                q(&r.project)
            ),
        });
    }
    for r in &listing.secrets {
        found.push(Found {
            kind: Kind::GceSecret,
            identity: format!("{} ({})", r.name, r.project),
            state: "secret".to_owned(),
            tracked: tracked
                .gcp_secrets
                .contains(&(r.project.clone(), r.name.clone())),
            ownership: ownership_of_runpod_name(&r.name, project_id),
            stop_command: format!(
                "gcloud secrets delete {} --project {}",
                q(&r.name),
                q(&r.project)
            ),
        });
    }
    Ok(found)
}

/// The RunPod source entry, adding what it found to `resources`.
fn runpod_source(env: &Env<'_>, tracked: &Tracked, resources: &mut Vec<Found>) -> Source {
    if !tracked.runpod_referenced {
        return source(
            "runpod",
            "skipped",
            Some("not used by this project".to_owned()),
        );
    }
    match runpod(env, tracked) {
        Ok(found) => {
            resources.extend(found);
            source("runpod", "ok", None)
        }
        Err(e) => source("runpod", "error", Some(e)),
    }
}

/// The Google Cloud source entry, adding what it found to `resources`.
fn gcp_source(env: &Env<'_>, tracked: &Tracked, resources: &mut Vec<Found>) -> Source {
    if tracked.gcp_projects.is_empty() {
        return source(
            "gcp",
            "skipped",
            Some("not used by this project".to_owned()),
        );
    }
    match gcp(env, tracked) {
        Ok(found) => {
            resources.extend(found);
            source("gcp", "ok", None)
        }
        Err(e) => source("gcp", "error", Some(e)),
    }
}

/// Discover untracked (and tracked) amicu resources. Never mutates anything.
pub(crate) fn discover(env: &Env<'_>, tracked: &Tracked) -> OrphansReport {
    let mut sources = Vec::new();
    let mut resources = Vec::new();

    if tracked.modal_referenced {
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
                        stop_command: format!(
                            "amicu modal stop -- {}",
                            quote_for(env.platform, &app.name)
                        ),
                        identity: app.name,
                        state: app.state,
                    });
                }
            }
            Err(e) => sources.push(source("modal", "error", Some(e))),
        }
    } else {
        sources.push(source(
            "modal",
            "skipped",
            Some("not used by this project".to_owned()),
        ));
    }

    sources.push(runpod_source(env, tracked, &mut resources));
    sources.push(gcp_source(env, tracked, &mut resources));

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
        Platform::Windows => {
            sources.push(source("launchd", "skipped", Some("not macOS".to_owned())));
            sources.push(source("systemd", "skipped", Some("not Linux".to_owned())));
            match task_scheduler(env, tracked) {
                Ok(found) => {
                    sources.push(source("task-scheduler", "ok", None));
                    resources.extend(found);
                }
                Err(e) => sources.push(source("task-scheduler", "error", Some(e))),
            }
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

/// A value interpolated into a printed command, quoted for the shell the user
/// most likely pastes it into: PowerShell on Windows, a POSIX shell elsewhere.
fn quote_for(platform: Platform, s: &str) -> String {
    match platform {
        Platform::Windows => amicu_llamacpp::service_windows::ps_quote(s),
        _ => shell_quote(s),
    }
}

/// The PowerShell commands that stop a Task Scheduler task and unregister it:
/// disable it first so its restart-on-failure cannot bring it back, end it,
/// then delete it.
fn task_stop_command(label: &str) -> String {
    let q = amicu_llamacpp::service_windows::ps_quote(&amicu_llamacpp::service_windows::task_name(
        label,
    ));
    format!("schtasks /Change /TN {q} /DISABLE; schtasks /End /TN {q}; schtasks /Delete /TN {q} /F")
}

fn task_scheduler(env: &Env<'_>, tracked: &Tracked) -> Result<Vec<Found>, String> {
    let tasks = amicu_llamacpp::service_windows::list_tasks(env.exec).map_err(|e| e.to_string())?;
    Ok(tasks
        .into_iter()
        .map(|t| Found {
            kind: Kind::ScheduledTask,
            tracked: tracked.has_label(&t.name),
            ownership: ownership_of_label(&t.name, tracked.project_id.as_deref()),
            stop_command: task_stop_command(&t.name),
            state: t.state.to_lowercase(),
            identity: t.name,
        })
        .collect())
}

/// The plist path as a shell word. Without a known agents dir it falls back to
/// `~/...`, quoting only the part after `~/` so the shell still expands the
/// tilde.
fn plist_arg(dir: Option<&Path>, label: &str) -> String {
    dir.map_or_else(
        || {
            let rest = format!("Library/LaunchAgents/{label}.plist");
            format!("~/{}", shell_quote(&rest))
        },
        |d| shell_quote(&d.join(format!("{label}.plist")).display().to_string()),
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
            let q = plist_arg(env.agents_dir.as_deref(), &label);
            Found {
                kind: Kind::LaunchdJob,
                tracked: tracked.has_label(&label),
                ownership: ownership_of_label(&label, tracked.project_id.as_deref()),
                stop_command: format!("launchctl unload -w {q} && rm {q}"),
                identity: label,
                state,
            }
        })
        .collect())
}

/// The command that stops a systemd unit and keeps it from coming back:
/// `disable --now` for a persistent unit, else a plain stop (a legacy
/// transient unit cannot be disabled).
fn systemd_stop_command(unit: &str) -> String {
    let q = shell_quote(unit);
    format!("systemctl --user disable --now -- {q} || systemctl --user stop -- {q}")
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
            stop_command: systemd_stop_command(unit),
            identity: (*unit).to_owned(),
            state,
        });
    }
    // Persistent units that are installed but not loaded (never started this
    // boot, or stopped but still enabled) are invisible to `list-units`, yet
    // would start again at the next login/boot: list the unit files too. Best
    // effort - the loaded-unit listing above is the authoritative part.
    if let Ok(files) = env.exec.exec(
        "systemctl",
        &[
            "--user",
            "list-unit-files",
            "io.amicu.*",
            "amicu*",
            "--no-legend",
            "--plain",
            "--no-pager",
        ],
    ) && files.success
    {
        for line in files.stdout.lines() {
            // UNIT STATE PRESET
            let cols: Vec<&str> = line.split_whitespace().collect();
            let Some(unit) = cols.first() else { continue };
            if !(unit.starts_with("amicu") || unit.starts_with(LAUNCHD_PREFIX))
                || found.iter().any(|f| f.identity == *unit)
            {
                continue;
            }
            let label = unit.strip_suffix(".service").unwrap_or(unit);
            found.push(Found {
                kind: Kind::SystemdUnit,
                tracked: tracked.has_label(label),
                ownership: ownership_of_label(label, tracked.project_id.as_deref()),
                stop_command: systemd_stop_command(unit),
                identity: (*unit).to_owned(),
                state: format!("not loaded ({})", cols.get(1).unwrap_or(&"unknown")),
            });
        }
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
    let tracked =
        Tracked::from_state(state.as_ref()).with_config(project.load_graph().ok().as_ref());
    let agents_dir = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join("Library/LaunchAgents"));
    let env = Env {
        platform: Platform::current(),
        exec: &HostExec,
        modal: &RealModal,
        runpod: &OpsListing(&crate::runpod_ops::RealRunPod),
        gcp: &GcpOpsListing(&crate::gcp_ops::RealGcp),
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
        /// Answer for `list-unit-files`; `None` falls back to `systemctl`.
        unit_files: Option<String>,
        /// Answer for the PowerShell Task Scheduler listing.
        powershell: Result<String, String>,
    }

    impl FakeExec {
        fn new(launchctl: &str, systemctl: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                launchctl: Ok(launchctl.to_owned()),
                systemctl: Ok(systemctl.to_owned()),
                unit_files: None,
                powershell: Ok(String::new()),
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
            let unit_files = self.unit_files.clone().map(Ok);
            let r = match program {
                "launchctl" => &self.launchctl,
                "systemctl" if args.contains(&"list-unit-files") && unit_files.is_some() => {
                    unit_files.as_ref().unwrap()
                }
                "systemctl" => &self.systemctl,
                "powershell" => &self.powershell,
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
            serial: 0,
            purge_pending: Vec::new(),
            extra: std::collections::BTreeMap::default(),
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

    struct NoGcpResources;

    impl GcpResources for NoGcpResources {
        fn list(&self, _: &[(String, Option<String>)]) -> Result<GcpListing, String> {
            panic!("Google Cloud must not be queried")
        }
    }

    struct NoRunPod;

    impl RunPodResources for NoRunPod {
        fn list(&self) -> Result<RunPodListing, String> {
            panic!("RunPod must not be queried")
        }
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
            runpod: &NoRunPod,
            gcp: &NoGcpResources,
            agents_dir,
        }
    }

    /// A project whose state references Modal, so discovery queries it.
    fn uses_modal_tracked() -> Tracked {
        Tracked {
            modal_referenced: true,
            ..Tracked::default()
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
            "amicu modal stop -- amicu-old"
        );
        assert!(exec.calls().is_empty(), "no commands on Other");
    }

    #[test]
    fn a_telemetry_sidecar_follows_its_instance() {
        let tracked = tracked_of(vec![host_stop("io.amicu.llama.a1b2c3d4.chat")]);
        assert!(tracked.has_label("io.amicu.telemetry.a1b2c3d4.chat"));
        assert!(tracked.has_label("io.amicu.telemetry.a1b2c3d4.chat.service"));
        assert!(!tracked.has_label("io.amicu.telemetry.a1b2c3d4.other"));
        assert_eq!(
            ownership_of_label("io.amicu.telemetry.a1b2c3d4.other", Some("a1b2c3d4")),
            Ownership::ThisProject
        );
    }

    #[test]
    fn modal_is_skipped_silently_when_nothing_references_it() {
        // A local-only project: no Modal stop record, no `modal` hardware.
        // The lookup (which would fail on missing credentials) must not run.
        let modal = FakeModal(Err("MODAL_TOKEN_ID is not set".to_owned()));
        let exec = FakeExec::new("", "");
        let report = discover(
            &env(Platform::Other, &exec, &modal, None),
            &tracked_of(vec![host_stop("io.amicu.llama.a1b2c3d4.chat")]),
        );
        assert_eq!(report.sources[0].name, "modal");
        assert_eq!(report.sources[0].status, "skipped");
        let mut buf = Vec::new();
        render(&report, OutputFormat::Human, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(!text.contains("warning"), "{text}");
        assert!(!text.contains("MODAL_TOKEN_ID"), "{text}");
    }

    #[test]
    fn modal_is_queried_when_state_or_config_references_it() {
        let modal = FakeModal(Err("MODAL_TOKEN_ID is not set".to_owned()));
        let exec = FakeExec::new("", "");
        // State: a Modal stop record.
        let from_state = Tracked::from_state(Some(&state_with(
            vec![modal_stop("amicu-x", None)],
            Vec::new(),
        )));
        let report = discover(&env(Platform::Other, &exec, &modal, None), &from_state);
        assert_eq!(report.sources[0].status, "error");
        // Config: a hardware profile on Modal.
        let mut graph = Graph::new();
        graph
            .hardware_profiles
            .push(amicu_core::resource::HardwareProfile {
                id: "hw".into(),
                provider: "modal".to_owned(),
                region: None,
                accelerator: None,
                cpu: None,
                memory: amicu_core::ByteSize::from_bytes(1 << 30),
                interconnect: None,
                price: None,
                runpod: None,
                gcp: None,
            });
        let from_config = Tracked::default().with_config(Some(&graph));
        let report = discover(&env(Platform::Other, &exec, &modal, None), &from_config);
        assert_eq!(report.sources[0].status, "error");
    }
    #[test]
    fn modal_error_is_reported_not_fatal() {
        let modal = FakeModal(Err("no credentials".to_owned()));
        let exec = FakeExec::new("", "");
        let report = discover(
            &env(Platform::Other, &exec, &modal, None),
            &uses_modal_tracked(),
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
            &tracked_of(vec![host_stop("io.amicu.llama.a1b2c3d4.chat")]),
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

    #[cfg(unix)]
    #[test]
    fn plist_fallback_keeps_tilde_unquoted() {
        assert_eq!(
            plist_arg(None, "io.amicu.llama.x"),
            "~/Library/LaunchAgents/io.amicu.llama.x.plist"
        );
        assert_eq!(
            plist_arg(None, "io.amicu.a b"),
            "~/'Library/LaunchAgents/io.amicu.a b.plist'"
        );
        assert_eq!(
            plist_arg(Some(Path::new("/Users/a b/Agents")), "io.amicu.x"),
            "'/Users/a b/Agents/io.amicu.x.plist'"
        );
    }

    #[test]
    fn operands_come_after_double_dash_and_stay_quoted() {
        let exec = FakeExec::new("", "amicu-x;y.service loaded active running amicu\n");
        let modal = FakeModal(Ok(vec![app("amicu-it's", "deployed")]));
        let report = discover(
            &env(Platform::Linux, &exec, &modal, None),
            &uses_modal_tracked(),
        );
        let cmds: Vec<_> = report
            .resources
            .iter()
            .map(|r| r.stop_command.as_str())
            .collect();
        assert!(
            cmds.contains(&"amicu modal stop -- 'amicu-it'\\''s'"),
            "{cmds:?}"
        );
        assert!(
            cmds.contains(&"systemctl --user disable --now -- 'amicu-x;y.service' || systemctl --user stop -- 'amicu-x;y.service'"),
            "{cmds:?}"
        );
    }

    #[test]
    fn launchctl_failure_is_a_source_error() {
        let exec = FakeExec {
            calls: Mutex::new(Vec::new()),
            launchctl: Err("boom".to_owned()),
            systemctl: Ok(String::new()),
            unit_files: None,
            powershell: Ok(String::new()),
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
            "systemctl --user disable --now -- io.amicu.llama.lost.service || \
             systemctl --user stop -- io.amicu.llama.lost.service"
        );
        assert!(exec.calls()[0].starts_with("systemctl --user list-units"));
    }

    #[test]
    fn systemd_lists_persistent_units_that_are_installed_but_not_loaded() {
        // `list-units` only shows loaded units; a persistent unit that is
        // enabled but not running is found through `list-unit-files`.
        let exec = FakeExec {
            unit_files: Some(
                "io.amicu.llama.a1b2c3d4.chat.service enabled enabled\n\
io.amicu.llama.a1b2c3d4.idle.service enabled enabled\n\
unrelated.service enabled enabled\n"
                    .to_owned(),
            ),
            ..FakeExec::new(
                "",
                "io.amicu.llama.a1b2c3d4.chat.service loaded active running c\n",
            )
        };
        let modal = FakeModal(Ok(vec![]));
        let report = discover(&env(Platform::Linux, &exec, &modal, None), &tracked_chat());
        let got: Vec<_> = report
            .resources
            .iter()
            .map(|r| (r.identity.as_str(), r.state.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "io.amicu.llama.a1b2c3d4.idle.service",
                    "not loaded (enabled)"
                ),
                ("io.amicu.llama.a1b2c3d4.chat.service", "active (running)"),
            ]
        );
        assert!(exec.calls().iter().any(|c| c.contains("list-unit-files")));
    }

    #[test]
    fn output_is_read_only_and_json_has_note() {
        let exec = FakeExec::new("", "");
        let modal = FakeModal(Ok(vec![app("amicu-x", "deployed")]));
        let report = discover(
            &env(Platform::Linux, &exec, &modal, None),
            &uses_modal_tracked(),
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
        assert!(human.contains("stop: amicu modal stop -- amicu-x"));
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

    #[test]
    fn modal_endpoint_ids_mark_modal_referenced() {
        let t = Tracked::from_state(Some(&state_with(Vec::new(), vec![endpoint("ap-123")])));
        assert!(t.modal_referenced);
        let t = Tracked::from_state(Some(&state_with(Vec::new(), vec![endpoint("io.amicu.x")])));
        assert!(!t.modal_referenced);
    }

    #[test]
    fn windows_lists_amicu_tasks_with_powershell_stop_commands() {
        let mut exec = FakeExec::new("", "");
        exec.powershell = Ok(format!(
            "TASK\tio.amicu.llama.{PID}.chat\tRunning\t267009\r\n\
             TASK\tio.amicu.llama.{PID}.old\tReady\t0\r\n\
             TASK\tio.amicu.llama.it's\tDisabled\t0\r\nEND\r\n"
        ));
        let modal = FakeModal(Ok(vec![app("amicu-x y", "deployed")]));
        let report = discover(
            &env(Platform::Windows, &exec, &modal, None),
            &Tracked {
                modal_referenced: true,
                ..tracked_chat()
            },
        );
        let status: Vec<_> = report.sources.iter().map(|s| (s.name, s.status)).collect();
        assert_eq!(
            status,
            [
                ("modal", "ok"),
                ("runpod", "skipped"),
                ("gcp", "skipped"),
                ("launchd", "skipped"),
                ("systemd", "skipped"),
                ("task-scheduler", "ok"),
            ]
        );
        let tasks: Vec<_> = report
            .resources
            .iter()
            .filter(|r| r.kind == Kind::ScheduledTask)
            .map(|r| {
                (
                    r.identity.as_str(),
                    r.state.as_str(),
                    r.tracked,
                    r.ownership,
                )
            })
            .collect();
        let chat = format!("io.amicu.llama.{PID}.chat");
        let old = format!("io.amicu.llama.{PID}.old");
        assert_eq!(
            tasks,
            [
                (old.as_str(), "ready", false, Ownership::ThisProject),
                ("io.amicu.llama.it's", "disabled", false, Ownership::Legacy),
                (chat.as_str(), "running", true, Ownership::ThisProject),
            ]
        );
        let odd = report
            .resources
            .iter()
            .find(|r| r.identity == "io.amicu.llama.it's")
            .unwrap();
        assert_eq!(
            odd.stop_command,
            "schtasks /Change /TN 'amicu\\io.amicu.llama.it''s' /DISABLE; \
             schtasks /End /TN 'amicu\\io.amicu.llama.it''s'; \
             schtasks /Delete /TN 'amicu\\io.amicu.llama.it''s' /F"
        );
        let modal_app = report
            .resources
            .iter()
            .find(|r| r.kind == Kind::ModalApp)
            .unwrap();
        assert_eq!(modal_app.stop_command, "amicu modal stop -- 'amicu-x y'");
    }

    #[test]
    fn windows_reports_a_failed_task_listing_as_an_error() {
        let mut exec = FakeExec::new("", "");
        exec.powershell = Err("Get-ScheduledTask: access denied".to_owned());
        let modal = FakeModal(Ok(vec![]));
        let report = discover(
            &env(Platform::Windows, &exec, &modal, None),
            &tracked_chat(),
        );
        let ts = report
            .sources
            .iter()
            .find(|s| s.name == "task-scheduler")
            .unwrap();
        assert_eq!(ts.status, "error");
        assert_eq!(report.resources.len(), 0);
    }
}
