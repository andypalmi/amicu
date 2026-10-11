//! Reconciling local state with what is actually live on Modal.
//!
//! `amicu modal stop` (or the Modal dashboard) can end an app behind the
//! project's back, leaving `.amicu/state/state.json` recording a deployment
//! that no longer exists; diffing the config against that stale state yields
//! "No changes" forever (amicu-planning#175). Before `plan`/`apply` diff,
//! [`refresh_state`] asks Modal which recorded apps are still live and drops
//! the ones that are not, so the planner shows a create.

use std::collections::HashSet;
use std::io::Write;

use amicu_modal::supervisor::app_name_for;
use amicu_modal::{Credentials, ModalClient, ModalError};

use crate::OutputFormat;
use crate::commands::build_runtime;
use crate::error::CliError;
use crate::project::{LocalState, Project};

/// Hardware profile `provider` value of a Modal-backed deployment.
const MODAL_PROVIDER: &str = "modal";

/// Looks up which Modal apps are live. A trait so unit tests mock it: no
/// network in tests.
pub(crate) trait AppLookup {
    /// Of `app_names`, the ones that exist on Modal. `Err` carries a message
    /// safe to print (no credentials).
    fn live_apps(&self, app_names: &[String]) -> Result<HashSet<String>, String>;
}

/// The real lookup: the same `find_app` call `amicu modal status` is built on.
pub(crate) struct ModalLookup;

impl AppLookup for ModalLookup {
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

/// What a refresh found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Refresh {
    /// One drift note per deployment dropped from the state.
    pub notes: Vec<String>,
    /// Set when the lookup failed and the state was kept as-is.
    pub warning: Option<String>,
}

/// Drops from `state` every Modal deployment (and its endpoint) whose app is
/// no longer live. Never fails: if the lookup errors, `state` is left as-is
/// and [`Refresh::warning`] says so.
pub(crate) fn refresh_state(state: &mut LocalState, lookup: &dyn AppLookup) -> Refresh {
    // (deployment id, app name) for every Modal deployment recorded in state.
    let recorded: Vec<(String, String)> = state
        .graph
        .deployments
        .iter()
        .filter(|d| {
            state
                .graph
                .hardware_profiles
                .iter()
                .any(|h| h.id == d.hardware_profile && h.provider == MODAL_PROVIDER)
        })
        .map(|d| (d.id.to_string(), app_name_for(d.id.as_str())))
        .collect();
    if recorded.is_empty() {
        return Refresh::default();
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
            };
        }
    };

    let mut notes = Vec::new();
    for (id, app) in recorded.iter().filter(|(_, app)| !live.contains(app)) {
        state.graph.deployments.retain(|d| d.id.as_str() != id);
        state.endpoints.retain(|h| h.deployment.as_str() != id);
        notes.push(format!(
            "deployment {id}: app {app} not found on Modal, will be recreated"
        ));
    }
    Refresh {
        notes,
        warning: None,
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
    let Some(mut state) = project.load_state()? else {
        return Ok(None);
    };
    let refresh = refresh_state(&mut state, lookup);
    if let Some(warning) = &refresh.warning {
        eprintln!("warning: {warning}");
    }
    let mut stderr = std::io::stderr();
    let sink: &mut dyn Write = if output.is_json() { &mut stderr } else { out };
    for note in &refresh.notes {
        writeln!(sink, "drift: {note}").map_err(|e| CliError::io("write failed", e))?;
    }
    Ok(Some(state))
}

/// Removes the deployment whose Modal app is `app_name`, and its endpoint,
/// from the project state. Returns the deployment id removed, or `None` when
/// the state does not hold that app.
pub(crate) fn forget_app(project: &Project, app_name: &str) -> Result<Option<String>, CliError> {
    let Some(mut state) = project.load_state()? else {
        return Ok(None);
    };
    let Some(id) = state
        .graph
        .deployments
        .iter()
        .find(|d| app_name_for(d.id.as_str()) == app_name)
        .map(|d| d.id.to_string())
    else {
        return Ok(None);
    };
    state.graph.deployments.retain(|d| d.id.as_str() != id);
    state.endpoints.retain(|h| h.deployment.as_str() != id);
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
            })
            .collect();
        LocalState {
            version: LocalState::VERSION.to_owned(),
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
    fn missing_app_is_dropped_and_plan_shows_create() {
        let config = amicu_hcl::parse_graph(MODAL_CONFIG).unwrap();
        let mut state = state_for(MODAL_CONFIG);
        // Stale state: the planner sees no change.
        let before = build_plan(&config, Some(&state.graph)).unwrap();
        assert_eq!(before.items.len(), 0);

        let refresh = refresh_state(&mut state, &Fake(Ok(vec![])));
        assert_eq!(refresh.notes.len(), 1);
        assert!(refresh.notes[0].contains(APP), "{:?}", refresh.notes);
        assert_eq!(state.graph.deployments.len(), 0);
        assert_eq!(state.endpoints.len(), 0);

        let plan = build_plan(&config, Some(&state.graph)).unwrap();
        assert_eq!(plan.items.len(), 1, "stopped app must be recreated");
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
        assert_eq!(forget_app(&project, APP).unwrap(), None);

        project.save_state(&state_for(MODAL_CONFIG)).unwrap();
        assert_eq!(forget_app(&project, "amicu-other").unwrap(), None);
        assert_eq!(
            forget_app(&project, APP).unwrap().as_deref(),
            Some("support_modal")
        );
        let state = project.load_state().unwrap().unwrap();
        assert_eq!(state.graph.deployments.len(), 0);
        assert_eq!(state.endpoints.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
