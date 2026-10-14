//! Stop records: what amicu needs to stop an instance that may be running.
//!
//! Each record is self-describing (backend + deterministic identity + the
//! provider's own id once known + who it was started as), so a deployment can
//! be stopped from state alone even when its endpoint handle is missing, its
//! start never finished, or its provider changed in the config. A record is
//! removed only once the stop is confirmed.

use amicu_core::id::DeploymentId;
use amicu_llamacpp::{HostBackend, HostContext};
use amicu_modal::supervisor::app_name_for;
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
    /// A Modal app.
    Modal,
}

impl Backend {
    /// The host backend of the machine amicu is running on.
    pub(crate) fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::HostLaunchd
        } else {
            Self::HostSystemd
        }
    }

    pub(crate) fn from_host(backend: HostBackend) -> Self {
        match backend {
            HostBackend::Launchd => Self::HostLaunchd,
            HostBackend::Systemd => Self::HostSystemd,
        }
    }

    pub(crate) fn is_modal(self) -> bool {
        self == Self::Modal
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::HostLaunchd => "launchd",
            Self::HostSystemd => "systemd",
            Self::Modal => "modal",
        }
    }
}

/// How to stop one instance of a deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StopRecord {
    /// The deployment this instance serves.
    pub deployment: DeploymentId,
    pub backend: Backend,
    /// Host: the launchd label / systemd unit. Modal: the app name.
    pub identity: String,
    /// Modal only: the app id once known (from the deploy). Stopping by id is
    /// preferred over a name lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Host only: the effective uid the instance was started as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// launchd only: the domain (`gui/<uid>` or `user/<uid>`) it was started in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Modal only: the workspace the app was deployed in.
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
}

impl StopRecord {
    /// A host instance, started as `context` (None: not known, e.g. state
    /// that predates these fields; the current user is assumed on stop).
    pub(crate) fn host(deployment: &DeploymentId, context: Option<&HostContext>) -> Self {
        Self {
            deployment: deployment.clone(),
            backend: context.map_or_else(Backend::host, |c| Backend::from_host(c.backend)),
            identity: amicu_llamacpp::host_label(deployment.as_str()),
            app_id: None,
            uid: context.map(|c| c.uid),
            domain: context.and_then(|c| c.domain.clone()),
            workspace: None,
            environment: None,
            definition: None,
            env_file: None,
            logs: Vec::new(),
            volumes: Vec::new(),
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
            domain: None,
            workspace: workspace.map(|w| w.name.clone()),
            environment: workspace.map(|w| w.environment.clone()),
            definition: None,
            env_file: None,
            logs: Vec::new(),
            volumes: Vec::new(),
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
    /// `<identity>.env` in records written when units were transient), in the
    /// directory amicu writes it to (`.../Library/LaunchAgents`, the systemd
    /// user unit dir `.../systemd/user`, or the legacy env dir `.../amicu` /
    /// `.../amicu/env`). `Ok(None)`: none
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
            Backend::Modal => return Err(format!("a Modal record has a service file '{def}'")),
        };
        Self::check_file(&path, &format!("{}.{ext}", self.identity), dirs)?;
        Ok(Some(path))
    }

    /// The recorded systemd secrets file, validated: absolute, no `..`, named
    /// exactly `<identity>.env`, in `.../.local/state/amicu/env`. `Ok(None)`:
    /// none recorded (the manager then derives it).
    pub(crate) fn env_path(&self) -> Result<Option<PathBuf>, String> {
        let Some(env) = &self.env_file else {
            return Ok(None);
        };
        if self.backend != Backend::HostSystemd {
            return Err(format!(
                "a {} record has an env file '{env}'",
                self.backend.name()
            ));
        }
        let path = PathBuf::from(env);
        Self::check_file(
            &path,
            &format!("{}.env", self.identity),
            &[".local/state/amicu/env"],
        )?;
        Ok(Some(path))
    }

    /// The recorded log files, validated like [`Self::definition_path`]:
    /// `<identity>.out.log` / `<identity>.err.log` in `.../Library/Logs/amicu`.
    pub(crate) fn log_paths(&self) -> Result<Vec<PathBuf>, String> {
        let mut out = Vec::new();
        for l in &self.logs {
            let path = PathBuf::from(l);
            let ok = ["out", "err"].iter().any(|k| {
                path.file_name().and_then(|n| n.to_str())
                    == Some(&format!("{}.{k}.log", self.identity))
            });
            if self.backend != Backend::HostLaunchd || !ok {
                return Err(format!("'{l}' is not a log file of {}", self.identity));
            }
            Self::check_file(
                &path,
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default(),
                &["Library/Logs/amicu"],
            )?;
            out.push(path);
        }
        Ok(out)
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
            _ => format!(
                "{}: {} job '{}'",
                self.deployment,
                self.backend.name(),
                self.identity
            ),
        }
    }
}
