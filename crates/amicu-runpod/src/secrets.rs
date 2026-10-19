//! Content-addressed RunPod account secrets shared by the serverless and pod
//! supervisors: naming, the env templates that reference them, and the
//! ledger of names that may exist for one deployment.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use amicu_llamacpp::LlamaCppError;

use crate::client::{CreateSecret, DeleteOutcome, RunPodClient};
use crate::error::RunPodError;
use crate::supervisor::{StopOutcome, secret_name, secret_reference};

/// For each `(env key, value)` in `secret`: points `env[key]` at a
/// content-addressed secret and returns the secrets to create.
pub(crate) fn reference_secrets(
    project_id: &str,
    deployment_id: &str,
    owner: &str,
    secret: &BTreeMap<String, String>,
    env: &mut BTreeMap<String, String>,
) -> Vec<CreateSecret> {
    let mut out = Vec::with_capacity(secret.len());
    for (key, value) in secret {
        let name = secret_name(project_id, deployment_id, owner, key, value);
        env.insert(key.clone(), secret_reference(&name));
        out.push(CreateSecret {
            name,
            value: value.clone(),
            description: Some(format!("amicu: {key} for deployment {deployment_id}")),
        });
    }
    out
}

/// The secret names that may exist for one deployment as far as this
/// process knows, plus the calls that keep that set honest.
pub(crate) struct SecretLedger {
    client: Arc<RunPodClient>,
    known: Arc<Mutex<Vec<String>>>,
}

impl SecretLedger {
    pub(crate) fn new(client: Arc<RunPodClient>) -> Self {
        Self {
            client,
            known: Arc::default(),
        }
    }

    pub(crate) fn shared(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.known)
    }

    pub(crate) fn note(&self, name: &str) {
        if let Ok(mut known) = self.known.lock()
            && !known.iter().any(|n| n == name)
        {
            known.push(name.to_owned());
        }
    }

    pub(crate) fn forget(&self, name: &str) {
        if let Ok(mut known) = self.known.lock() {
            known.retain(|n| n != name);
        }
    }

    pub(crate) fn known(&self) -> Vec<String> {
        self.known.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Makes sure `secret` exists. Names are content-addressed, so an
    /// existing name already holds this value and is left alone. Returns
    /// whether this call created it.
    pub(crate) async fn ensure(&self, secret: &CreateSecret) -> Result<bool, LlamaCppError> {
        let name = &secret.name;
        let exists = self
            .client
            .find_secret(name)
            .await
            .map_err(|e| secret_err(name, &e))?
            .is_some();
        self.note(name);
        if exists {
            return Ok(false);
        }
        match self.client.create_secret(secret).await {
            Ok(_) => Ok(true),
            // Created concurrently under the same name: the same value.
            Err(RunPodError::Conflict { .. }) => Ok(false),
            Err(RunPodError::Transport(detail)) => match self.client.find_secret(name).await {
                Ok(Some(_)) => Ok(true),
                Ok(None) => {
                    self.forget(name);
                    Err(LlamaCppError::Supervisor(format!(
                        "writing RunPod secret `{name}` failed in transport ({detail}) and it \
                         does not exist; run the command again"
                    )))
                }
                Err(e) => Err(LlamaCppError::Supervisor(format!(
                    "writing RunPod secret `{name}` failed in transport ({detail}) and looking \
                     it up failed too ({e}); it may exist"
                ))),
            },
            Err(e) => {
                // RunPod answered and refused: nothing was written.
                self.forget(name);
                Err(secret_err(name, &e))
            }
        }
    }

    /// Best-effort removal of `names` (secrets written for a resource write
    /// RunPod refused); what cannot be removed stays known.
    pub(crate) async fn discard(&self, names: &[String]) {
        for name in names {
            if self.delete_by_name(name).await.is_ok() {
                self.forget(name);
            }
        }
    }

    /// Once the resource references only `current`: best-effort deletion of
    /// every other known secret (the previous generation).
    pub(crate) async fn retire_stale(&self, current: &[CreateSecret]) {
        let current: BTreeSet<&str> = current.iter().map(|s| s.name.as_str()).collect();
        for name in self.known() {
            if !current.contains(name.as_str()) && self.delete_by_name(&name).await.is_ok() {
                self.forget(&name);
            }
        }
    }

    pub(crate) async fn delete_by_name(&self, name: &str) -> Result<(), RunPodError> {
        if let Some(existing) = self.client.find_secret(name).await? {
            self.client.delete_secret(&existing.id).await?;
        }
        Ok(())
    }

    /// After RunPod confirmed `kind` `id` deleted (`outcome`): deletes every
    /// known secret, unless the resource was already gone and the account is
    /// not `confirmed` (a `404` from the wrong account must not cost the
    /// right one its secrets), in which case they are kept and listed.
    pub(crate) async fn finish_stop(
        &self,
        kind: &str,
        id: &str,
        outcome: DeleteOutcome,
        confirmed: bool,
    ) -> Result<StopOutcome, LlamaCppError> {
        let mut names = self.known();
        names.sort();
        if outcome == DeleteOutcome::AlreadyGone && !confirmed {
            return Ok(StopOutcome::AlreadyGone {
                kept_secrets: names,
            });
        }
        let mut failed: BTreeMap<String, String> = BTreeMap::new();
        for name in names {
            match self.delete_by_name(&name).await {
                Ok(()) => self.forget(&name),
                Err(e) => {
                    failed.insert(name, e.to_string());
                }
            }
        }
        if !failed.is_empty() {
            return Err(LlamaCppError::Supervisor(format!(
                "RunPod {kind} {id} was deleted, but these secrets could not be: {}",
                failed
                    .iter()
                    .map(|(n, e)| format!("`{n}` ({e})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(match outcome {
            DeleteOutcome::Deleted => StopOutcome::Deleted,
            DeleteOutcome::AlreadyGone => StopOutcome::AlreadyGone {
                kept_secrets: Vec::new(),
            },
        })
    }
}

/// [`ProcessSupervisor::stop`](amicu_llamacpp::ProcessSupervisor::stop)'s
/// view of a [`StopOutcome`]: kept secrets are an error naming them.
pub(crate) fn stop_result(kind: &str, id: &str, outcome: StopOutcome) -> Result<(), LlamaCppError> {
    match outcome {
        StopOutcome::Deleted => Ok(()),
        StopOutcome::AlreadyGone { kept_secrets } if kept_secrets.is_empty() => Ok(()),
        StopOutcome::AlreadyGone { kept_secrets } => Err(LlamaCppError::Supervisor(format!(
            "RunPod {kind} {id} was already gone, but the account could not be confirmed, so \
             these secrets were kept: {}",
            kept_secrets
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

pub(crate) fn secret_err(name: &str, error: &RunPodError) -> LlamaCppError {
    LlamaCppError::Supervisor(format!("writing RunPod secret `{name}` failed: {error}"))
}
