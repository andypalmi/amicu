//! Per-deployment runtime records carried alongside the graph in a
//! [`StateSnapshot`](crate::StateSnapshot).
//!
//! The graph says what *should* exist; these records say what amicu has
//! actually started (or is about to start), with everything needed to find and
//! stop it. They back the "no orphans" invariant (amicu-planning#205):
//!
//! - **Write-ahead.** Before a provider is asked to create anything, a record
//!   in [`DeploymentPhase::Pending`] carrying the [`StopIdentity`] is
//!   persisted. A crash between "create" and "record the handle" therefore
//!   still leaves enough in state to find and stop the resource.
//! - **Confirmed stop.** A record leaves state ([`RuntimeState::remove`]) only
//!   after the stop is confirmed; an unconfirmed stop stays as
//!   [`DeploymentPhase::Stopping`].
//!
//! Records hold references and identifiers only, never secret material.

use std::collections::BTreeMap;

use amicu_core::id::DeploymentId;
use amicu_runtime::RuntimeHandle;
use serde::{Deserialize, Serialize};

/// Where a deployment's runtime instance is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentPhase {
    /// Write-ahead record: a start is about to be (or was) attempted and may
    /// have partially succeeded. The instance may or may not exist; stopping
    /// must be idempotent.
    Pending,
    /// The instance started and its [`RuntimeHandle`] is recorded.
    Running,
    /// A stop was requested but not yet confirmed. The record must stay until
    /// it is.
    Stopping,
}

/// Everything required to stop a deployment's instance without consulting
/// anything but state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopIdentity {
    /// The deployment this instance serves.
    pub deployment: DeploymentId,
    /// Which provider/backend kind owns the instance (e.g. `"modal"`,
    /// `"llamacpp"`, `"launchd"`), selecting the adapter that can stop it.
    pub backend: String,
    /// Backend-specific identity fields (workspace, environment, app name,
    /// unit/plist path, host, project id, ...). Identifiers only; never
    /// secret values.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// The runtime record for one deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    /// Lifecycle phase.
    pub phase: DeploymentPhase,
    /// How to stop the instance.
    pub stop: StopIdentity,
    /// The started instance's handle, once known (absent while only
    /// [`DeploymentPhase::Pending`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<RuntimeHandle>,
}

impl DeploymentRecord {
    /// A write-ahead record for `stop`, with no handle yet.
    #[must_use]
    pub fn pending(stop: StopIdentity) -> Self {
        Self {
            phase: DeploymentPhase::Pending,
            stop,
            handle: None,
        }
    }

    /// The deployment this record is about.
    #[must_use]
    pub fn deployment(&self) -> &DeploymentId {
        &self.stop.deployment
    }
}

/// All per-deployment runtime records, keyed by deployment id.
///
/// Ordered (`BTreeMap`) so the canonical JSON, and hence the checksum, is
/// deterministic.
///
/// Deserialization rejects a map whose key differs from the record's own
/// `stop.deployment`, so a record can never be filed under another
/// deployment's id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RuntimeStateWire")]
pub struct RuntimeState {
    /// Records by deployment id.
    pub deployments: BTreeMap<String, DeploymentRecord>,
}

#[derive(Deserialize)]
struct RuntimeStateWire {
    #[serde(default)]
    deployments: BTreeMap<String, DeploymentRecord>,
}

impl TryFrom<RuntimeStateWire> for RuntimeState {
    type Error = String;

    fn try_from(wire: RuntimeStateWire) -> Result<Self, String> {
        for (key, record) in &wire.deployments {
            if key != record.deployment().as_str() {
                return Err(format!(
                    "runtime record filed under `{key}` belongs to deployment `{}`",
                    record.deployment()
                ));
            }
        }
        Ok(Self {
            deployments: wire.deployments,
        })
    }
}

/// Proof that a deployment's resource is confirmed gone, required to drop its
/// record from state.
///
/// Construct it only after the provider/backend has *confirmed* the stop (an
/// idempotent stop that reports the resource absent counts). It is
/// deliberately not `Deserialize`/`Default`: it cannot be conjured from
/// stored data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedStop {
    deployment: DeploymentId,
}

impl ConfirmedStop {
    /// Attest that `deployment`'s stop was confirmed.
    #[must_use]
    pub fn new(deployment: DeploymentId) -> Self {
        Self { deployment }
    }

    /// The deployment whose stop was confirmed.
    #[must_use]
    pub fn deployment(&self) -> &DeploymentId {
        &self.deployment
    }
}

impl RuntimeState {
    /// Whether no records are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.deployments.is_empty()
    }

    /// The record for `id`, if any.
    #[must_use]
    pub fn get(&self, id: &DeploymentId) -> Option<&DeploymentRecord> {
        self.deployments.get(id.as_str())
    }

    /// Insert or replace the record for its deployment, returning the old one.
    pub fn upsert(&mut self, record: DeploymentRecord) -> Option<DeploymentRecord> {
        self.deployments
            .insert(record.deployment().as_str().to_owned(), record)
    }

    /// Drop the record for `id`. Call only after the stop is confirmed.
    pub fn remove(&mut self, id: &DeploymentId) -> Option<DeploymentRecord> {
        self.deployments.remove(id.as_str())
    }
}
