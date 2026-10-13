//! Stop records: what amicu needs to stop an instance that may be running.
//!
//! Each record is self-describing (backend + deterministic identity + the
//! provider's own id once known), so a deployment can be stopped from state
//! alone even when its endpoint handle is missing, its start never finished,
//! or its provider changed in the config. A record is removed only once the
//! stop is confirmed.

use amicu_core::id::DeploymentId;
use amicu_modal::supervisor::app_name_for;
use serde::{Deserialize, Serialize};

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

    pub(crate) fn is_modal(self) -> bool {
        self == Self::Modal
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
}

impl StopRecord {
    pub(crate) fn host(deployment: &DeploymentId) -> Self {
        Self {
            deployment: deployment.clone(),
            backend: Backend::host(),
            identity: amicu_llamacpp::host_label(deployment.as_str()),
            app_id: None,
        }
    }

    pub(crate) fn modal(deployment: &DeploymentId, app_id: Option<String>) -> Self {
        Self {
            deployment: deployment.clone(),
            backend: Backend::Modal,
            identity: app_name_for(deployment.as_str()),
            app_id,
        }
    }

    /// Whether `other` addresses the same running thing (same kind of backend
    /// and identity), so starting `other` replaces this instance in place.
    pub(crate) fn same_target(&self, other: &Self) -> bool {
        self.backend.is_modal() == other.backend.is_modal() && self.identity == other.identity
    }
}
