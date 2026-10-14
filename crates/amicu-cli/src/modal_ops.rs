//! The Modal operations apply/destroy/refresh need, behind a trait so tests
//! never touch the network, plus the workspace identity checks.

use amicu_modal::{Credentials, ModalClient, ModalError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::CliError;
use crate::refresh::{AppLookup, ModalLookup};

/// The environment requests are made in. amicu's Modal calls never name one,
/// so they use the token's default environment.
pub(crate) const DEFAULT_ENVIRONMENT: &str = "default";

/// Which Modal workspace (and environment) a token acts in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Workspace {
    pub name: String,
    pub environment: String,
}

impl std::fmt::Display for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "workspace '{}' (environment '{}')",
            self.name, self.environment
        )
    }
}

/// The Modal calls amicu makes outside of deploying.
#[async_trait]
pub(crate) trait ModalOps: Send + Sync {
    /// The workspace the configured token belongs to.
    async fn workspace(&self) -> Result<Workspace, CliError>;
    /// The app id published under `name`, if any.
    async fn find_app(&self, name: &str) -> Result<Option<String>, CliError>;
    /// Stop an app by id. `Ok(false)`: Modal says no such app exists.
    async fn stop_app(&self, app_id: &str) -> Result<bool, CliError>;
}

/// Everything the commands need from Modal: lookups for refresh and the
/// stop/identity operations.
pub(crate) trait ModalBackend: ModalOps + AppLookup {}
impl<T: ModalOps + AppLookup> ModalBackend for T {}

async fn connect() -> Result<ModalClient, CliError> {
    let credentials = Credentials::from_env().map_err(ModalError::from)?;
    Ok(ModalClient::connect(credentials).await?)
}

#[async_trait]
impl ModalOps for ModalLookup {
    async fn workspace(&self) -> Result<Workspace, CliError> {
        let mut client = connect().await?;
        Ok(Workspace {
            name: client.workspace_name().await?,
            environment: DEFAULT_ENVIRONMENT.to_owned(),
        })
    }

    async fn find_app(&self, name: &str) -> Result<Option<String>, CliError> {
        Ok(connect().await?.find_app(name).await?)
    }

    async fn stop_app(&self, app_id: &str) -> Result<bool, CliError> {
        match connect().await?.stop_app(app_id).await {
            Ok(()) => Ok(true),
            Err(e) if e.is_not_found() => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// The refusal for acting on `deployment`'s instance with a token from a
/// different workspace (or environment) than the one it was created in.
pub(crate) fn workspace_mismatch(
    deployment: &str,
    recorded: &Workspace,
    active: &Workspace,
) -> CliError {
    CliError::Message(format!(
        "deployment '{deployment}' lives in Modal {recorded}, but the active token is for \
         {active}. Switch MODAL_TOKEN_ID and MODAL_TOKEN_SECRET (and MODAL_ENVIRONMENT) to \
         {recorded} and retry; nothing was changed"
    ))
}

/// `Err` when a record's workspace is known and differs from `active`.
/// Records without one (state that predates it) are accepted.
pub(crate) fn check_workspace(
    deployment: &str,
    record_workspace: Option<&str>,
    record_environment: Option<&str>,
    active: &Workspace,
) -> Result<(), CliError> {
    let Some(name) = record_workspace else {
        return Ok(());
    };
    let recorded = Workspace {
        name: name.to_owned(),
        environment: record_environment.unwrap_or(DEFAULT_ENVIRONMENT).to_owned(),
    };
    if &recorded == active {
        Ok(())
    } else {
        Err(workspace_mismatch(deployment, &recorded, active))
    }
}
