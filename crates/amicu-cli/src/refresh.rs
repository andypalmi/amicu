//! Reconciling local state with what is actually live on Modal.
//!
//! `amicu modal stop` (or the Modal dashboard) can end an app behind the
//! project's back, leaving `.amicu/state/state.json` recording a deployment
//! that no longer exists; diffing the config against that stale state yields
//! "No changes" forever. Before `plan`/`apply` diff,
//! [`refresh_state`] asks Modal which recorded apps are still live and drops
//! the ones that are not, so the planner shows a create.

use std::collections::HashSet;
use std::io::Write;

use amicu_modal::supervisor::app_name_for;
use amicu_modal::{Credentials, ModalClient, ModalError};

use crate::OutputFormat;
use crate::commands::build_runtime;
use crate::error::CliError;
use crate::lock::StateLock;
use crate::modal_ops::{Workspace, check_workspace};
use crate::project::{LocalState, Project};
use crate::stops::StopRecord;

/// Hardware profile `provider` value of a Modal-backed deployment.
const MODAL_PROVIDER: &str = "modal";

/// Looks up which Modal apps are live. A trait so unit tests mock it: no
/// network in tests.
pub(crate) trait AppLookup {
    /// Of `app_names`, the ones that exist on Modal. `Err` carries a message
    /// safe to print (no credentials).
    fn live_apps(&self, app_names: &[String]) -> Result<HashSet<String>, String>;

    /// The workspace the active token belongs to, when it can be told. The
    /// default (`None`) skips the workspace check.
    fn active_workspace(&self) -> Result<Option<Workspace>, String> {
        Ok(None)
    }

    /// Every app id the active workspace can see, when it can be listed. The
    /// default (`None`) skips proving ownership of workspace-less records.
    fn app_ids(&self) -> Result<Option<HashSet<String>>, String> {
        Ok(None)
    }

    /// Whether the host service (launchd job / systemd unit) a host stop
    /// record names is running. The default ([`HostUnit::Unknown`]) skips the
    /// host check. `Err` carries a message safe to print.
    fn host_unit(&self, _record: &StopRecord) -> Result<HostUnit, String> {
        Ok(HostUnit::Unknown)
    }
}

/// What the host service manager says about one recorded host instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostUnit {
    /// Loaded and running.
    Running,
    /// Not loaded, stopped, or crashed: the instance is not serving.
    Down,
    /// Not asked.
    Unknown,
}

/// The real lookup: the same `find_app` call `amicu modal status` is built on.
pub(crate) struct ModalLookup;

impl AppLookup for ModalLookup {
    fn app_ids(&self) -> Result<Option<HashSet<String>>, String> {
        let run = || -> Result<Vec<String>, CliError> {
            let rt = build_runtime()?;
            rt.block_on(crate::modal_ops::ModalOps::list_app_ids(self))
        };
        run()
            .map(|v| Some(v.into_iter().collect()))
            .map_err(|e| e.to_string())
    }

    fn host_unit(&self, record: &StopRecord) -> Result<HostUnit, String> {
        use amicu_llamacpp::Presence;
        let supervisor = amicu_llamacpp::host_supervisor();
        // Only ask about an instance this user and service manager own: a
        // different uid or backend would be answering about someone else's job.
        let ctx = supervisor
            .host_context()
            .ok_or_else(|| "cannot tell which user and service manager amicu acts as".to_owned())?;
        if crate::stops::Backend::from_host(ctx.backend) != record.backend
            || record.uid.is_some_and(|u| u != ctx.uid)
        {
            return Err(format!(
                "{} was started under another user or service manager",
                record.describe()
            ));
        }
        match supervisor.presence(&record.identity, record.domain.as_deref()) {
            Ok(Presence::Running) => Ok(HostUnit::Running),
            Ok(Presence::Down) => Ok(HostUnit::Down),
            Ok(Presence::Unknown) => Ok(HostUnit::Unknown),
            Err(e) => Err(e.to_string()),
        }
    }

    fn active_workspace(&self) -> Result<Option<Workspace>, String> {
        let run = || -> Result<Workspace, CliError> {
            let rt = build_runtime()?;
            rt.block_on(crate::modal_ops::ModalOps::workspace(self))
        };
        run().map(Some).map_err(|e| e.to_string())
    }

    fn live_apps(&self, app_names: &[String]) -> Result<HashSet<String>, String> {
        // `ModalError`'s `Display` never contains a token.
        let run = || -> Result<HashSet<String>, CliError> {
            let credentials = Credentials::from_env().map_err(ModalError::from)?;
            let rt = build_runtime()?;
            let live = rt.block_on(async {
                let mut client = ModalClient::connect(credentials).await?;
                let mut live = HashSet::new();
                for name in app_names {
                    if client.find_app(name).await?.is_some() {
                        live.insert(name.clone());
                    }
                }
                Ok::<_, ModalError>(live)
            })?;
            Ok(live)
        };
        run().map_err(|e| e.to_string())
    }
}

/// The Modal app id recorded for deployment `id`: from its stop record or its
/// endpoint handle (a Modal handle's `id` is the app id).
fn recorded_app_id(state: &LocalState, id: &str) -> Option<String> {
    state
        .stops
        .iter()
        .find(|r| r.deployment.as_str() == id && r.app_id.is_some())
        .and_then(|r| r.app_id.clone())
        .or_else(|| {
            state
                .endpoints
                .iter()
                .find(|h| h.deployment.as_str() == id)
                .map(|h| h.id.clone())
        })
}

/// Why [`prove_workspaces`] failed.
enum ProofError {
    /// A record is in another workspace, or its ownership is unproven.
    Mismatch(String),
    /// The workspace's apps could not be listed.
    Unlisted(String),
}

/// Check every Modal stop record against the `active` workspace. Records
/// with a workspace must match it. Records without one (v1-derived) that carry
/// an app id must be proven to belong to it - the id has to appear in the
/// workspace's app list, fetched at most once - and are then stamped with it.
/// Materialises the derived records into `state.stops`.
fn prove_workspaces(
    state: &mut LocalState,
    lookup: &dyn AppLookup,
    active: &Workspace,
) -> Result<(), ProofError> {
    let mut records = crate::commands::apply::effective_stops(Some(state));
    let mut listed: Option<Result<Option<HashSet<String>>, String>> = None;
    for r in records.iter_mut().filter(|r| r.backend.is_modal()) {
        if r.workspace.is_some() {
            check_workspace(
                r.deployment.as_str(),
                r.workspace.as_deref(),
                r.environment.as_deref(),
                active,
            )
            .map_err(|e| ProofError::Mismatch(e.to_string()))?;
        } else if let Some(app_id) = r.app_id.clone() {
            let ids = listed.get_or_insert_with(|| lookup.app_ids());
            match ids {
                Ok(Some(ids)) if ids.contains(&app_id) => {
                    r.workspace = Some(active.name.clone());
                    r.environment = Some(active.environment.clone());
                }
                Ok(Some(_)) => {
                    return Err(ProofError::Mismatch(
                        crate::modal_ops::unproven_workspace(
                            r.deployment.as_str(),
                            &app_id,
                            active,
                        )
                        .to_string(),
                    ));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(ProofError::Unlisted(format!(
                        "cannot list the apps of {active} to prove ownership of {app_id} \
                         ({e}); nothing was changed"
                    )));
                }
            }
        }
    }
    state.stops = records;
    Ok(())
}

/// What a refresh found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Refresh {
    /// One drift note per deployment dropped from the state.
    pub notes: Vec<String>,
    /// Set when the lookup failed and the state was kept as-is.
    pub warning: Option<String>,
    /// Set when the active Modal workspace differs from the one a recorded
    /// instance lives in: the command must refuse to continue.
    pub mismatch: Option<String>,
}

/// Drops from `state` every Modal deployment (and its endpoint) whose app is
/// no longer live. Never fails: if the lookup errors, `state` is left as-is
/// and [`Refresh::warning`] says so.
#[cfg(test)]
pub(crate) fn refresh_state(state: &mut LocalState, lookup: &dyn AppLookup) -> Refresh {
    refresh_state_mode(state, lookup, true)
}

/// Like [`refresh_state`]. With `strict` false (read-only `plan`), an unknown
/// active workspace only warns and the recorded state is used as-is, with
/// nothing marked pending; strict (apply, destroy) refuses.
pub(crate) fn refresh_state_mode(
    state: &mut LocalState,
    lookup: &dyn AppLookup,
    strict: bool,
) -> Refresh {
    let host = refresh_host(state, lookup);
    let mut modal = refresh_modal(state, lookup, strict);
    if modal.mismatch.is_some() {
        return modal;
    }
    let mut notes = host.notes;
    notes.append(&mut modal.notes);
    modal.notes = notes;
    modal.warning = match (host.warning, modal.warning) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    };
    modal
}

/// Checks every recorded host instance against the host service manager. A
/// deployment whose service is down (reboot, crash, `launchctl remove`) is kept
/// in state, marked pending: its stop record stays, the planner shows a create,
/// and apply restarts it (releasing whatever is left under the label) or
/// destroy confirms its stop. Nothing is ever dropped here, and a failed check
/// only warns.
fn refresh_host(state: &mut LocalState, lookup: &dyn AppLookup) -> Refresh {
    let mut out = Refresh::default();
    let mut unchecked = Vec::new();
    for record in crate::commands::apply::effective_stops(Some(state)) {
        if record.backend.is_modal()
            || !state
                .graph
                .deployments
                .iter()
                .any(|d| d.id == record.deployment)
            || state.pending.contains(&record.deployment)
        {
            continue;
        }
        match lookup.host_unit(&record) {
            Ok(HostUnit::Down) => {
                state.pending.push(record.deployment.clone());
                out.notes.push(format!(
                    "deployment {}: {} is not running (reboot, crash or manual removal); kept in \
                     state as pending: apply will restart it, destroy will confirm its stop",
                    record.deployment,
                    record.describe()
                ));
            }
            Ok(HostUnit::Running | HostUnit::Unknown) => {}
            Err(e) => unchecked.push(format!("{}: {e}", record.deployment)),
        }
    }
    if !unchecked.is_empty() {
        out.warning = Some(format!(
            "could not check host services ({}); using local state as-is",
            unchecked.join("; ")
        ));
    }
    out
}

/// `(deployment id, app name)` for every Modal deployment recorded in state.
/// The name is the one recorded in the stop record, so an app created under
/// an older naming scheme is still looked up by the name it really has.
fn recorded_modal_apps(state: &LocalState) -> Vec<(String, String)> {
    let stops = crate::commands::apply::effective_stops(Some(state));
    let mut recorded = Vec::new();
    for d in state.graph.deployments.iter().filter(|d| {
        state
            .graph
            .hardware_profiles
            .iter()
            .any(|h| h.id == d.hardware_profile && h.provider == MODAL_PROVIDER)
    }) {
        let mut names: Vec<String> = stops
            .iter()
            .filter(|r| r.deployment == d.id && r.backend.is_modal())
            .map(|r| r.identity.clone())
            .collect();
        names.sort();
        names.dedup();
        if names.is_empty() {
            names.push(app_name_for(d.id.as_str()));
        }
        recorded.extend(names.into_iter().map(|n| (d.id.to_string(), n)));
    }
    recorded
}

fn refresh_modal(state: &mut LocalState, lookup: &dyn AppLookup, strict: bool) -> Refresh {
    let recorded = recorded_modal_apps(state);
    if recorded.is_empty() {
        return Refresh::default();
    }

    // Refuse to reason about apps recorded under a different workspace. With
    // Modal instances recorded, an unknown workspace is a refusal too (a
    // warning for read-only `plan`).
    let active = match lookup.active_workspace() {
        Ok(active) => active,
        Err(error) => {
            let msg = format!(
                "cannot determine the active Modal workspace ({error}); \
                 recorded Modal instances cannot be placed in a workspace"
            );
            return if strict {
                Refresh {
                    mismatch: Some(format!(
                        "{msg}. Refusing to touch them. Check MODAL_TOKEN_ID and \
                         MODAL_TOKEN_SECRET and retry; nothing was changed"
                    )),
                    ..Refresh::default()
                }
            } else {
                Refresh {
                    warning: Some(format!("{msg}; planning against the recorded state as-is")),
                    ..Refresh::default()
                }
            };
        }
    };
    if let Some(active) = active {
        match prove_workspaces(state, lookup, &active) {
            Ok(()) => {}
            Err(ProofError::Mismatch(msg)) => {
                return Refresh {
                    mismatch: Some(msg),
                    ..Refresh::default()
                };
            }
            Err(ProofError::Unlisted(msg)) if strict => {
                return Refresh {
                    mismatch: Some(msg),
                    ..Refresh::default()
                };
            }
            // Read-only `plan` only warns when it cannot list the apps.
            Err(ProofError::Unlisted(msg)) => {
                return Refresh {
                    warning: Some(format!("{msg}; planning against the recorded state as-is")),
                    ..Refresh::default()
                };
            }
        }
    }

    let names: Vec<String> = recorded.iter().map(|(_, app)| app.clone()).collect();
    let live = match lookup.live_apps(&names) {
        Ok(live) => live,
        Err(error) => {
            return Refresh {
                notes: Vec::new(),
                warning: Some(format!(
                    "could not check Modal for live apps ({error}); using local state as-is"
                )),
                mismatch: None,
            };
        }
    };

    let mut notes = Vec::new();
    for (id, app) in recorded.iter().filter(|(_, app)| !live.contains(app)) {
        // Never drop an instance's stop identity on a name lookup alone: a
        // pending deployment, or one with a recorded app id, stays in state.
        if state.pending.iter().any(|p| p.as_str() == id) {
            continue;
        }
        // Not found by name is not proof the instance is gone (wrong
        // workspace, unpublished app): never drop its stop identity. Keep it
        // pending, so apply recreates it and destroy confirms the stop.
        state.pending.push(id.as_str().into());
        let how = match recorded_app_id(state, id) {
            Some(app_id) => format!("recorded app id {app_id}"),
            None => "no app id recorded".to_owned(),
        };
        notes.push(format!(
            "deployment {id}: app {app} not found on Modal by name; kept in state as pending \
             ({how}): apply will recreate it, destroy will try to confirm its stop"
        ));
    }
    Refresh {
        notes,
        warning: None,
        mismatch: None,
    }
}

/// Loads the project state and reconciles it with Modal. Drift notes go to
/// `out` (stderr when `output` is JSON, to keep stdout a single JSON value);
/// warnings always go to stderr.
pub(crate) fn load_refreshed_state(
    project: &Project,
    lookup: &dyn AppLookup,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<Option<LocalState>, CliError> {
    Ok(load_refreshed_mode(project, lookup, output, out, false)?.0)
}

/// Like [`load_refreshed_state`], also reporting whether the refresh dropped
/// anything from the recorded state (so a caller can persist the pruned state).
pub(crate) fn load_refreshed(
    project: &Project,
    lookup: &dyn AppLookup,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<(Option<LocalState>, bool), CliError> {
    load_refreshed_mode(project, lookup, output, out, true)
}

fn load_refreshed_mode(
    project: &Project,
    lookup: &dyn AppLookup,
    output: OutputFormat,
    out: &mut dyn Write,
    strict: bool,
) -> Result<(Option<LocalState>, bool), CliError> {
    let Some(mut state) = project.load_state()? else {
        return Ok((None, false));
    };
    let refresh = refresh_state_mode(&mut state, lookup, strict);
    if let Some(mismatch) = refresh.mismatch {
        return Err(CliError::Message(mismatch));
    }
    if let Some(warning) = &refresh.warning {
        let _ = writeln!(std::io::stderr(), "warning: {warning}");
    }
    let mut stderr = std::io::stderr();
    let sink: &mut dyn Write = if output.is_json() { &mut stderr } else { out };
    for note in &refresh.notes {
        writeln!(sink, "drift: {note}").map_err(|e| CliError::io("write failed", e))?;
    }
    Ok((Some(state), !refresh.notes.is_empty()))
}

/// Removes what `amicu modal stop` just stopped from the project state: the
/// stop records for Modal app `app_name` with app id `stopped_id` (or no app id
/// yet), and, once the deployment has no record left, the deployment and its
/// endpoint. Records for anything else (another app id, a host instance) are
/// kept. Refuses when the active workspace differs from a record's. Returns the
/// deployment id removed, or `None` when the state does not hold that app.
pub(crate) fn forget_app(
    project: &Project,
    app_name: &str,
    stopped_id: &str,
    active: Option<&Workspace>,
) -> Result<Option<String>, CliError> {
    // No state file: nothing to update, and do not create `.amicu/`.
    if !project.state_file().exists() {
        return Ok(None);
    }
    let _lock = StateLock::acquire(&project.state_dir(), "modal stop", None)?;
    let Some(mut state) = project.load_state()? else {
        return Ok(None);
    };
    let mut records = crate::commands::apply::effective_stops(Some(&state));
    // The deployment whose recorded Modal app is named `app_name`: the name in
    // the record, whatever scheme it was created under.
    let Some(id) = records
        .iter()
        .find(|r| r.backend.is_modal() && r.identity == app_name)
        .map(|r| r.deployment.to_string())
    else {
        return Ok(None);
    };
    if let Some(active) = active {
        for r in records
            .iter()
            .filter(|r| r.deployment.as_str() == id && r.backend.is_modal())
        {
            check_workspace(
                &id,
                r.workspace.as_deref(),
                r.environment.as_deref(),
                active,
            )?;
        }
    }
    // Only the records this stop actually stopped.
    records.retain(|r| {
        !(r.deployment.as_str() == id
            && r.backend.is_modal()
            && r.identity == app_name
            && match r.app_id.as_deref() {
                Some(a) => a == stopped_id,
                // Without a known workspace, never match an id-less record
                // by name: it could be another workspace's app.
                None => active.is_some(),
            })
    });
    let others_remain = records.iter().any(|r| r.deployment.as_str() == id);
    state.stops = records;
    if others_remain {
        // Something else of this deployment may still run: keep it in state.
        project.save_state(&state)?;
        return Ok(Some(id));
    }
    state.graph.deployments.retain(|d| d.id.as_str() != id);
    state.endpoints.retain(|h| h.deployment.as_str() != id);
    state.pending.retain(|p| p.as_str() != id);
    project.save_state(&state)?;
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use amicu_runtime::RuntimeHandle;

    use super::*;
    use crate::planner::build_plan;

    const MODAL_CONFIG: &str = r#"resource "model" "support_chat" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-7B-Instruct"
  }
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "4GiB"
  }
}

resource "hardware_profile" "cloud_gpu" {
  provider = "modal"
  accelerator {
    kind   = "gpu"
    model  = "h100"
    count  = 1
    memory = "80GiB"
  }
  memory = "80GiB"
}

resource "deployment" "support_modal" {
  model    = model.support_chat.id
  hardware = hardware_profile.cloud_gpu.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}
"#;

    const APP: &str = "amicu-support-modal";

    /// Mock lookup: a fixed live set, or a failure.
    struct Fake(Result<Vec<&'static str>, &'static str>);

    impl AppLookup for Fake {
        fn live_apps(&self, _: &[String]) -> Result<HashSet<String>, String> {
            match &self.0 {
                Ok(live) => Ok(live.iter().map(|s| (*s).to_owned()).collect()),
                Err(e) => Err((*e).to_owned()),
            }
        }
    }

    fn state_for(config: &str) -> LocalState {
        let graph = amicu_hcl::parse_graph(config).expect("fixture parses");
        let endpoints = graph
            .deployments
            .iter()
            .map(|d| RuntimeHandle {
                id: "app-id".to_owned(),
                deployment: d.id.clone(),
                endpoint: "https://example.modal.run".to_owned(),
                exposure: amicu_runtime::EndpointExposure::Public {
                    gateway: amicu_runtime::GatewayAuth::ModalProxyToken,
                },
            })
            .collect();
        LocalState {
            version: LocalState::VERSION.to_owned(),
            project_id: None,
            pending: Vec::new(),
            stops: Vec::new(),
            fingerprints: std::collections::BTreeMap::default(),
            serial: 0,
            purge_pending: Vec::new(),
            extra: std::collections::BTreeMap::default(),
            graph,
            endpoints,
        }
    }

    #[test]
    fn live_app_keeps_state() {
        let mut state = state_for(MODAL_CONFIG);
        let refresh = refresh_state(&mut state, &Fake(Ok(vec![APP])));
        assert_eq!(refresh, Refresh::default());
        assert_eq!(state.graph.deployments.len(), 1);
        assert_eq!(state.endpoints.len(), 1);
    }

    #[test]
    fn missing_app_by_name_is_kept_pending_and_plan_shows_create() {
        let config = amicu_hcl::parse_graph(MODAL_CONFIG).unwrap();
        // Legacy state: no app id was ever recorded, so a name lookup is all
        // there is to go on.
        let mut state = state_for(MODAL_CONFIG);
        state.endpoints.clear();
        // Stale state: the planner sees no change.
        let before = build_plan(&config, Some(&state.graph)).unwrap();
        assert_eq!(before.items.len(), 0);

        let refresh = refresh_state(&mut state, &Fake(Ok(vec![])));
        assert_eq!(refresh.notes.len(), 1);
        assert!(refresh.notes[0].contains(APP), "{:?}", refresh.notes);
        // Not found by name is not proof it is gone: kept, as pending, with
        // nothing dropped.
        assert_eq!(state.graph.deployments.len(), 1);
        assert_eq!(state.pending.len(), 1);

        // Pending is planned as absent, so apply recreates it.
        let mut planning = state.graph.clone();
        planning
            .deployments
            .retain(|d| !state.pending.contains(&d.id));
        let plan = build_plan(&config, Some(&planning)).unwrap();
        assert_eq!(plan.items.len(), 1, "stopped app must be recreated");
    }

    /// A recorded Modal app id is the instance's stop identity: a name lookup
    /// that finds nothing must not drop it. It is kept as pending, so apply
    /// recreates it and destroy can still confirm the stop by id.
    #[test]
    fn a_recorded_app_id_is_never_pruned_on_a_name_lookup() {
        let mut state = state_for(MODAL_CONFIG);
        assert_eq!(state.endpoints.len(), 1, "fixture records an app id");
        let refresh = refresh_state(&mut state, &Fake(Ok(vec![])));
        assert_eq!(refresh.notes.len(), 1);
        assert!(refresh.notes[0].contains("pending"), "{:?}", refresh.notes);
        assert_eq!(state.graph.deployments.len(), 1);
        assert_eq!(state.endpoints.len(), 1);
        assert_eq!(state.pending.len(), 1);
    }

    #[test]
    fn a_pending_deployment_is_never_pruned_or_renoted() {
        let mut state = state_for(MODAL_CONFIG);
        state.endpoints.clear();
        state.pending = vec![state.graph.deployments[0].id.clone()];
        let refresh = refresh_state(&mut state, &Fake(Ok(vec![])));
        assert_eq!(refresh.notes.len(), 0);
        assert_eq!(state.graph.deployments.len(), 1);
    }

    #[test]
    fn lookup_failure_warns_and_keeps_state() {
        let mut state = state_for(MODAL_CONFIG);
        let refresh = refresh_state(&mut state, &Fake(Err("no credentials")));
        assert_eq!(refresh.notes.len(), 0);
        assert!(refresh.warning.unwrap().contains("no credentials"));
        assert_eq!(state.graph.deployments.len(), 1);
        assert_eq!(state.endpoints.len(), 1);
    }

    #[test]
    fn non_modal_state_never_calls_lookup() {
        let local = MODAL_CONFIG.replace("\"modal\"", "\"local_apple\"");
        let mut state = state_for(&local);
        // A lookup that would warn if it were consulted.
        let refresh = refresh_state(&mut state, &Fake(Err("must not be called")));
        assert_eq!(refresh, Refresh::default());
        assert_eq!(state.graph.deployments.len(), 1);
    }

    #[test]
    fn forget_app_removes_deployment_and_endpoint() {
        let dir = std::env::temp_dir().join(format!("amicu-refresh-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);

        // No state file: outside a project / nothing recorded.
        assert_eq!(forget_app(&project, APP, "app-id", None).unwrap(), None);

        project.save_state(&state_for(MODAL_CONFIG)).unwrap();
        assert_eq!(
            forget_app(&project, "amicu-other", "app-id", None).unwrap(),
            None
        );
        assert_eq!(
            forget_app(&project, APP, "app-id", None)
                .unwrap()
                .as_deref(),
            Some("support_modal")
        );
        let state = project.load_state().unwrap().unwrap();
        assert_eq!(state.graph.deployments.len(), 0);
        assert_eq!(state.endpoints.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_app_skips_when_state_is_locked() {
        let dir = std::env::temp_dir().join(format!("amicu-forget-locked-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);
        project.save_state(&state_for(MODAL_CONFIG)).unwrap();

        let held = StateLock::acquire(&project.state_dir(), "apply", None).unwrap();
        let err = forget_app(&project, APP, "app-id", None).unwrap_err();
        assert!(matches!(err, CliError::StateLocked(_)), "{err}");
        // State untouched while locked.
        let state = project.load_state().unwrap().unwrap();
        assert_eq!(state.graph.deployments.len(), 1);
        assert_eq!(state.endpoints.len(), 1);

        drop(held);
        assert_eq!(
            forget_app(&project, APP, "app-id", None)
                .unwrap()
                .as_deref(),
            Some("support_modal")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_app_outside_a_project_creates_nothing() {
        let dir = std::env::temp_dir().join(format!("amicu-forget-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);
        assert_eq!(forget_app(&project, APP, "app-id", None).unwrap(), None);
        assert!(!dir.join(".amicu").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_project_scoped_app_is_looked_up_and_forgotten_by_its_recorded_name() {
        let mut state = state_for(MODAL_CONFIG);
        let id = state.graph.deployments[0].id.clone();
        let scoped = amicu_modal::supervisor::project_app_name("aaaa1111", id.as_str());
        assert_ne!(scoped, APP);
        state.stops = vec![crate::stops::StopRecord::modal_in(
            "aaaa1111",
            &id,
            Some("app-id".to_owned()),
            None,
        )];
        // Live under its recorded name: no drift. The legacy name alone is not it.
        let live: &'static str = Box::leak(scoped.clone().into_boxed_str());
        assert_eq!(
            refresh_state(&mut state.clone(), &Fake(Ok(vec![live]))),
            Refresh::default()
        );
        let refresh = refresh_state(&mut state, &Fake(Ok(vec![APP])));
        assert_eq!(state.pending.len(), 1, "{refresh:?}");
        assert!(refresh.notes[0].contains(&scoped), "{refresh:?}");

        let dir = std::env::temp_dir().join(format!("amicu-forget-scoped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);
        project.save_state(&state).unwrap();
        assert_eq!(forget_app(&project, APP, "app-id", None).unwrap(), None);
        assert_eq!(
            forget_app(&project, &scoped, "app-id", None)
                .unwrap()
                .as_deref(),
            Some(id.as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_app_skips_non_modal_deployments() {
        let dir = std::env::temp_dir().join(format!("amicu-forget-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);
        let local = MODAL_CONFIG.replace("\"modal\"", "\"local_apple\"");
        project.save_state(&state_for(&local)).unwrap();
        assert_eq!(forget_app(&project, APP, "app-id", None).unwrap(), None);
        let state = project.load_state().unwrap().unwrap();
        assert_eq!(state.graph.deployments.len(), 1);
        assert_eq!(state.endpoints.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_mode_routes_drift_notes_away_from_stdout() {
        let dir = std::env::temp_dir().join(format!("amicu-json-notes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);
        let mut legacy = state_for(MODAL_CONFIG);
        legacy.endpoints.clear();
        project.save_state(&legacy).unwrap();

        let mut out = Vec::new();
        let (state, pruned) =
            load_refreshed(&project, &Fake(Ok(vec![])), OutputFormat::Json, &mut out).unwrap();
        assert!(pruned);
        assert_eq!(state.unwrap().pending.len(), 1);
        assert!(out.is_empty(), "JSON mode must keep stdout clean");

        let mut out = Vec::new();
        load_refreshed(&project, &Fake(Ok(vec![])), OutputFormat::Human, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("drift:"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct WsFake(Workspace, Option<HashSet<String>>);

    impl AppLookup for WsFake {
        fn live_apps(&self, names: &[String]) -> Result<HashSet<String>, String> {
            Ok(names.iter().cloned().collect())
        }
        fn active_workspace(&self) -> Result<Option<Workspace>, String> {
            Ok(Some(self.0.clone()))
        }
        fn app_ids(&self) -> Result<Option<HashSet<String>>, String> {
            Ok(self.1.clone())
        }
    }

    struct BrokenWorkspace;

    impl AppLookup for BrokenWorkspace {
        fn live_apps(&self, names: &[String]) -> Result<HashSet<String>, String> {
            Ok(names.iter().cloned().collect())
        }
        fn active_workspace(&self) -> Result<Option<Workspace>, String> {
            Err("missing credentials".to_owned())
        }
    }

    fn acme(name: &str) -> Workspace {
        Workspace {
            name: name.to_owned(),
            environment: crate::modal_ops::DEFAULT_ENVIRONMENT.to_owned(),
        }
    }

    fn modal_state_in(workspace: &str) -> LocalState {
        let mut state = state_for(MODAL_CONFIG);
        let id = state.graph.deployments[0].id.clone();
        state.stops = vec![crate::stops::StopRecord::modal(
            &id,
            Some("app-id".to_owned()),
            Some(&acme(workspace)),
        )];
        state
    }

    #[test]
    fn refresh_refuses_when_the_active_workspace_differs() {
        let mut state = modal_state_in("acme");
        let refresh = refresh_state(&mut state, &WsFake(acme("other-co"), None));
        let msg = refresh.mismatch.expect("mismatch");
        assert!(msg.contains("acme") && msg.contains("other-co"), "{msg}");
        assert!(msg.contains("MODAL_TOKEN_ID"), "{msg}");
        assert_eq!(state.pending.len(), 0, "nothing touched");

        let project_dir =
            std::env::temp_dir().join(format!("amicu-ws-refresh-{}", std::process::id()));
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = Project::new(&project_dir);
        project.save_state(&modal_state_in("acme")).unwrap();
        let mut out = Vec::new();
        let err = load_refreshed(
            &project,
            &WsFake(acme("other-co"), None),
            OutputFormat::Human,
            &mut out,
        )
        .expect_err("refresh must refuse");
        assert!(err.to_string().contains("other-co"), "{err}");
        let _ = std::fs::remove_dir_all(&project_dir);

        // Same workspace: fine.
        let mut state = modal_state_in("acme");
        assert!(
            refresh_state(&mut state, &WsFake(acme("acme"), None))
                .mismatch
                .is_none()
        );
    }

    #[test]
    fn an_unknown_workspace_with_modal_records_is_a_refusal() {
        let mut state = modal_state_in("acme");
        let refresh = refresh_state(&mut state, &BrokenWorkspace);
        let msg = refresh.mismatch.expect("must refuse");
        assert!(
            msg.contains("cannot determine the active Modal workspace"),
            "{msg}"
        );
    }

    /// v1 state has no stop records; its derived Modal record must be proven
    /// to belong to the active workspace, and is then stamped with it.
    #[test]
    fn v1_derived_records_are_proven_then_stamped_or_refused() {
        let mut state = state_for(MODAL_CONFIG);
        assert_eq!(state.stops.len(), 0);
        let ids: HashSet<String> = ["app-id".to_owned()].into_iter().collect();
        let refresh = refresh_state(&mut state, &WsFake(acme("acme"), Some(ids)));
        assert!(refresh.mismatch.is_none());
        let rec = &state.stops[0];
        assert_eq!(rec.workspace.as_deref(), Some("acme"));

        let mut state = state_for(MODAL_CONFIG);
        let refresh = refresh_state(&mut state, &WsFake(acme("other-co"), Some(HashSet::new())));
        let msg = refresh.mismatch.expect("unproven");
        assert!(msg.contains("cannot see that app"), "{msg}");
        assert_eq!(state.stops.len(), 0, "nothing stamped");
    }

    /// Without a known active workspace, only the exact stopped app id is
    /// forgotten; an id-less record is never matched by name.
    #[test]
    fn forget_app_with_an_unknown_workspace_never_removes_id_less_records() {
        let mut state = modal_state_in("acme");
        let id = state.graph.deployments[0].id.clone();
        state.stops = vec![crate::stops::StopRecord::modal(&id, None, None)];
        let (project, dir) = project_with(&state, "unknown-ws");
        forget_app(&project, APP, "app-id", None).unwrap();
        let after = project.load_state().unwrap().unwrap();
        assert_eq!(after.stops.len(), 1, "id-less record kept");
        assert_eq!(after.graph.deployments.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn project_with(state: &LocalState, tag: &str) -> (Project, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("amicu-forget-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);
        project.save_state(state).unwrap();
        (project, dir)
    }

    /// `amicu modal stop` removes only the record it stopped; a record for a
    /// different app id (or a host instance) keeps the deployment in state.
    #[test]
    fn forget_app_removes_only_the_records_it_stopped() {
        let mut state = modal_state_in("acme");
        let id = state.graph.deployments[0].id.clone();
        state.stops.push(crate::stops::StopRecord::modal(
            &id,
            Some("app-OTHER".to_owned()),
            Some(&acme("acme")),
        ));
        let (project, dir) = project_with(&state, "partial");
        let removed = forget_app(&project, APP, "app-id", Some(&acme("acme"))).unwrap();
        assert_eq!(removed.as_deref(), Some(id.as_str()));
        let after = project.load_state().unwrap().unwrap();
        assert_eq!(after.graph.deployments.len(), 1, "other record keeps it");
        let ids: Vec<_> = after.stops.iter().map(|r| r.app_id.as_deref()).collect();
        assert_eq!(ids, [Some("app-OTHER")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_app_refuses_in_another_workspace_and_changes_nothing() {
        let state = modal_state_in("acme");
        let (project, dir) = project_with(&state, "ws");
        let err = forget_app(&project, APP, "app-id", Some(&acme("other-co")))
            .expect_err("workspace mismatch");
        assert!(err.to_string().contains("other-co"), "{err}");
        let after = project.load_state().unwrap().unwrap();
        assert_eq!(after.stops, state.stops);
        assert_eq!(after.graph.deployments.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_app_without_state_does_not_create_the_state_dir() {
        let dir = std::env::temp_dir().join(format!("amicu-forget-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);
        assert_eq!(forget_app(&project, APP, "app-id", None).unwrap(), None);
        assert!(!project.state_dir().exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_only_warns_when_the_workspace_is_unknown_and_marks_nothing_pending() {
        let mut state = modal_state_in("acme");
        let refresh = refresh_state_mode(&mut state, &BrokenWorkspace, false);
        assert!(refresh.mismatch.is_none());
        let warning = refresh.warning.expect("warns");
        assert!(warning.contains("as-is"), "{warning}");
        assert_eq!(state.pending.len(), 0);
        assert_eq!(refresh.notes.len(), 0);

        // And through the plan entry point.
        let dir = std::env::temp_dir().join(format!("amicu-plan-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);
        project.save_state(&modal_state_in("acme")).unwrap();
        let mut out = Vec::new();
        let loaded =
            load_refreshed_state(&project, &BrokenWorkspace, OutputFormat::Human, &mut out)
                .unwrap()
                .unwrap();
        assert_eq!(loaded.pending.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct CountingList(std::cell::Cell<u32>);

    impl AppLookup for CountingList {
        fn live_apps(&self, names: &[String]) -> Result<HashSet<String>, String> {
            Ok(names.iter().cloned().collect())
        }
        fn active_workspace(&self) -> Result<Option<Workspace>, String> {
            Ok(Some(acme("acme")))
        }
        fn app_ids(&self) -> Result<Option<HashSet<String>>, String> {
            self.0.set(self.0.get() + 1);
            Ok(Some(
                ["app-id".to_owned(), "app-id-2".to_owned()]
                    .into_iter()
                    .collect(),
            ))
        }
    }

    #[test]
    fn the_app_list_is_fetched_once_per_refresh() {
        let mut state = state_for(MODAL_CONFIG);
        let id = state.graph.deployments[0].id.clone();
        state.stops = vec![
            crate::stops::StopRecord::modal(&id, Some("app-id".to_owned()), None),
            crate::stops::StopRecord::modal(&id, Some("app-id-2".to_owned()), None),
        ];
        let lookup = CountingList(std::cell::Cell::new(0));
        let refresh = refresh_state(&mut state, &lookup);
        assert!(refresh.mismatch.is_none(), "{:?}", refresh.mismatch);
        assert_eq!(lookup.0.get(), 1);
        assert!(
            state
                .stops
                .iter()
                .all(|r| r.workspace.as_deref() == Some("acme"))
        );
    }
}
