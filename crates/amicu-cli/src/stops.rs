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
    /// Host only: the service definition file (launchd plist / systemd env
    /// file) recorded before the start. It holds the injected environment
    /// (the API key), so it is deleted with the stop, from this path rather
    /// than one recomputed from `$HOME`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// Host only: log files the service writes; removed by `destroy --purge`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
    /// Modal only: the shared cache Volumes the app mounts; deleted by
    /// `destroy --purge` once no Modal deployment remains.
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
