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
    /// A stable id for this project, minted when state is first written. It
    /// scopes host service labels so two projects never share a launchd job
    /// or systemd unit. Absent in state written before v3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
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
    /// Digests of the out-of-graph inputs (secret pins, image, binary path)
    /// each deployment was last confirmed started with, so a change to one is
    /// planned as an Update. Absent in older state: nothing is then compared.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub fingerprints: std::collections::BTreeMap<
        amicu_core::id::DeploymentId,
        amicu_engine::fingerprint::Fingerprint,
    >,
}

impl LocalState {
    /// Written by this version. v2 adds `pending` and `stops`; v3 adds
    /// `project_id` and the per-record files and volumes, which an older
    /// binary would silently drop on its next save.
    pub const VERSION: &'static str = "v3";
    /// Versions this binary can read.
    pub const SUPPORTED: &'static [&'static str] = &["v1", "v2", "v3"];
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

    /// A fresh project id: 8 hex digits mixing the absolute root, the clock
    /// and the pid. Stored in state, so it only has to be unlikely to collide.
    pub(crate) fn mint_id(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::path::absolute(&self.root)
            .unwrap_or_else(|_| self.root.clone())
            .hash(&mut h);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
            .hash(&mut h);
        std::process::id().hash(&mut h);
        format!("{:08x}", h.finish() & 0xffff_ffff)
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
        let problem = match fs::read_to_string(&path) {
            Ok(text) => match parse_state(&text) {
                Ok(state) => return Ok(Some(state)),
                // A newer state must never be "restored" from an older backup.
                Err(StateParseError::Unsupported(msg)) => {
                    return Err(CliError::Message(format!("{}: {msg}", path.display())));
                }
                Err(StateParseError::Corrupt(e)) => {
                    format!("{} is corrupt or unreadable ({e})", path.display())
                }
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
                shell_quote(&bak),
                shell_quote(&path)
            )))
        } else {
            Err(CliError::Message(problem))
        }
    }

    fn state_backup_file(&self) -> PathBuf {
        self.state_dir().join("state.json.bak")
    }

    fn backup_parses(&self) -> bool {
        fs::read_to_string(self.state_backup_file()).is_ok_and(|t| parse_state(&t).is_ok())
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
        // Never overwrite a state this amicu does not understand (written by a
        // newer version): that would silently downgrade or destroy it.
        if let Ok(existing) = fs::read_to_string(&path)
            && let Err(StateParseError::Unsupported(msg)) = parse_state(&existing)
        {
            return Err(CliError::Message(format!(
                "refusing to overwrite {}: it {msg}",
                path.display()
            )));
        }
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
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let mut file = opts.open(&tmp).map_err(|e| write_err(&tmp, e))?;
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| write_err(&tmp, e))?;
            drop(file);
            self.rotate_backup(&dir, &unique)?;
            fs::rename(&tmp, &path).map_err(|e| write_err(&path, e))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
            return result;
        }
        // The new state is already in place; a failed directory sync only
        // weakens crash durability, so warn rather than fail the save.
        if let Err(e) = sync_dir(&dir) {
            eprintln!(
                "warning: state saved but could not sync {}: {e}",
                dir.display()
            );
        }
        Ok(())
    }

    /// Keep the current `state.json` as `state.json.bak`, but only when it is
    /// itself valid, so a corrupt file never displaces a good backup. The
    /// backup is a hard link (the old inode survives the later rename), with
    /// a copy as fallback; it is installed by rename so it is never torn.
    fn rotate_backup(&self, dir: &Path, unique: &str) -> Result<(), CliError> {
        let path = self.state_file();
        let valid = fs::read_to_string(&path).is_ok_and(|t| parse_state(&t).is_ok());
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

/// Why a state file did not load.
enum StateParseError {
    /// Not valid state JSON.
    Corrupt(String),
    /// Valid JSON of a version this amicu does not understand.
    Unsupported(String),
}

/// Parse state text, accepting only the versions this binary understands.
/// Used for loading and for deciding whether a file (or backup) is valid, so
/// an unsupported-version file is never treated as a good generation.
fn parse_state(text: &str) -> Result<LocalState, StateParseError> {
    let state: LocalState =
        serde_json::from_str(text).map_err(|e| StateParseError::Corrupt(e.to_string()))?;
    if LocalState::SUPPORTED.contains(&state.version.as_str()) {
        Ok(state)
    } else {
        Err(StateParseError::Unsupported(format!(
            "has state version '{}', which this amicu does not understand (supported: {}); \
             upgrade amicu rather than overwrite it",
            state.version,
            LocalState::SUPPORTED.join(", ")
        )))
    }
}

/// Single-quote a path for safe pasting into a POSIX shell.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
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
        // The generation tag rides in `pending`: `version` must stay a
        // supported one or the state would (rightly) not load.
        LocalState {
            version: LocalState::VERSION.to_owned(),
            project_id: None,
            graph: Graph::new(),
            endpoints: Vec::new(),
            pending: vec![tag.into()],
            stops: Vec::new(),
            fingerprints: std::collections::BTreeMap::default(),
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

        assert_eq!(p.load_state().unwrap().unwrap().pending[0].as_str(), "gen3");
        let bak: LocalState =
            serde_json::from_str(&fs::read_to_string(p.state_backup_file()).unwrap()).unwrap();
        assert_eq!(bak.pending[0].as_str(), "gen2");
        let leftovers: Vec<_> = fs::read_dir(p.state_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_hint_shell_quotes_paths() {
        let root = scratch("quote it");
        let p = Project::new(&root);
        p.save_state(&state("a")).unwrap();
        p.save_state(&state("b")).unwrap();
        fs::write(p.state_file(), "x").unwrap();
        let err = p.load_state().unwrap_err().to_string();
        assert!(err.contains("cp '") && err.contains("quote it"), "{err}");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("mode");
        let p = Project::new(&root);
        p.save_state(&state("a")).unwrap();
        let mode = fs::metadata(p.state_file()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn leftover_temp_files_from_a_crash_are_ignored() {
        let root = scratch("leftover");
        let p = Project::new(&root);
        p.save_state(&state("good")).unwrap();
        fs::write(p.state_dir().join("state.json.123.abc.tmp"), "{ torn").unwrap();
        assert_eq!(p.load_state().unwrap().unwrap().pending[0].as_str(), "good");
        p.save_state(&state("next")).unwrap();
        assert_eq!(p.load_state().unwrap().unwrap().pending[0].as_str(), "next");
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
        assert_eq!(bak.pending[0].as_str(), "old");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_unsupported_version_is_never_a_valid_state_or_backup() {
        let root = scratch("unsupported");
        let p = Project::new(&root);
        p.save_state(&state("good")).unwrap();
        p.save_state(&state("good2")).unwrap();
        // A newer-version file replaces state.json (e.g. written by a newer amicu).
        let mut newer = serde_json::to_value(state("future")).unwrap();
        newer["version"] = "v9".into();
        fs::write(p.state_file(), newer.to_string()).unwrap();
        let err = p.load_state().expect_err("unsupported");
        assert!(err.to_string().contains("v9"), "{err}");
        assert!(
            !err.to_string().contains("cp "),
            "never advise restoring over it: {err}"
        );
        // Saving must refuse to overwrite it (and so cannot rotate it into
        // the backup either).
        let err = p
            .save_state(&state("next"))
            .expect_err("must not overwrite");
        assert!(err.to_string().contains("refusing to overwrite"), "{err}");
        assert_eq!(
            fs::read_to_string(p.state_file()).unwrap(),
            newer.to_string()
        );
        let bak: LocalState =
            serde_json::from_str(&fs::read_to_string(p.state_backup_file()).unwrap()).unwrap();
        assert_eq!(
            bak.pending[0].as_str(),
            "good",
            "backup kept the last valid one"
        );
        // And an unsupported backup does not count as a valid previous generation.
        fs::write(p.state_backup_file(), newer.to_string()).unwrap();
        assert!(!p.backup_parses());
        let _ = fs::remove_dir_all(&root);
    }
}
