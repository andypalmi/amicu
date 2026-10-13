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
}

impl LocalState {
    pub const VERSION: &'static str = "v1";
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

    fn state_file(&self) -> PathBuf {
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
        let problem = match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(state) => return Ok(Some(state)),
                Err(e) => format!("{} is corrupt or unreadable ({e})", path.display()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !self.backup_parses() {
                    return Ok(None);
                }
                format!("{} is missing", path.display())
            }
            Err(e) => return Err(CliError::io(format!("cannot read {}", path.display()), e)),
        };
        // Never silently fall back to the previous generation: it may be
        // stale and applying against it could orphan running resources.
        let bak = self.state_backup_file();
        if self.backup_parses() {
            Err(CliError::Message(format!(
                "{problem}. {} holds the previous valid state; inspect it and, if correct, \
                 restore it with `cp {} {}`",
                bak.display(),
                bak.display(),
                path.display()
            )))
        } else {
            Err(CliError::Message(problem))
        }
    }

    fn state_backup_file(&self) -> PathBuf {
        self.state_dir().join("state.json.bak")
    }

    fn backup_parses(&self) -> bool {
        fs::read_to_string(self.state_backup_file())
            .is_ok_and(|t| serde_json::from_str::<LocalState>(&t).is_ok())
    }

    /// Durably replace `state.json`: write a uniquely named temp file in the
    /// same directory, fsync it, rotate the previous generation to
    /// `state.json.bak`, rename over the target, then fsync the directory.
    pub fn save_state(&self, state: &LocalState) -> Result<(), CliError> {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let dir = self.state_dir();
        fs::create_dir_all(&dir)
            .map_err(|e| CliError::io(format!("cannot create {}", dir.display()), e))?;
        let path = self.state_file();
        let text = serde_json::to_string_pretty(state)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let unique = format!(
            "{}.{nanos:08x}{:x}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let tmp = dir.join(format!("state.json.{unique}.tmp"));
        let write_err = |p: &Path, e| CliError::io(format!("cannot write {}", p.display()), e);

        let result = (|| {
            let mut file = fs::File::create(&tmp).map_err(|e| write_err(&tmp, e))?;
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| write_err(&tmp, e))?;
            drop(file);
            self.rotate_backup(&dir, &unique)?;
            fs::rename(&tmp, &path).map_err(|e| write_err(&path, e))?;
            sync_dir(&dir).map_err(|e| write_err(&dir, e))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// Keep the current `state.json` as `state.json.bak`, but only when it is
    /// itself valid, so a corrupt file never displaces a good backup. The
    /// backup is a hard link (the old inode survives the later rename), with
    /// a copy as fallback; it is installed by rename so it is never torn.
    fn rotate_backup(&self, dir: &Path, unique: &str) -> Result<(), CliError> {
        let path = self.state_file();
        let valid =
            fs::read_to_string(&path).is_ok_and(|t| serde_json::from_str::<LocalState>(&t).is_ok());
        if !valid {
            return Ok(());
        }
        let bak = self.state_backup_file();
        let bak_tmp = dir.join(format!("state.json.{unique}.bak.tmp"));
        if fs::hard_link(&path, &bak_tmp).is_err() {
            fs::copy(&path, &bak_tmp)
                .and_then(|_| fs::File::open(&bak_tmp)?.sync_all())
                .map_err(|e| CliError::io(format!("cannot write {}", bak_tmp.display()), e))?;
        }
        fs::rename(&bak_tmp, &bak).map_err(|e| {
            let _ = fs::remove_file(&bak_tmp);
            CliError::io(format!("cannot write {}", bak.display()), e)
        })
    }
}

/// fsync a directory so a rename inside it survives power loss (unix only;
/// Windows cannot open directories as files).
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
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

#[cfg(test)]
mod durability_tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("amicu-state-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(tag: &str) -> LocalState {
        LocalState {
            version: tag.to_owned(),
            graph: Graph::new(),
            endpoints: Vec::new(),
        }
    }

    #[test]
    fn save_rotates_previous_generation_to_bak_and_leaves_no_temp_files() {
        let root = scratch("rotate");
        let p = Project::new(&root);
        p.save_state(&state("gen1")).unwrap();
        assert!(!p.state_backup_file().exists());
        p.save_state(&state("gen2")).unwrap();
        p.save_state(&state("gen3")).unwrap();

        assert_eq!(p.load_state().unwrap().unwrap().version, "gen3");
        let bak: LocalState =
            serde_json::from_str(&fs::read_to_string(p.state_backup_file()).unwrap()).unwrap();
        assert_eq!(bak.version, "gen2");
        let leftovers: Vec<_> = fs::read_dir(p.state_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn leftover_temp_files_from_a_crash_are_ignored() {
        let root = scratch("leftover");
        let p = Project::new(&root);
        p.save_state(&state("good")).unwrap();
        fs::write(p.state_dir().join("state.json.123.abc.tmp"), "{ torn").unwrap();
        assert_eq!(p.load_state().unwrap().unwrap().version, "good");
        p.save_state(&state("next")).unwrap();
        assert_eq!(p.load_state().unwrap().unwrap().version, "next");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_state_errors_and_points_at_the_backup() {
        for corrupt in [None, Some(""), Some("{ truncated")] {
            let root = scratch("corrupt");
            let p = Project::new(&root);
            p.save_state(&state("old")).unwrap();
            p.save_state(&state("new")).unwrap();
            match corrupt {
                Some(text) => fs::write(p.state_file(), text).unwrap(),
                None => fs::remove_file(p.state_file()).unwrap(),
            }
            let err = p.load_state().unwrap_err().to_string();
            assert!(err.contains("state.json.bak"), "{err}");
            // The backup is never used silently and is left untouched.
            assert!(p.backup_parses());
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn corrupt_state_without_backup_still_errors_and_missing_is_none() {
        let root = scratch("nobak");
        let p = Project::new(&root);
        assert!(p.load_state().unwrap().is_none());
        fs::create_dir_all(p.state_dir()).unwrap();
        fs::write(p.state_file(), "garbage").unwrap();
        let err = p.load_state().unwrap_err().to_string();
        assert!(err.contains("corrupt") && !err.contains(".bak"), "{err}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn saving_over_corrupt_state_keeps_the_good_backup() {
        let root = scratch("keepbak");
        let p = Project::new(&root);
        p.save_state(&state("old")).unwrap();
        p.save_state(&state("new")).unwrap();
        fs::write(p.state_file(), "garbage").unwrap();
        p.save_state(&state("fresh")).unwrap();
        let bak: LocalState =
            serde_json::from_str(&fs::read_to_string(p.state_backup_file()).unwrap()).unwrap();
        assert_eq!(bak.version, "old");
        let _ = fs::remove_dir_all(root);
    }
}
