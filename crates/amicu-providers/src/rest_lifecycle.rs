//! Shared, crate-private provision-adoption and teardown algorithm for
//! REST-style server adapters.
//!
//! Implements the orphan-proof contract once, so every REST adapter gets
//! identical semantics:
//!
//! - **Scope first.** Before any "absent" answer is trusted, the record's scope
//!   fingerprint must match the record and the backend scope (zone, project,
//!   collection) must be reachable. A 404 at the wrong scope is an error.
//! - **Adoption** matches identity, offering and region, and errors on
//!   ambiguity.
//! - **Teardown** deletes by recorded id and by identity, then re-checks
//!   absence before reporting [`TeardownOutcome::ConfirmedGone`].
//! - Names and ids from a (persisted, hence untrusted) record are validated
//!   before they are put into a URL.

use amicu_core::adapter::AdapterError;
use async_trait::async_trait;

use crate::provider::{ProviderError, ProvisioningHandle, TeardownOutcome};

/// A resource found by identity lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub id: String,
    /// Offering the resource was created with, when the API exposes it.
    pub offering: Option<String>,
    /// Region the resource sits in, when the API exposes it.
    pub region: Option<String>,
    /// Raw provider state string, when the API exposes it.
    pub state: Option<String>,
}

/// Pages fetched at most per identity listing (loop guard).
pub(crate) const MAX_PAGES: u32 = 100;

/// The backend operations the generic algorithms need.
#[async_trait]
pub(crate) trait ServerApi: Sync {
    /// The scope fingerprint implied by the record's own fields (provider,
    /// zone/region, project). Errors when the record lacks a required scope
    /// part (e.g. a Scaleway record with no zone): there is no region fallback.
    fn scope_of(&self, handle: &ProvisioningHandle) -> Result<String, AdapterError>;

    /// Probe that the record's backend scope exists and is reachable with this
    /// credential (collection / project endpoint answers 2xx). Returns the
    /// backend-reported account or project id when the API exposes one.
    async fn verify_scope(
        &self,
        handle: &ProvisioningHandle,
        token: Option<&str>,
    ) -> Result<Option<String>, AdapterError>;

    /// Every resource carrying the record's identity name, following
    /// pagination. Must filter client-side on the exact name (never trust a
    /// server-side filter, since results are deleted). A collection-level 404
    /// is an error, not an empty list.
    async fn list_by_identity(
        &self,
        handle: &ProvisioningHandle,
        token: Option<&str>,
    ) -> Result<Vec<Found>, AdapterError>;

    /// Delete resource `id`. A missing resource (HTTP 404) is `Ok`; only call
    /// after [`guard_scope`].
    async fn delete(
        &self,
        handle: &ProvisioningHandle,
        id: &str,
        token: Option<&str>,
    ) -> Result<(), AdapterError>;

    /// Whether resource `id` still exists (HTTP 404 means it does not); only
    /// call after [`guard_scope`].
    async fn exists(
        &self,
        handle: &ProvisioningHandle,
        id: &str,
        token: Option<&str>,
    ) -> Result<bool, AdapterError>;
}

/// A backend-contract failure with a clear, secret-free message.
pub(crate) fn refuse(detail: impl Into<String>) -> AdapterError {
    AdapterError::from_kind(ProviderError::Backend {
        detail: detail.into(),
    })
}

/// Reject a name or id that could alter a URL path or query.
pub(crate) fn check_record(handle: &ProvisioningHandle) -> Result<(), AdapterError> {
    let name = &handle.identity.name;
    let name_ok = name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !name.is_empty() && !name_ok {
        return Err(refuse("stop record has an invalid identity name"));
    }
    let id_ok = |id: &str| {
        id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    if handle.has_resource_id() && !id_ok(&handle.id) {
        return Err(refuse("stop record has an invalid resource id"));
    }
    Ok(())
}

/// Verify the record's scope before trusting any 404 (or creating anything):
/// the stored fingerprint must equal the one the record implies, and the
/// backend scope must be reachable. Returns the backend-reported account id.
pub(crate) async fn guard_scope<A: ServerApi>(
    api: &A,
    handle: &ProvisioningHandle,
    token: Option<&str>,
) -> Result<Option<String>, AdapterError> {
    check_record(handle)?;
    let expected = api.scope_of(handle)?;
    match handle.scope.as_deref() {
        Some(stored) if stored == expected => {}
        Some(_) => return Err(refuse("stop record scope does not match its fields")),
        None => return Err(refuse("stop record has no scope fingerprint")),
    }
    let observed = api.verify_scope(handle, token).await?;
    if let (Some(recorded), Some(seen)) = (handle.resources.get("account_id"), &observed)
        && recorded != seen
    {
        return Err(refuse(
            "credential belongs to a different account/project than the stop record",
        ));
    }
    Ok(observed)
}

/// Find the single resource already created under the prepared record's
/// identity, for adoption. `Ok(None)` when none exists; an error when the
/// match disagrees on offering or region, or is ambiguous.
pub(crate) async fn adopt<A: ServerApi>(
    api: &A,
    prepared: &ProvisioningHandle,
    token: Option<&str>,
) -> Result<Option<Found>, AdapterError> {
    let mut found = api.list_by_identity(prepared, token).await?;
    if found.len() > 1 {
        return Err(refuse(format!(
            "{} resources carry identity '{}'; refusing to guess",
            found.len(),
            prepared.identity.name
        )));
    }
    let Some(one) = found.pop() else {
        return Ok(None);
    };
    let differs = |recorded: &Option<String>, expected: &str| {
        recorded.as_deref().is_some_and(|r| r != expected)
    };
    if differs(&one.offering, &prepared.offering_id) || differs(&one.region, &prepared.region) {
        return Err(refuse(
            "existing resource with this identity has a different offering or region",
        ));
    }
    Ok(Some(one))
}

/// Stop everything `handle` describes and confirm absence.
pub(crate) async fn teardown<A: ServerApi>(
    api: &A,
    handle: &ProvisioningHandle,
    token: Option<&str>,
) -> Result<TeardownOutcome, AdapterError> {
    guard_scope(api, handle, token).await?;
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
    for f in api.list_by_identity(handle, token).await? {
        if !ids.contains(&f.id) {
            ids.push(f.id);
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

/// In-memory simulators: the single live instance already created under the
/// prepared record's identity, for idempotent provisioning. Errors on a
/// disagreeing offering/region or on ambiguity.
pub(crate) fn adopt_in_memory(
    instances: &std::collections::HashMap<String, ProvisioningHandle>,
    prepared: &ProvisioningHandle,
) -> Result<Option<ProvisioningHandle>, AdapterError> {
    if prepared.identity.name.is_empty() {
        return Err(refuse("record has no resource identity"));
    }
    let mut live = instances.values().filter(|h| {
        h.identity.name == prepared.identity.name
            && !matches!(
                h.state,
                crate::provider::ProvisioningState::Terminated { .. }
            )
    });
    let Some(one) = live.next() else {
        return Ok(None);
    };
    if live.next().is_some() {
        return Err(refuse("several instances carry this identity"));
    }
    if one.offering_id != prepared.offering_id || one.region != prepared.region {
        return Err(refuse(
            "existing instance with this identity has a different offering or region",
        ));
    }
    Ok(Some(one.clone()))
}

/// In-memory simulators: keys of every instance the record names, by recorded
/// id or by identity.
pub(crate) fn matching_keys(
    instances: &std::collections::HashMap<String, ProvisioningHandle>,
    handle: &ProvisioningHandle,
) -> Vec<String> {
    instances
        .iter()
        .filter(|(key, h)| {
            (handle.has_resource_id() && **key == handle.id)
                || (!handle.identity.name.is_empty() && h.identity.name == handle.identity.name)
        })
        .map(|(key, _)| key.clone())
        .collect()
}
