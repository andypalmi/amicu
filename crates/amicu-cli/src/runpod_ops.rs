//! RunPod access for apply/destroy, behind a trait so tests never touch the
//! network, plus the account identity checks.

use std::sync::Arc;

use amicu_core::secret::SecretValue;
use amicu_providers::{HttpTransport, ReqwestTransport};
use amicu_runpod::{ApiKey, RunPodClient, RunPodError};
use amicu_runtime::exposure::{CheckTiming, EndpointCheck, PROBE_PATH};
use async_trait::async_trait;

use crate::error::CliError;

/// The management key apply and destroy act with.
pub(crate) const API_KEY_ENV_VAR: &str = "RUNPOD_API_KEY";

#[async_trait]
pub(crate) trait RunPodOps: Send + Sync {
    /// A management client for the configured key.
    fn client(&self) -> Result<RunPodClient, CliError>;
    /// Whether `key` is the management key itself.
    fn is_management_key(&self, key: &SecretValue) -> bool;
    /// The transport the readiness probe goes through.
    fn probe(&self) -> Arc<dyn HttpTransport>;
    /// The post-deploy check: an anonymous request is refused and `key` is
    /// accepted. `Err` is secret-free.
    async fn verify_exposure(&self, endpoint: &str, key: &SecretValue) -> Result<(), String>;
}

pub(crate) struct RealRunPod;

#[async_trait]
impl RunPodOps for RealRunPod {
    fn client(&self) -> Result<RunPodClient, CliError> {
        match std::env::var(API_KEY_ENV_VAR) {
            Ok(key) if !key.trim().is_empty() => Ok(RunPodClient::new(ApiKey::new(key))),
            _ => Err(missing_key()),
        }
    }

    fn is_management_key(&self, key: &SecretValue) -> bool {
        std::env::var(API_KEY_ENV_VAR)
            .is_ok_and(|k| !k.trim().is_empty() && k.as_bytes() == key.expose_secret())
    }

    fn probe(&self) -> Arc<dyn HttpTransport> {
        Arc::new(ReqwestTransport::new())
    }

    async fn verify_exposure(&self, endpoint: &str, key: &SecretValue) -> Result<(), String> {
        // The gateway checks the bearer key itself: no extra headers.
        amicu_runtime::exposure::verify_endpoint(&EndpointCheck {
            endpoint,
            path: PROBE_PATH,
            api_key: key,
            gateway_headers: &[],
            timing: CheckTiming::PUBLIC,
        })
        .await
        .map_err(|e| e.detail().to_owned())
    }
}

pub(crate) fn missing_key() -> CliError {
    CliError::Message(format!(
        "RunPod needs {API_KEY_ENV_VAR} set to a RunPod API key that can manage serverless \
         endpoints and secrets; nothing was changed"
    ))
}

pub(crate) fn runpod_error(context: &str, e: &RunPodError) -> CliError {
    let hint = match e {
        RunPodError::CredentialsRejected { .. } => format!(
            "; check that {API_KEY_ENV_VAR} is a valid RunPod API key with read/write access"
        ),
        _ => String::new(),
    };
    CliError::Message(format!("{context}: {e}{hint}"))
}

/// Checks the management key with a cheap call and returns the account
/// fingerprint to record, creating the personal-account marker if needed.
pub(crate) async fn preflight(ops: &dyn RunPodOps) -> Result<String, CliError> {
    let client = ops.client()?;
    client
        .verify_key()
        .await
        .map_err(|e| runpod_error("RunPod preflight failed; nothing was started", &e))?;
    match client.account_identity(true).await {
        Ok(Some(id)) => Ok(id.fingerprint()),
        Ok(None) => Err(CliError::Message(
            "RunPod preflight could not establish the account identity; nothing was started"
                .to_owned(),
        )),
        Err(e) => Err(runpod_error(
            "RunPod preflight could not read the account identity; nothing was started",
            &e,
        )),
    }
}

/// The account `client` acts in, without creating anything. `None`: unknown,
/// which never counts as a match.
pub(crate) async fn current_account(client: &RunPodClient) -> Result<Option<String>, CliError> {
    client
        .account_identity(false)
        .await
        .map(|id| id.map(|i| i.fingerprint()))
        .map_err(|e| runpod_error("could not read the RunPod account identity", &e))
}

/// `Err` unless the record's account is known and equals `active`.
pub(crate) fn check_account(
    deployment: &str,
    recorded: Option<&str>,
    active: Option<&str>,
) -> Result<(), CliError> {
    match (recorded, active) {
        (Some(r), Some(a)) if r == a => Ok(()),
        (None, _) => Err(CliError::Message(format!(
            "deployment '{deployment}' has a RunPod endpoint recorded without its account, so \
             amicu cannot tell whether {API_KEY_ENV_VAR} acts in the same account; it was kept \
             in state and nothing was changed"
        ))),
        (Some(r), a) => Err(CliError::Message(format!(
            "deployment '{deployment}' lives in RunPod account {r}, but {API_KEY_ENV_VAR} acts \
             in {}. Switch {API_KEY_ENV_VAR} to a key of that account and retry; nothing was \
             changed",
            a.unwrap_or("an account amicu has not seen (no amicu-account-marker secret)")
        ))),
    }
}
