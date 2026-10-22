//! Stop records: what amicu needs to stop an instance that may be running.
//!
//! Each record is self-describing (backend + deterministic identity + the
//! provider's own id once known + who it was started as), so a deployment can
//! be stopped from state alone even when its endpoint handle is missing, its
//! start never finished, or its provider changed in the config. A record is
//! removed only once the stop is confirmed.

use amicu_core::id::DeploymentId;
use amicu_llamacpp::{HostBackend, HostContext};
use amicu_modal::supervisor::{app_name_for, project_app_name};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::modal_ops::Workspace;

/// Which backend runs the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Backend {
    /// A launchd job (macOS host).
    HostLaunchd,
    /// A systemd user unit (Linux host).
    HostSystemd,
    /// A per-user Task Scheduler task (Windows host).
    HostTaskScheduler,
    /// A Modal app.
    Modal,
    /// A RunPod Serverless endpoint.
    Runpod,
    /// An always-on RunPod Pod.
    RunpodPod,
    /// A Google Compute Engine VM and its firewall rule.
    Gce,
}

impl Backend {
    /// The host backend of the machine amicu is running on.
    pub(crate) fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::HostLaunchd
        } else if cfg!(windows) {
            Self::HostTaskScheduler
        } else {
            Self::HostSystemd
        }
    }

    pub(crate) fn from_host(backend: HostBackend) -> Self {
        match backend {
            HostBackend::Launchd => Self::HostLaunchd,
            HostBackend::Systemd => Self::HostSystemd,
            HostBackend::TaskScheduler => Self::HostTaskScheduler,
        }
    }

    pub(crate) fn is_modal(self) -> bool {
        self == Self::Modal
    }

    pub(crate) fn is_host(self) -> bool {
        matches!(
            self,
            Self::HostLaunchd | Self::HostSystemd | Self::HostTaskScheduler
        )
    }

    pub(crate) fn is_runpod(self) -> bool {
        matches!(self, Self::Runpod | Self::RunpodPod)
    }

    pub(crate) fn is_gce(self) -> bool {
        self == Self::Gce
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::HostLaunchd => "launchd",
            Self::HostSystemd => "systemd",
            Self::HostTaskScheduler => "task-scheduler",
            Self::Modal => "modal",
            Self::Runpod => "runpod",
            Self::RunpodPod => "runpod-pod",
            Self::Gce => "gce",
        }
    }
}

/// How to stop one instance of a deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StopRecord {
    /// The deployment this instance serves.
    pub deployment: DeploymentId,
    pub backend: Backend,
    /// Host: the launchd label / systemd unit. Modal: the app name. RunPod:
    /// the endpoint or pod name.
    pub identity: String,
    /// Modal: the app id; RunPod: the endpoint or pod id. Set once known; stopping
    /// by id is preferred over a name lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Host only: the effective uid the instance was started as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// Task Scheduler only: the SID of the user the task runs as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// launchd only: the domain (`gui/<uid>` or `user/<uid>`) it was started in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Modal: the workspace the app was deployed in. RunPod: the account
    /// fingerprint the endpoint was created in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Modal only: the environment the app was deployed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Host only: the service definition file (launchd plist / systemd unit
    /// file) recorded before the start. It is deleted with the stop, from this path rather
    /// than one recomputed from `$HOME`, and the systemd unit's secrets file goes with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// systemd only: the 0600 environment file (secrets) the unit reads,
    /// recorded before the start with the unit path and deleted before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,
    /// Host only: log files the service writes; removed by `destroy --purge`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
    /// Modal only: the shared cache Volumes the app mounts. Recorded so
    /// destroy can list them; never deleted by amicu (other projects share them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    /// RunPod only: every account secret that may exist for the endpoint or pod,
    /// recorded before it is written and dropped only once deleted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Compute Engine only: the project, zones, instance, firewall rule and
    /// owner marker. Written before the VM is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gcp: Option<amicu_gcp::GceRecord>,
    /// Fields this amicu does not know (from a newer build of the same
    /// version), kept verbatim so a load-save cycle never drops them.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

impl StopRecord {
    /// What a RunPod record names: `"pod"` for a Pod, else `"endpoint"`.
    pub(crate) fn runpod_kind(&self) -> &'static str {
        if self.backend == Backend::RunpodPod {
            "pod"
        } else {
            "endpoint"
        }
    }

    /// A host instance, started as `context` (None: not known, e.g. state
    /// that predates these fields; the current user is assumed on stop).
    pub(crate) fn host(deployment: &DeploymentId, context: Option<&HostContext>) -> Self {
        Self {
            deployment: deployment.clone(),
            backend: context.map_or_else(Backend::host, |c| Backend::from_host(c.backend)),
            identity: amicu_llamacpp::host_label(deployment.as_str()),
            app_id: None,
            uid: context.and_then(|c| c.uid),
            sid: context.and_then(|c| c.sid.clone()),
            domain: context.and_then(|c| c.domain.clone()),
            workspace: None,
            environment: None,
            definition: None,
            env_file: None,
            logs: Vec::new(),
            volumes: Vec::new(),
            secrets: Vec::new(),
            gcp: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    /// A host instance of the project `project_id`: its label is scoped to
    /// the project, and the files it will leave on the host are recorded.
    pub(crate) fn host_in(
        project_id: &str,
        deployment: &DeploymentId,
        context: Option<&HostContext>,
        paths: Option<amicu_llamacpp::ServicePaths>,
    ) -> Self {
        let mut rec = Self::host(deployment, context);
        rec.identity = amicu_llamacpp::project_host_label(project_id, deployment.as_str());
        if let Some(p) = paths {
            rec.definition = Some(p.definition.to_string_lossy().into_owned());
            rec.env_file = p.env.map(|e| e.to_string_lossy().into_owned());
            rec.logs = p
                .logs
                .iter()
                .map(|l| l.to_string_lossy().into_owned())
                .collect();
        }
        rec
    }

    /// Record the Volumes a Modal app mounts.
    #[must_use]
    pub(crate) fn with_volumes(mut self, volumes: &[&str]) -> Self {
        self.volumes = volumes.iter().map(|v| (*v).to_owned()).collect();
        self
    }

    /// A Modal app of the project `project_id`: its name is scoped to the
    /// project and exact per deployment, see [`project_app_name`].
    pub(crate) fn modal_in(
        project_id: &str,
        deployment: &DeploymentId,
        app_id: Option<String>,
        workspace: Option<&Workspace>,
    ) -> Self {
        let mut rec = Self::modal(deployment, app_id, workspace);
        rec.identity = project_app_name(project_id, deployment.as_str());
        rec
    }

    /// A Modal app under the unscoped legacy name. Only for state written
    /// before names were project-scoped: such a record keeps its recorded name.
    pub(crate) fn modal(
        deployment: &DeploymentId,
        app_id: Option<String>,
        workspace: Option<&Workspace>,
    ) -> Self {
        Self {
            deployment: deployment.clone(),
            backend: Backend::Modal,
            identity: app_name_for(deployment.as_str()),
            app_id,
            uid: None,
            sid: None,
            domain: None,
            workspace: workspace.map(|w| w.name.clone()),
            environment: workspace.map(|w| w.environment.clone()),
            definition: None,
            env_file: None,
            logs: Vec::new(),
            volumes: Vec::new(),
            secrets: Vec::new(),
            gcp: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    /// A RunPod endpoint of the project `project_id`, in the account
    /// `account` (an [`amicu_runpod::AccountIdentity`] fingerprint).
    pub(crate) fn runpod_in(
        project_id: &str,
        deployment: &DeploymentId,
        endpoint_id: Option<String>,
        account: Option<&str>,
        secrets: Vec<String>,
    ) -> Self {
        Self {
            deployment: deployment.clone(),
            backend: Backend::Runpod,
            identity: amicu_runpod::project_endpoint_name(project_id, deployment.as_str()),
            app_id: endpoint_id,
            uid: None,
            sid: None,
            domain: None,
            workspace: account.map(str::to_owned),
            environment: None,
            definition: None,
            env_file: None,
            logs: Vec::new(),
            volumes: Vec::new(),
            secrets,
            gcp: None,
            extra: std::collections::BTreeMap::new(),
        }
    }

    /// [`Self::runpod_in`] for an always-on pod.
    pub(crate) fn runpod_pod_in(
        project_id: &str,
        deployment: &DeploymentId,
        pod_id: Option<String>,
        account: Option<&str>,
        secrets: Vec<String>,
    ) -> Self {
        Self {
            backend: Backend::RunpodPod,
            ..Self::runpod_in(project_id, deployment, pod_id, account, secrets)
        }
    }

    /// A Compute Engine VM: `record` is persisted before the first create.
    pub(crate) fn gce(deployment: &DeploymentId, record: amicu_gcp::GceRecord) -> Self {
        Self {
            backend: Backend::Gce,
            identity: record.instance.clone(),
            gcp: Some(record),
            ..Self::modal(deployment, None, None)
        }
    }

    /// A Compute Engine record derived without one in state (state written
    /// before it existed): it names nothing, and stopping it explains why.
    pub(crate) fn gce_unrecorded(deployment: &DeploymentId) -> Self {
        Self {
            backend: Backend::Gce,
            identity: format!("gce-{deployment}"),
            ..Self::modal(deployment, None, None)
        }
    }

    /// Whether `other` addresses the same running thing: the same backend
    /// (launchd and systemd are never interchangeable) and identity.
    pub(crate) fn same_target(&self, other: &Self) -> bool {
        self.backend == other.backend && self.identity == other.identity
    }

    /// Whether `other` is the very same recorded instance (ignoring the
    /// workspace stamp, which is filled in after the fact).
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        self.deployment == other.deployment
            && self.backend == other.backend
            && self.identity == other.identity
            && self.app_id == other.app_id
    }

    /// The recorded service definition, validated: absolute, no `..`, named
    /// exactly `<identity>.plist` (launchd) / `<identity>.service` (systemd; or
    /// `<identity>.env` in records written when units were transient) /
    /// `<identity>.xml` (Task Scheduler), in the directory amicu writes it to
    /// (`.../Library/LaunchAgents`, the systemd user unit dir
    /// `.../systemd/user`, the legacy env dir `.../amicu` / `.../amicu/env`, or
    /// `...\amicu\tasks`). `Ok(None)`: none
    /// recorded (a legacy record). Anything else is refused, so a tampered
    /// state file can never make amicu delete an unrelated file.
    pub(crate) fn definition_path(&self) -> Result<Option<PathBuf>, String> {
        let Some(def) = &self.definition else {
            return Ok(None);
        };
        let path = PathBuf::from(def);
        let (ext, dirs): (&str, &[&str]) = match self.backend {
            Backend::HostLaunchd => ("plist", &["Library/LaunchAgents"]),
            // Persistent user unit; legacy (transient-era) records name the
            // unit's env file instead.
            Backend::HostSystemd if path.extension().is_some_and(|e| e == "env") => {
                ("env", &["amicu", "amicu/env"])
            }
            Backend::HostSystemd => ("service", &[".config/systemd/user"]),
            Backend::HostTaskScheduler => ("xml", &["amicu/tasks"]),
            Backend::Modal | Backend::Runpod | Backend::RunpodPod | Backend::Gce => {
                return Err(format!(
                    "a {} record has a service file '{def}'",
                    self.backend.name()
                ));
            }
        };
        Self::check_file(&path, &format!("{}.{ext}", self.identity), dirs)?;
        if self.backend == Backend::HostTaskScheduler {
            check_anchored(&path, task_scheduler_base().as_deref(), "tasks")?;
        }
        Ok(Some(path))
    }

    /// The recorded secrets file, validated: absolute, no `..`, named exactly
    /// `<identity>.env`, in `.../.local/state/amicu/env` (systemd) or
    /// `...\amicu\env` (Task Scheduler). `Ok(None)`: none recorded (the
    /// manager then derives it).
    pub(crate) fn env_path(&self) -> Result<Option<PathBuf>, String> {
        let Some(env) = &self.env_file else {
            return Ok(None);
        };
        let dirs: &[&str] = match self.backend {
            Backend::HostSystemd => &[".local/state/amicu/env"],
            Backend::HostTaskScheduler => &["amicu/env"],
            _ => {
                return Err(format!(
                    "a {} record has an env file '{env}'",
                    self.backend.name()
                ));
            }
        };
        let path = PathBuf::from(env);
        Self::check_file(&path, &format!("{}.env", self.identity), dirs)?;
        if self.backend == Backend::HostTaskScheduler {
            check_anchored(&path, task_scheduler_base().as_deref(), "env")?;
        }
        Ok(Some(path))
    }

    /// The recorded log files, validated like [`Self::definition_path`]:
    /// `<identity>.out.log` / `<identity>.err.log` in `.../Library/Logs/amicu`
    /// (launchd) or `...\amicu\logs` (Task Scheduler).
    pub(crate) fn log_paths(&self) -> Result<Vec<PathBuf>, String> {
        checked_log_paths(self.backend, &self.identity, &self.logs)
    }

    /// The log files this record left behind, for a purge-pending entry.
    pub(crate) fn purge_entry(&self) -> Option<PurgePending> {
        (!self.logs.is_empty()).then(|| PurgePending {
            deployment: self.deployment.clone(),
            backend: self.backend,
            identity: self.identity.clone(),
            logs: self.logs.clone(),
        })
    }

    fn check_file(path: &Path, name: &str, dirs: &[&str]) -> Result<(), String> {
        use std::path::Component;
        let shown = path.display();
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            return Err(format!("'{shown}' is not a clean absolute path"));
        }
        if path.file_name().and_then(|n| n.to_str()) != Some(name) {
            return Err(format!("'{shown}' is not named '{name}'"));
        }
        let in_dir = path
            .parent()
            .is_some_and(|parent| dirs.iter().any(|d| parent.ends_with(d)));
        if !in_dir {
            return Err(format!(
                "'{shown}' is not in the directory amicu writes it to ({})",
                dirs.join(" or ")
            ));
        }
        Ok(())
    }

    /// A human description naming the exact label / app id, for recovery text.
    pub(crate) fn describe(&self) -> String {
        match self.backend {
            Backend::Modal => format!(
                "{}: Modal app '{}' (app id {})",
                self.deployment,
                self.identity,
                self.app_id.as_deref().unwrap_or("unknown")
            ),
            Backend::Runpod => format!(
                "{}: RunPod endpoint '{}' (endpoint id {})",
                self.deployment,
                self.identity,
                self.app_id.as_deref().unwrap_or("unknown")
            ),
            Backend::RunpodPod => format!(
                "{}: RunPod pod '{}' (pod id {})",
                self.deployment,
                self.identity,
                self.app_id.as_deref().unwrap_or("unknown")
            ),
            Backend::Gce => match &self.gcp {
                Some(g) => format!(
                    "{}: Compute Engine VM '{}' (project {}, zone {})",
                    self.deployment, self.identity, g.project, g.zone
                ),
                None => format!("{}: Compute Engine VM '{}'", self.deployment, self.identity),
            },
            _ => format!(
                "{}: {} job '{}'",
                self.deployment,
                self.backend.name(),
                self.identity
            ),
        }
    }
}

/// Each of `logs` validated as exactly `identity`'s own log file in amicu's
/// log directory (`<identity>.out.log` / `<identity>.err.log`).
fn checked_log_paths(
    backend: Backend,
    identity: &str,
    logs: &[String],
) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for l in logs {
        let path = PathBuf::from(l);
        let ok = ["out", "err"].iter().any(|k| {
            path.file_name().and_then(|n| n.to_str()) == Some(&format!("{identity}.{k}.log"))
        });
        let dir = match backend {
            Backend::HostLaunchd => "Library/Logs/amicu",
            Backend::HostTaskScheduler => "amicu/logs",
            _ => return Err(format!("'{l}' is not a log file of {identity}")),
        };
        if !ok {
            return Err(format!("'{l}' is not a log file of {identity}"));
        }
        StopRecord::check_file(
            &path,
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default(),
            &[dir],
        )?;
        if backend == Backend::HostTaskScheduler {
            check_anchored(&path, task_scheduler_base().as_deref(), "logs")?;
        }
        out.push(path);
    }
    Ok(out)
}

/// Where the Task Scheduler manager keeps its files: `%LOCALAPPDATA%\amicu`.
fn task_scheduler_base() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("amicu"))
}

/// A recorded Task Scheduler file must lie directly in `base\sub`, the
/// directory the manager writes it to, compared case-insensitively and with
/// either separator (as Windows resolves paths): a suffix match alone would
/// accept the same names under another profile, drive or UNC share.
pub(crate) fn check_anchored(path: &Path, base: Option<&Path>, sub: &str) -> Result<(), String> {
    let Some(base) = base else {
        return Err(format!(
            "'{}': LOCALAPPDATA is not set, so amicu cannot tell where its Task Scheduler \
             files belong",
            path.display()
        ));
    };
    let norm = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    let expected = base.join(sub);
    let full = norm(path);
    let parent = full.rsplit_once('\\').map(|(dir, _)| dir);
    if parent == Some(norm(&expected).as_str()) {
        Ok(())
    } else {
        Err(format!(
            "'{}' is not in the directory amicu writes it to ({})",
            path.display(),
            expected.display()
        ))
    }
}

/// Files a destroyed deployment left behind that a plain `destroy` does not
/// remove (its log files). Kept in state until `amicu destroy --purge`
/// removes them, so amicu never loses track of a file it created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PurgePending {
    pub deployment: DeploymentId,
    pub backend: Backend,
    /// The identity (launchd label) the logs are named after.
    pub identity: String,
    pub logs: Vec<String>,
}

impl PurgePending {
    /// The log files, validated like [`StopRecord::log_paths`].
    pub(crate) fn log_paths(&self) -> Result<Vec<PathBuf>, String> {
        checked_log_paths(self.backend, &self.identity, &self.logs)
    }
}
