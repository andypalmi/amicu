//! Shared, crate-private teardown algorithm for REST-style server adapters.
//!
//! Implements the confirmed-gone teardown of the orphan-proof contract once, so
//! every REST adapter gets identical semantics: stop what the record names
//! (by recorded id and by deterministic identity), treat 404 as gone, then
//! re-check absence before reporting [`TeardownOutcome::ConfirmedGone`].

use amicu_core::adapter::AdapterError;
use async_trait::async_trait;

use crate::provider::{ProvisioningHandle, TeardownOutcome};

/// The three backend operations the generic teardown needs.
#[async_trait]
pub(crate) trait ServerApi: Sync {
    /// Ids of every resource carrying the record's identity name. Adapters must
    /// filter client-side on the exact name: never trust a server-side filter
    /// to be exact, since the result is deleted.
    async fn list_by_identity(
        &self,
        handle: &ProvisioningHandle,
        token: Option<&str>,
    ) -> Result<Vec<String>, AdapterError>;

    /// Delete resource `id`. A missing resource (HTTP 404) is `Ok`.
    async fn delete(
        &self,
        handle: &ProvisioningHandle,
        id: &str,
        token: Option<&str>,
    ) -> Result<(), AdapterError>;

    /// Whether resource `id` still exists (HTTP 404 means it does not).
    async fn exists(
        &self,
        handle: &ProvisioningHandle,
        id: &str,
        token: Option<&str>,
    ) -> Result<bool, AdapterError>;
}

/// Stop everything `handle` describes and confirm absence.
pub(crate) async fn teardown<A: ServerApi>(
    api: &A,
    handle: &ProvisioningHandle,
    token: Option<&str>,
) -> Result<TeardownOutcome, AdapterError> {
    // Recorded id plus anything found under the identity (covers a crash before
    // the id was recorded, and duplicates from a retried provision).
    let mut ids: Vec<String> = Vec::new();
    if handle.has_resource_id() {
        ids.push(handle.id.clone());
    } else if handle.identity.name.is_empty() {
        // Nothing was ever recorded and there is no identity to look up: the
        // record cannot describe any resource, so refusing is safer than
        // guessing (a blank name must never match and delete anything).
        return Ok(TeardownOutcome::Unconfirmed {
            reason: "record has neither a resource id nor an identity".into(),
        });
    }
    for id in api.list_by_identity(handle, token).await? {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    for id in &ids {
        api.delete(handle, id, token).await?;
    }
    for id in &ids {
        if api.exists(handle, id, token).await? {
            return Ok(TeardownOutcome::Unconfirmed {
                reason: format!("resource '{id}' still present after delete"),
            });
        }
    }
    let leftover = api.list_by_identity(handle, token).await?;
    if !leftover.is_empty() {
        return Ok(TeardownOutcome::Unconfirmed {
            reason: format!(
                "{} resource(s) still carry identity '{}'",
                leftover.len(),
                handle.identity.name
            ),
        });
    }
    Ok(TeardownOutcome::ConfirmedGone)
}
