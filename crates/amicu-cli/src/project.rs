//! Project layout: config discovery, graph loading, and local state.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use amicu_core::graph::Graph;
use amicu_hcl::parse_graph;
use amicu_runtime::RuntimeHandle;
use serde::{Deserialize, Serialize};

use crate::error::CliError;

/// A rooted Amicu project directory.
#[derive(Debug, Clone)]
pub(crate) struct Project {
    root: PathBuf,
    /// The state serial this handle last read or wrote (`None` before any
    /// load or save). [`Project::save_state`] only writes over the serial it
    /// expects, so a concurrent writer is detected instead of overwritten.
    /// Shared by clones: they are the same logical reader.
    seen_serial: Arc<Mutex<Option<u64>>>,
    /// Whether the "skipping nested amicu project" note was already printed, so
    /// it shows once per command however often the config is listed.
    nested_note_shown: Arc<AtomicBool>,
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
    /// Monotonic write counter: every save writes the serial it read plus one,
    /// and refuses to write if the file on disk no longer carries the serial
    /// that was read (see [`Project::save_state`]). Absent before v4 (read as 0).
    #[serde(default)]
    pub serial: u64,
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
    /// Files left behind by destroyed deployments (their log files), kept until
    /// `amicu destroy --purge` removes them. Not drift: the deployments are
    /// gone, only these files remain. Absent in older state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub purge_pending: Vec<crate::stops::PurgePending>,
    /// Top-level fields this amicu does not know (written by a newer build of
    /// the same version). Kept verbatim so a load-save cycle never drops them.
    ///
    /// Unknown-field rule for the rest of the document: stop records keep
    /// theirs the same way; endpoint handles with an unknown field are
    /// refused on load; and `graph` has no catch-all, so adding a field
    /// inside `graph` requires a `VERSION` bump and a migration.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

impl LocalState {
    /// Written by this version. v2 adds `pending` and `stops`; v3 adds
    /// `project_id` and the per-record files and volumes; v4 adds `serial`.
    /// An older binary would silently drop these on its next save (and a v3
    /// writer would not bump the serial, defeating the compare-and-swap).
    pub const VERSION: &'static str = "v4";
    /// Versions this binary can read: every `from` in [`MIGRATIONS`] plus
    /// [`Self::VERSION`].
    pub const SUPPORTED: &'static [&'static str] = &["v1", "v2", "v3", "v4"];
}

/// One step `from` -> `to` applied to the raw state JSON on load, so
/// [`LocalState`] only ever deserializes the current shape.
struct Migration {
    from: &'static str,
    to: &'static str,
    apply: fn(&mut serde_json::Map<String, serde_json::Value>),
}

/// Ordered, contiguous chain ending at [`LocalState::VERSION`]. A schema bump
/// appends one entry here and bumps `VERSION` and `SUPPORTED`.
const MIGRATIONS: &[Migration] = &[
    Migration {
        from: "v1",
        to: "v2",
        // v2 added `pending` and `stops`. v1 has neither; an empty `stops`
        // makes the stop identity be derived from the graph.
        apply: |state| {
            state
                .entry("pending")
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            state
                .entry("stops")
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        },
    },
    Migration {
        from: "v2",
        to: "v3",
        // v3 added `project_id` (minted on the next save when absent) and
        // per-record files and volumes (all optional): nothing to rewrite.
        apply: |_| {},
    },
    Migration {
        from: "v3",
        to: "v4",
        // v4 added `serial`; absent reads as 0 and the next save writes 1.
        apply: |_| {},
    },
];

/// Run every migration from `version` up to [`LocalState::VERSION`] in order.
fn migrate(
    mut version: String,
    state: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    while version != LocalState::VERSION {
        let step = MIGRATIONS
            .iter()
            .find(|m| m.from == version)
            .ok_or_else(|| format!("no migration from state version '{version}'"))?;
        (step.apply)(state);
        version = step.to.to_owned();
        state.insert("version".into(), version.clone().into());
    }
    Ok(())
}

impl Project {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            seen_serial: Arc::new(Mutex::new(None)),
            nested_note_shown: Arc::new(AtomicBool::new(false)),
        }
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

    /// Every config `*.hcl` file of this project, sorted by path.
    ///
    /// The root must be an amicu project: it holds a config `.hcl` file
    /// directly, or a real `.amicu/` marker (`config.yaml` or
    /// `state/state.json`; a bare `.amicu/` does not count). Otherwise (e.g. a parent directory of
    /// several unrelated projects) nothing is loaded and `NoConfig` is returned.
    /// The walk is recursive but skips hidden dirs (`.amicu/`, `.terraform/`,
    /// `.git/`), `target/`, `node_modules/`, `*.lock.hcl` files, and nested
    /// directories that are another amicu project (see
    /// [`is_nested_amicu_project`]) or a Terraform project (`.terraform/` or
    /// `.terraform.lock.hcl`). The note about a skipped amicu project prints
    /// once per `Project`.
    pub fn config_files(&self) -> Result<Vec<PathBuf>, CliError> {
        if !self.is_project_root()? {
            return Err(CliError::NoConfig(self.root.clone()));
        }
        let mut files = Vec::new();
        let mut skipped = Vec::new();
        collect_hcl(&self.root, &mut files, &mut skipped)?;
        skipped.sort();
        let first = !skipped.is_empty() && !self.nested_note_shown.swap(true, Ordering::Relaxed);
        for dir in skipped.iter().filter(|_| first) {
            eprintln!(
                "note: skipping nested amicu project {} (it has its own .amicu/); run amicu there",
                dir.display()
            );
        }
        files.sort();
        Ok(files)
    }

    /// Whether the root is an amicu project: it carries the same real marker as
    /// a nested project (see [`is_nested_amicu_project`]), or has a config
    /// `.hcl` file directly in the root. A bare `.amicu/` is not enough: the
    /// state lock creates `.amicu/state/` in whatever directory it runs from.
    pub(crate) fn is_project_root(&self) -> Result<bool, CliError> {
        if is_nested_amicu_project(&self.root) {
            return Ok(true);
        }
        let entries = fs::read_dir(&self.root)
            .map_err(|e| CliError::io(format!("cannot read {}", self.root.display()), e))?;
        for entry in entries {
            let path = entry
                .map_err(|e| CliError::io("cannot read directory entry", e))?
                .path();
            if path.is_file() && is_config_file(&path) {
                return Ok(true);
            }
        }
        Ok(false)
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
        let state = self.read_state()?;
        // The serial a later save must find on disk (0: no state yet).
        self.set_seen(Some(state.as_ref().map_or(0, |s| s.serial)));
        Ok(state)
    }

    fn seen(&self) -> Option<u64> {
        *self
            .seen_serial
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn set_seen(&self, serial: Option<u64>) {
        *self
            .seen_serial
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = serial;
    }

    fn read_state(&self) -> Result<Option<LocalState>, CliError> {
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
    ///
    /// Compare-and-swap: the write only goes through when the file on disk
    /// still carries the serial this handle last read or wrote, and stores
    /// that serial plus one. A second amicu run on the same machine that got
    /// past the state lock therefore fails this save instead of overwriting
    /// the other run; nothing is merged or discarded, and the caller surfaces
    /// the error. This is same-machine protection only, not a distributed
    /// lock: state shared through git or a synced folder is unsupported. The
    /// final check and the swap are atomic with respect to other savers (a
    /// short lock on `state.cas`); the state lock still covers the whole
    /// read-plan-apply-save sequence.
    /// TODO: a remote backend needs a server-side compare-and-swap.
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
        let (base, readable) = self.check_serial()?;
        let next = base.checked_add(1).ok_or_else(|| {
            CliError::Message(format!(
                "{} has the maximum state serial; treating it as corrupt, nothing was written",
                path.display()
            ))
        })?;
        let mut state = state.clone();
        state.serial = next;
        let text = serde_json::to_string_pretty(&state)?;
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
            // The final check, backup rotation and swap run under a short
            // exclusive lock on `state.cas`, so two savers cannot both pass
            // the check. The OS drops it with the process: nothing to clean up.
            let cas = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join("state.cas"))
                .map_err(|e| write_err(&dir.join("state.cas"), e))?;
            cas.lock()
                .map_err(|e| write_err(&dir.join("state.cas"), e))?;
            let (again, still_readable) = self.check_serial()?;
            if again != base || still_readable != readable {
                return Err(Self::conflict(&path));
            }
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
            let _ = writeln!(
                std::io::stderr(),
                "warning: state saved but could not sync {}: {e}",
                dir.display()
            );
        }
        self.set_seen(Some(next));
        Ok(())
    }

    /// The serial now on disk, provided it is the one this handle last read or
    /// wrote; otherwise the compare-and-swap conflict. Also reports whether the
    /// file was readable. A missing file is serial 0, and an unreadable (corrupt)
    /// one is treated as the handle's own serial so it can be recovered by
    /// saving over it. A file of an unsupported (newer) version is always
    /// refused.
    fn check_serial(&self) -> Result<(u64, bool), CliError> {
        let path = self.state_file();
        let on_disk = match fs::read_to_string(&path) {
            Ok(text) => match parse_state(&text) {
                Ok(s) => Some(s.serial),
                Err(StateParseError::Unsupported(msg)) => {
                    return Err(CliError::Message(format!(
                        "refusing to overwrite {}: it {msg}",
                        path.display()
                    )));
                }
                Err(StateParseError::Corrupt(_)) => None,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(0),
            Err(e) => return Err(CliError::io(format!("cannot read {}", path.display()), e)),
        };
        let readable = on_disk.is_some();
        match (self.seen(), on_disk) {
            (Some(seen), Some(disk)) if seen != disk => Err(Self::conflict(&path)),
            (Some(seen), _) => Ok((seen, readable)),
            // Never read: only a fresh project (no state yet) may write.
            (None, Some(0) | None) => Ok((0, readable)),
            (None, Some(_)) => Err(Self::conflict(&path)),
        }
    }

    fn conflict(path: &Path) -> CliError {
        CliError::StateLocked(format!(
            "{} changed since it was read (another amicu process wrote it); \
             nothing was overwritten. Re-run the command to start from the current state",
            path.display()
        ))
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
    let corrupt = |e: serde_json::Error| StateParseError::Corrupt(e.to_string());
    let mut value: serde_json::Value = serde_json::from_str(text).map_err(corrupt)?;
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    if let Some(version) = version {
        if !LocalState::SUPPORTED.contains(&version.as_str()) {
            return Err(StateParseError::Unsupported(format!(
                "has state version '{version}', which this amicu does not understand \
                 (supported: {}); upgrade amicu rather than overwrite it",
                LocalState::SUPPORTED.join(", ")
            )));
        }
        if let Some(map) = value.as_object_mut() {
            migrate(version, map).map_err(StateParseError::Unsupported)?;
        }
    }
    // Endpoint handles are a shared API type that cannot carry a catch-all, so
    // an unknown field there is refused rather than silently dropped on save.
    if let Some(endpoints) = value.get("endpoints").and_then(|e| e.as_array()) {
        for key in endpoints
            .iter()
            .filter_map(|e| e.as_object())
            .flat_map(serde_json::Map::keys)
        {
            if !ENDPOINT_FIELDS.contains(&key.as_str()) {
                return Err(StateParseError::Unsupported(format!(
                    "has an endpoint field '{key}' this amicu does not understand; \
                     upgrade amicu rather than overwrite it"
                )));
            }
        }
    }
    serde_json::from_value(value).map_err(corrupt)
}

/// The fields of a persisted endpoint handle ([`RuntimeHandle`]).
const ENDPOINT_FIELDS: &[&str] = &["id", "deployment", "endpoint", "exposure"];

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
fn collect_hcl(
    dir: &Path,
    files: &mut Vec<PathBuf>,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), CliError> {
    let entries =
        fs::read_dir(dir).map_err(|e| CliError::io(format!("cannot read {}", dir.display()), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| CliError::io("cannot read directory entry", e))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| CliError::io("cannot read directory entry", e))?;
        // Symlinked directories are never followed: a link back up the tree
        // would recurse forever, and a link out is not part of this project.
        if file_type.is_dir() {
            if is_excluded_dir(&path) {
                continue;
            }
            if is_nested_amicu_project(&path) {
                skipped.push(path);
            } else if !is_foreign_project(&path) {
                collect_hcl(&path, files, skipped)?;
            }
        } else if path.is_file() && is_config_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

/// A config file: `*.hcl`, but not a tool lock file such as
/// `.terraform.lock.hcl` (`*.lock.hcl`).
fn is_config_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    Path::new(&lower).extension().is_some_and(|e| e == "hcl") && !lower.ends_with(".lock.hcl")
}

/// Whether a nested directory is a different amicu project: `.amicu/` holding
/// what `amicu init` or a real apply wrote. A bare `.amicu/` is not enough: the
/// state lock creates `.amicu/state/` in any directory amicu is run from.
fn is_nested_amicu_project(dir: &Path) -> bool {
    dir.join(".amicu/config.yaml").is_file() || dir.join(".amicu/state/state.json").is_file()
}

/// Whether a nested directory is the root of a Terraform project, whose files
/// are not ours (`.terraform/` or `.terraform.lock.hcl`).
fn is_foreign_project(dir: &Path) -> bool {
    [".terraform", ".terraform.lock.hcl"]
        .iter()
        .any(|m| dir.join(m).exists())
}

/// Directories skipped during discovery: state, hidden dirs, and build output.
fn is_excluded_dir(path: &Path) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.starts_with('.') || name == "target" || name == "node_modules",
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
            serial: 0,
            purge_pending: Vec::new(),
            extra: std::collections::BTreeMap::default(),
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

    fn migrated(from: &str, mut v: serde_json::Value) -> serde_json::Value {
        let map = v.as_object_mut().unwrap();
        let step = MIGRATIONS.iter().find(|m| m.from == from).unwrap();
        (step.apply)(map);
        v
    }

    #[test]
    fn migration_chain_is_contiguous_and_ends_at_current_version() {
        let mut at = MIGRATIONS[0].from;
        assert_eq!(LocalState::SUPPORTED[0], at);
        for (m, supported) in MIGRATIONS.iter().zip(LocalState::SUPPORTED) {
            assert_eq!(m.from, at);
            assert_eq!(*supported, at);
            at = m.to;
        }
        assert_eq!(at, LocalState::VERSION);
        assert_eq!(LocalState::SUPPORTED.last(), Some(&LocalState::VERSION));
        assert_eq!(LocalState::SUPPORTED.len(), MIGRATIONS.len() + 1);
    }

    #[test]
    fn v1_to_v2_adds_pending_and_stops_without_touching_existing_values() {
        let v1 = serde_json::json!({"version": "v1", "graph": {}});
        let out = migrated("v1", v1);
        assert_eq!(out["pending"], serde_json::json!([]));
        assert_eq!(out["stops"], serde_json::json!([]));
        let kept = serde_json::json!({"pending": ["a"], "stops": []});
        assert_eq!(migrated("v1", kept)["pending"], serde_json::json!(["a"]));
    }

    #[test]
    fn v2_to_v3_leaves_the_document_unchanged() {
        let v2 = serde_json::json!({"version": "v2", "pending": [], "stops": []});
        assert_eq!(migrated("v2", v2.clone()), v2);
    }

    #[test]
    fn old_versions_load_and_are_stamped_current() {
        let root = scratch("migrate");
        let p = Project::new(&root);
        p.save_state(&state("g")).unwrap();
        let base: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.state_file()).unwrap()).unwrap();
        for old in ["v1", "v2"] {
            let mut v = base.clone();
            v["version"] = old.into();
            v.as_object_mut().unwrap().remove("pending");
            v.as_object_mut().unwrap().remove("stops");
            fs::write(p.state_file(), v.to_string()).unwrap();
            let loaded = p.load_state().unwrap().unwrap();
            // v2 predates neither field being optional: both default to empty.
            assert_eq!(loaded.version, LocalState::VERSION, "{old}");
            assert!(loaded.pending.is_empty() && loaded.stops.is_empty());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn newer_version_is_refused_even_when_its_shape_differs() {
        let text = r#"{"version": "v5", "graph": "not a graph any more"}"#;
        assert!(matches!(
            parse_state(text),
            Err(StateParseError::Unsupported(m)) if m.contains("v5") && m.contains("upgrade amicu")
        ));
    }

    #[test]
    fn unknown_top_level_fields_survive_a_load_save_cycle() {
        let root = scratch("unknown");
        let p = Project::new(&root);
        p.save_state(&state("g")).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.state_file()).unwrap()).unwrap();
        v["from_the_future"] = serde_json::json!({"k": [1, 2]});
        fs::write(p.state_file(), v.to_string()).unwrap();
        let loaded = p.load_state().unwrap().unwrap();
        p.save_state(&loaded).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.state_file()).unwrap()).unwrap();
        assert_eq!(saved["from_the_future"], serde_json::json!({"k": [1, 2]}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_stop_record_fields_survive_and_unknown_endpoint_fields_are_refused() {
        let root = scratch("nested");
        let p = Project::new(&root);
        let mut s = state("g");
        s.stops = vec![crate::stops::StopRecord::host(&"d".into(), None)];
        s.endpoints = vec![RuntimeHandle {
            id: "e".into(),
            deployment: "d".into(),
            endpoint: "http://127.0.0.1:1".into(),
            exposure: amicu_runtime::EndpointExposure::Loopback,
        }];
        p.save_state(&s).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.state_file()).unwrap()).unwrap();
        v["stops"][0]["from_the_future"] = serde_json::json!(7);
        fs::write(p.state_file(), v.to_string()).unwrap();
        let loaded = p.load_state().unwrap().unwrap();
        p.save_state(&loaded).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.state_file()).unwrap()).unwrap();
        assert_eq!(saved["stops"][0]["from_the_future"], 7);

        v["endpoints"][0]["from_the_future"] = serde_json::json!(true);
        fs::write(p.state_file(), v.to_string()).unwrap();
        let err = p.load_state().expect_err("unknown endpoint field");
        assert!(err.to_string().contains("from_the_future"), "{err}");
        assert!(err.to_string().contains("upgrade amicu"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn serial_counts_saves_and_survives_a_reload() {
        let root = scratch("serial");
        let p = Project::new(&root);
        assert!(p.load_state().unwrap().is_none());
        p.save_state(&state("a")).unwrap();
        p.save_state(&state("b")).unwrap();
        assert_eq!(p.load_state().unwrap().unwrap().serial, 2);
        // A fresh handle continues from what it reads.
        let q = Project::new(&root);
        let loaded = q.load_state().unwrap().unwrap();
        q.save_state(&loaded).unwrap();
        assert_eq!(q.load_state().unwrap().unwrap().serial, 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stale_writer_is_refused_and_the_winner_is_untouched() {
        let root = scratch("cas");
        let a = Project::new(&root);
        a.save_state(&state("base")).unwrap();
        let b = Project::new(&root);
        let b_view = b.load_state().unwrap().unwrap();
        // A writes after B read: B's save must fail, not overwrite.
        a.save_state(&state("a-wins")).unwrap();
        let err = b.save_state(&b_view).expect_err("stale write");
        assert!(
            err.to_string().contains("changed since it was read"),
            "{err}"
        );
        let on_disk = a.load_state().unwrap().unwrap();
        assert_eq!(on_disk.pending, vec!["a-wins".to_owned().into()]);
        // Re-reading is the way forward.
        let fresh = b.load_state().unwrap().unwrap();
        b.save_state(&fresh).unwrap();
        // A handle that never read existing state may not write over it.
        let c = Project::new(&root);
        assert!(c.save_state(&state("blind")).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    /// Eight writers each add their own tag to `pending` by read-modify-write,
    /// retrying from a fresh read when refused. Without the compare-and-swap,
    /// concurrent writers lose each other's tags (and the serial undercounts).
    #[test]
    fn concurrent_writers_never_overwrite_each_other() {
        const WRITERS: usize = 8;
        let root = scratch("race");
        Project::new(&root).save_state(&state("seed")).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(WRITERS));
        let threads: Vec<_> = (0..WRITERS)
            .map(|i| {
                let (root, barrier) = (root.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let p = Project::new(&root);
                    barrier.wait();
                    let mut refused = 0usize;
                    loop {
                        let mut s = p.load_state().unwrap().unwrap();
                        s.pending.push(format!("w{i}").into());
                        match p.save_state(&s) {
                            Ok(()) => return refused,
                            Err(e) => {
                                assert!(e.to_string().contains("changed since"), "{e}");
                                refused += 1;
                            }
                        }
                    }
                })
            })
            .collect();
        let refused: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        let last = Project::new(&root).load_state().unwrap().unwrap();
        let mut tags: Vec<String> = last.pending.iter().map(|d| d.as_str().to_owned()).collect();
        tags.sort();
        let mut want: Vec<String> = (0..WRITERS).map(|i| format!("w{i}")).collect();
        want.push("seed".into());
        want.sort();
        assert_eq!(
            tags, want,
            "a writer's update was lost ({refused} refusals)"
        );
        assert_eq!(last.serial, 1 + WRITERS as u64);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_max_serial_is_refused_not_wrapped() {
        let root = scratch("max");
        let p = Project::new(&root);
        p.save_state(&state("x")).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.state_file()).unwrap()).unwrap();
        v["serial"] = u64::MAX.into();
        fs::write(p.state_file(), v.to_string()).unwrap();
        let q = Project::new(&root);
        let loaded = q.load_state().unwrap().unwrap();
        let err = q.save_state(&loaded).expect_err("overflow");
        assert!(err.to_string().contains("maximum state serial"), "{err}");
        assert_eq!(
            Project::new(&root).load_state().unwrap().unwrap().serial,
            u64::MAX
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_newer_version_file_is_refused_by_the_cas_check() {
        let root = scratch("newer-cas");
        let p = Project::new(&root);
        p.save_state(&state("x")).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.state_file()).unwrap()).unwrap();
        v["version"] = "v9".into();
        let newer = v.to_string();
        fs::write(p.state_file(), &newer).unwrap();
        let err = p.check_serial().expect_err("newer version");
        assert!(err.to_string().contains("v9"), "{err}");
        assert!(p.save_state(&state("y")).is_err());
        assert_eq!(fs::read_to_string(p.state_file()).unwrap(), newer);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn v3_state_migrates_to_serial_zero() {
        let root = scratch("v3");
        let p = Project::new(&root);
        p.save_state(&state("x")).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.state_file()).unwrap()).unwrap();
        v["version"] = "v3".into();
        v.as_object_mut().unwrap().remove("serial");
        fs::write(p.state_file(), v.to_string()).unwrap();
        let q = Project::new(&root);
        let loaded = q.load_state().unwrap().unwrap();
        assert_eq!((loaded.version.as_str(), loaded.serial), ("v4", 0));
        q.save_state(&loaded).unwrap();
        assert_eq!(q.load_state().unwrap().unwrap().serial, 1);
        let _ = fs::remove_dir_all(&root);
    }
}
