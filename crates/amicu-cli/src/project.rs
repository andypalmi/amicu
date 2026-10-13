//! Project layout: config discovery, graph loading, and local state.

use std::fs;
use std::path::{Path, PathBuf};

use amicu_core::graph::Graph;
use amicu_hcl::parse_graph;
use amicu_runtime::RuntimeHandle;
use serde::{Deserialize, Serialize};

use crate::error::CliError;

/// A rooted Amicu project directory.
#[derive(Debug, Clone)]
pub(crate) struct Project {
    root: PathBuf,
}

/// The minimal local state persisted by `apply`.
///
/// Full history/drift/locking is out of scope; this records the applied graph
/// and the endpoints that were started, so later commands can diff against it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LocalState {
    pub version: String,
    pub graph: Graph,
    #[serde(default)]
    pub endpoints: Vec<RuntimeHandle>,
    /// Deployments whose start was begun (state is written ahead of the
    /// provider call) but not confirmed. Each is also in `graph`, so it can be
    /// found and stopped from state alone even if amicu died mid-start.
    /// Absent in state written before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<amicu_core::id::DeploymentId>,
    /// How to stop every instance that may be running (see [`crate::stops`]).
    /// Written ahead of each start and removed only once a stop is confirmed.
    /// Absent in v1 state; the stop identity is then derived from the graph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stops: Vec<crate::stops::StopRecord>,
}

impl LocalState {
    /// Written by this version. v2 adds `pending` and `stops`, which a v1-only
    /// binary would silently drop on its next save.
    pub const VERSION: &'static str = "v2";
    /// Versions this binary can read.
    pub const SUPPORTED: &'static [&'static str] = &["v1", "v2"];
}

impl Project {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Discover the project rooted at the current working directory.
    pub fn discover() -> Result<Self, CliError> {
        let cwd = std::env::current_dir()
            .map_err(|e| CliError::io("cannot determine current directory", e))?;
        Ok(Self::new(cwd))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dot_dir(&self) -> PathBuf {
        self.root.join(".amicu")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dot_dir().join("state")
    }

    pub(crate) fn state_file(&self) -> PathBuf {
        self.state_dir().join("state.json")
    }

    /// Every `*.hcl` file under the project root, sorted by path.
    ///
    /// Walks recursively, skipping `.amicu/`, hidden dirs, and `target/`.
    pub fn config_files(&self) -> Result<Vec<PathBuf>, CliError> {
        let mut files = Vec::new();
        collect_hcl(&self.root, &mut files)?;
        files.sort();
        Ok(files)
    }

    /// Parse and merge every config file into a single graph.
    pub fn load_graph(&self) -> Result<Graph, CliError> {
        let files = self.config_files()?;
        if files.is_empty() {
            return Err(CliError::NoConfig(self.root.clone()));
        }
        let mut merged = Graph::new();
        for path in files {
            let text = fs::read_to_string(&path)
                .map_err(|e| CliError::io(format!("cannot read {}", path.display()), e))?;
            let graph = parse_graph(&text).map_err(|source| CliError::Hcl {
                file: path.clone(),
                source,
            })?;
            merge_into(&mut merged, graph);
        }
        Ok(merged)
    }

    pub fn load_state(&self) -> Result<Option<LocalState>, CliError> {
        let path = self.state_file();
        match fs::read_to_string(&path) {
            Ok(text) => {
                let state: LocalState = serde_json::from_str(&text)?;
                if !LocalState::SUPPORTED.contains(&state.version.as_str()) {
                    return Err(CliError::Message(format!(
                        "{} has state version '{}', which this amicu does not understand \
                         (supported: {}); upgrade amicu rather than overwrite it",
                        path.display(),
                        state.version,
                        LocalState::SUPPORTED.join(", ")
                    )));
                }
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CliError::io(format!("cannot read {}", path.display()), e)),
        }
    }

    pub fn save_state(&self, state: &LocalState) -> Result<(), CliError> {
        let dir = self.state_dir();
        fs::create_dir_all(&dir)
            .map_err(|e| CliError::io(format!("cannot create {}", dir.display()), e))?;
        let path = self.state_file();
        let text = serde_json::to_string_pretty(state)?;
        // Write a sibling temp file then rename over the target, so a crash
        // mid-write can never leave a truncated state file.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)
            .map_err(|e| CliError::io(format!("cannot write {}", tmp.display()), e))?;
        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            CliError::io(format!("cannot write {}", path.display()), e)
        })?;
        Ok(())
    }
}

/// Recursively collect `*.hcl` files under `dir`, skipping excluded subtrees.
fn collect_hcl(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), CliError> {
    let entries =
        fs::read_dir(dir).map_err(|e| CliError::io(format!("cannot read {}", dir.display()), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| CliError::io("cannot read directory entry", e))?;
        let path = entry.path();
        if path.is_dir() {
            if !is_excluded_dir(&path) {
                collect_hcl(&path, files)?;
            }
        } else if path.is_file() && path.extension().is_some_and(|e| e == "hcl") {
            files.push(path);
        }
    }
    Ok(())
}

/// Directories skipped during discovery: state, hidden dirs, and build output.
fn is_excluded_dir(path: &Path) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.starts_with('.') || name == "target",
        None => true,
    }
}

/// Merge `src`'s resources into `dst`, preserving order.
fn merge_into(dst: &mut Graph, mut src: Graph) {
    dst.models.append(&mut src.models);
    dst.hardware_profiles.append(&mut src.hardware_profiles);
    dst.deployments.append(&mut src.deployments);
    dst.routing_policies.append(&mut src.routing_policies);
    dst.data_classes.append(&mut src.data_classes);
    dst.governance_rules.append(&mut src.governance_rules);
    dst.budgets.append(&mut src.budgets);
    dst.tenants.append(&mut src.tenants);
    dst.use_cases.append(&mut src.use_cases);
    dst.credentials.append(&mut src.credentials);
}

/// Whether a graph carries no resources (e.g. a comments-only variables file).
pub(crate) fn is_empty_graph(graph: &Graph) -> bool {
    graph.models.is_empty()
        && graph.hardware_profiles.is_empty()
        && graph.deployments.is_empty()
        && graph.routing_policies.is_empty()
        && graph.data_classes.is_empty()
        && graph.governance_rules.is_empty()
        && graph.budgets.is_empty()
        && graph.tenants.is_empty()
        && graph.use_cases.is_empty()
        && graph.credentials.is_empty()
}
