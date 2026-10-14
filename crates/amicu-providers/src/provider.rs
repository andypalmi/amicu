//! The provider adapter family trait and its provisioning surface.
//!
//! A [`Provider`] adapter provisions and tears down external compute (a cloud
//! GPU host, a bare-metal server, a local machine) behind the common
//! [`Adapter`] contract. As with every family, the method's type is the
//! load-bearing signal for *when* it runs:
//!
//! - [`capabilities`](Provider::capabilities) is **planning-time and pure**: a
//!   synchronous `fn` returning the owned, serde-serializable
//!   [`ProviderCapabilities`] descriptor with no side effects and no backend
//!   contact. The planner reads it during `plan` to filter and rank stacks.
//! - [`provision`](Provider::provision) and [`teardown`](Provider::teardown)
//!   are **apply-time**: `async fn`s that touch the backend and run only during
//!   `apply`.
//! - Health is inherited from [`Adapter::health`] - reachability without side
//!   effects.
//!
//! Credentials are bound by reference: [`ProvisionRequest`] and
//! [`ProvisioningHandle`] carry only a [`SecretRef`] (a name, safe to plan and
//! log), never the [`SecretValue`]. The value is resolved transiently through a
//! [`SecretResolver`] at apply time, used to authenticate the backend call, and
//! dropped - it is never stored on the handle nor serialized. Every fallible
//! method returns [`AdapterError`]; adapters never panic.
//!
//! # The orphan-proof provider contract
//!
//! Every adapter must make it impossible to leak a billable resource. The
//! contract (enforced for every adapter by the [`conformance`](crate::conformance)
//! harness) is:
//!
//! 1. **Everything needed to stop a resource lives in the record.** The
//!    [`ProvisioningHandle`] is a self-describing, serializable stop record:
//!    provider kind, region/zone, credential *reference*, a deterministic
//!    [`ResourceIdentity`], the provider's resource ids (and, for self-hosted,
//!    the SSH host and agent id), and a schema version. It carries no secret
//!    value (a compile-time assertion guards that `SecretValue` stays
//!    non-`Serialize`).
//! 2. **Write-ahead.** [`Provider::prepare`] is pure (no backend contact) and
//!    returns the record *before* any resource exists. The caller persists it,
//!    and only then calls [`Provider::provision`]. A record whose `id` is empty
//!    means "may or may not exist"; it is still stoppable.
//! 3. **Unique, pre-known identity.** `prepare` derives the resource name from
//!    deployment id + project id plus a random nonce generated once and kept in
//!    the record, so the name is known before any API call yet cannot collide
//!    with another deployment's resource. `provision` names/tags the resource
//!    with it and adopts an existing resource carrying that exact identity
//!    (and the same offering and region) rather than creating a duplicate; more
//!    than one match is an error, never a guess. If the process dies between
//!    the create call and persisting the returned ids, or `provision` fails
//!    midway, [`Provider::teardown`] finds the resource by its identity.
//! 4. **Teardown works from the record alone**, never from adapter
//!    configuration, and is idempotent: an absent resource is confirmed gone.
//!    "Absent" is only trusted inside the right scope: the record carries a
//!    scope fingerprint (zone, project, ...) that is verified against the record
//!    and probed on the backend before any 404 counts as gone. A 404 at the
//!    wrong zone/project/account is an error, never confirmation.
//! 5. **Right credential.** The record is bound to a fingerprint of the
//!    credential that created the resource ([`credential_binding`]; never the
//!    token). Another account's token sees an empty collection and 404s, which
//!    would read as "gone", so teardown under a different credential returns
//!    [`TeardownOutcome::Unconfirmed`]. Changing the binding is an explicit act
//!    ([`ProvisioningHandle::rebind_credential`]); a record never bound cannot
//!    vouch for absence at all.
//! 6. **Confirmed or not.** [`Provider::teardown`] returns
//!    [`TeardownOutcome::ConfirmedGone`] only after the backend confirmed the
//!    resource absent; otherwise [`TeardownOutcome::Unconfirmed`] (or an error).
//!    Callers keep the record in state until `ConfirmedGone`. There is no
//!    warn-and-drop path: after any `provision` error the caller must still
//!    tear down the prepared record.
//!
//! [`SecretValue`]: amicu_core::secret::SecretValue

use std::collections::BTreeMap;

use amicu_core::adapter::{
    Adapter, AdapterError, AdapterErrorKind, AdapterId, DescribeCapabilities,
};
use amicu_core::diagnostic::{DiagnosticCode, Severity, catalog};
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::ProviderCapabilities;

/// The provisioning request handed to [`Provider::provision`].
///
/// It names *what* to provision by referencing a [`ComputeOffering`] id and a
/// [`Region`] code from the provider's [`ProviderCapabilities`], and binds the
/// backend credential by [`SecretRef`] - a name only. No [`SecretValue`] ever
/// appears here, so the request is safe to serialize into a plan.
///
/// [`ComputeOffering`]: crate::capability::ComputeOffering
/// [`Region`]: crate::capability::Region
/// [`SecretValue`]: amicu_core::secret::SecretValue
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionRequest {
    /// The [`ComputeOffering`](crate::capability::ComputeOffering) id to
    /// provision, e.g. `"gpu.h100.1x"`.
    pub offering_id: String,
    /// The [`Region`](crate::capability::Region) code to provision in, e.g.
    /// `"eu-central-1"`.
    pub region: String,
    /// Optional advanced override for the zone (AZ) a zone-addressed provider API
    /// targets, e.g. `"fr-par-2"`. `None` lets the adapter resolve the region's
    /// default zone; the planner never sets this - it is not a planning input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// Reference to the credential authenticating the backend call, matching
    /// the provider's [`CredentialsModel`](crate::capability::CredentialsModel)
    /// kind. `None` for a credential-free provider. Resolved to a value only at
    /// apply time; the value is never stored here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<SecretRef>,
}

impl ProvisionRequest {
    /// A request to provision `offering_id` in `region` with no credential.
    #[must_use]
    pub fn new(offering_id: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            offering_id: offering_id.into(),
            region: region.into(),
            zone: None,
            credentials: None,
        }
    }

    /// Bind the backend credential by reference (name only, never a value).
    #[must_use]
    pub fn with_credentials(mut self, credentials: SecretRef) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Override the zone (AZ) the adapter would otherwise resolve from the
    /// region. Advanced use only; leave unset to take the region's default zone.
    #[must_use]
    pub fn with_zone(mut self, zone: impl Into<String>) -> Self {
        self.zone = Some(zone.into());
        self
    }
}

/// The lifecycle state of a provisioned instance, observed via
/// [`Provider::status`].
///
/// Intended transitions:
/// - `Requested` → `Provisioning` → `Ready` (bring-up).
/// - `Ready` → `Stopping` → `Stopped` (hibernate, when the provider's
///   [`LifecycleBehavior::supports_hibernation`] is set); `Stopped` →
///   `Provisioning` → `Ready` (resume).
/// - `Ready` | `Stopped` → `Terminated` (teardown; terminal).
/// - any non-terminal state → `Failed` (terminal).
///
/// Plain data, serde-tagged (`{"state": "..."}`); every `detail` string is
/// secret-free.
///
/// [`LifecycleBehavior::supports_hibernation`]: crate::capability::LifecycleBehavior::supports_hibernation
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProvisioningState {
    /// The request was accepted; bring-up has not yet started.
    Requested,
    /// The instance is being brought up (or resumed).
    Provisioning {
        /// Safe, secret-free progress summary.
        detail: String,
    },
    /// Provisioned and ready to serve.
    Ready {
        /// Safe, secret-free readiness summary.
        detail: String,
    },
    /// Being stopped / hibernated.
    Stopping {
        /// Safe, secret-free progress summary.
        detail: String,
    },
    /// Stopped / hibernated and resumable.
    Stopped {
        /// Safe, secret-free summary of the stopped instance.
        detail: String,
    },
    /// Torn down; terminal.
    Terminated {
        /// Safe, secret-free summary of the teardown.
        detail: String,
    },
    /// A bring-up or lifecycle operation failed; terminal.
    Failed {
        /// Safe, secret-free failure summary.
        detail: String,
    },
}

impl ProvisioningState {
    /// Whether the instance is [`Ready`](ProvisioningState::Ready).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, ProvisioningState::Ready { .. })
    }

    /// Whether the instance is [`Stopped`](ProvisioningState::Stopped).
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        matches!(self, ProvisioningState::Stopped { .. })
    }

    /// Whether the instance is [`Failed`](ProvisioningState::Failed).
    #[must_use]
    pub fn is_failed(&self) -> bool {
        matches!(self, ProvisioningState::Failed { .. })
    }

    /// Whether the state is terminal (admits no further transitions):
    /// [`Terminated`](ProvisioningState::Terminated) or
    /// [`Failed`](ProvisioningState::Failed).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ProvisioningState::Terminated { .. } | ProvisioningState::Failed { .. }
        )
    }
}

/// Current schema version of [`ProvisioningHandle`].
pub const HANDLE_SCHEMA_VERSION: u32 = 2;

fn default_schema_version() -> u32 {
    HANDLE_SCHEMA_VERSION
}

fn default_state() -> ProvisioningState {
    ProvisioningState::Requested
}

// Compile-time guarantee that the stop record cannot carry a secret value: the
// build fails if `SecretValue` ever becomes `Serialize`.
const _: () = {
    trait AmbiguousIfSerialize<A> {
        fn probe() {}
    }
    impl<T: ?Sized> AmbiguousIfSerialize<()> for T {}
    impl<T: ?Sized + Serialize> AmbiguousIfSerialize<u8> for T {}
    let _ = <amicu_core::secret::SecretValue as AmbiguousIfSerialize<_>>::probe;
};

/// Domain-separated, truncated SHA-256 fingerprint of a credential's secret
/// bytes: `amicu-cred-v1:` + 32 hex chars (128 bits). It identifies *which*
/// credential created a resource without being usable as one.
#[must_use]
pub fn credential_binding(secret: &SecretValue) -> String {
    binding_of_bytes(secret.expose_secret())
}

/// The binding of a record that uses no credential.
pub(crate) const NO_CREDENTIAL_BINDING: &str = "amicu-cred-v1:none";

pub(crate) fn binding_of_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut hasher = Sha256::new();
    hasher.update(b"amicu-cred-v1:");
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::from("amicu-cred-v1:");
    for byte in &digest[..16] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The identity a resource is created under.
///
/// Built from the deployment id and project id plus a random nonce generated
/// once, in [`Provider::prepare`], and persisted in the record. The name is
/// therefore known before any API call, stable across retries and crashes, and
/// unique per prepared record: two records for the same deployment and project
/// never share a name, and the name cannot collide with another deployment's.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResourceIdentity {
    /// The deployment this resource belongs to.
    #[serde(default)]
    pub deployment_id: String,
    /// The project this resource belongs to.
    #[serde(default)]
    pub project_id: String,
    /// Random 128-bit nonce (32 lowercase hex chars); empty until `prepare`.
    #[serde(default)]
    pub nonce: String,
    /// The provider-safe resource name/tag: `amicu-<project>-<deployment>-<nonce>`
    /// (lowercase alphanumerics and `-`, at most 63 characters). Empty until
    /// the nonce is set.
    #[serde(default)]
    pub name: String,
}

impl ResourceIdentity {
    /// Maximum length of the derived name (a DNS label).
    pub const MAX_NAME_LEN: usize = 63;

    /// An identity for `deployment_id` in `project_id`, not yet nonced. The
    /// nonce (and so the name) is added by [`Provider::prepare`] through
    /// [`ProvisioningHandle::prepared`].
    #[must_use]
    pub fn new(deployment_id: impl Into<String>, project_id: impl Into<String>) -> Self {
        Self {
            deployment_id: deployment_id.into(),
            project_id: project_id.into(),
            nonce: String::new(),
            name: String::new(),
        }
    }

    /// Set a fresh random nonce, unless one is already present, and derive the
    /// name. Idempotent on an already-nonced identity.
    #[must_use]
    pub fn with_fresh_nonce(self) -> Self {
        if self.nonce.is_empty() {
            let nonce = uuid::Uuid::new_v4().simple().to_string();
            self.with_nonce(&nonce)
        } else {
            self
        }
    }

    /// Set `nonce` (lowercase hex) and derive the name from it.
    #[must_use]
    pub fn with_nonce(mut self, nonce: &str) -> Self {
        let slug = |v: &str| -> String {
            v.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches('-')
                .to_string()
        };
        let mut base = format!(
            "amicu-{}-{}",
            slug(&self.project_id),
            slug(&self.deployment_id)
        );
        base.truncate(Self::MAX_NAME_LEN - nonce.len() - 1);
        self.name = format!("{}-{nonce}", base.trim_end_matches('-'));
        self.nonce = nonce.to_string();
        self
    }
}

/// A fully self-describing, serializable stop record for a provisioned (or
/// about-to-be-provisioned) resource.
///
/// Produced by [`Provider::prepare`] *before* any resource exists, persisted
/// by the caller (write-ahead), completed by [`Provider::provision`], and
/// sufficient on its own for [`Provider::status`] / [`Provider::teardown`].
/// It carries the credential by [`SecretRef`] (name only); a compile-time
/// assertion keeps `SecretValue` non-`Serialize`, so no secret can be stored.
/// Fields have serde defaults so older/newer records stay readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawHandle")]
pub struct ProvisioningHandle {
    /// Schema version of this record.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Primary provider-assigned resource id. Empty until `provision` has
    /// recorded one; an empty id still stops via [`identity`](Self::identity).
    #[serde(default)]
    pub id: String,
    /// The provider kind that owns the resource.
    pub provider: AdapterId,
    /// The region the resource runs in.
    pub region: String,
    /// The zone (AZ), when the adapter resolved one; carried so later
    /// zone-addressed calls target the same zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// The offering that was provisioned.
    #[serde(default)]
    pub offering_id: String,
    /// Reference to the credential used for backend operations (name only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<SecretRef>,
    /// The identity the resource is created under.
    #[serde(default)]
    pub identity: ResourceIdentity,
    /// Scope fingerprint (provider, zone/region, project) fixed at `prepare`.
    /// Teardown refuses to trust an "absent" answer unless this matches the
    /// record and the backend scope is reachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Fingerprint of the credential that created the resource
    /// (`amicu-cred-v1:` + hex of a truncated, domain-separated SHA-256 of the
    /// token; never the token). Teardown refuses to treat "absent" as gone under
    /// a different credential: another account's token sees an empty world. Bind
    /// it with [`bind_credential`](Self::bind_credential) right after `prepare`,
    /// before the record is persisted; `provision` binds it if still unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_binding: Option<String>,
    /// Account/project id the backend reported for the scope (OVH project id,
    /// Scaleway project), checked before any absence is trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Organization the backend reported for the scope (Scaleway), recorded at
    /// create next to the project in `account_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    /// Pinned SSH host-key fingerprint (`SHA256:...`) for self-hosted records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host_key: Option<String>,
    /// Further provider resource ids / addressing needed to stop the resource
    /// (e.g. `ssh_host`, `ssh_user`, `ssh_port`, `agent_id` for self-hosted).
    /// Ids and addresses only - never secret values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources: BTreeMap<String, String>,
    /// The resource's lifecycle state.
    #[serde(default = "default_state")]
    pub state: ProvisioningState,
}

/// Wire shape of [`ProvisioningHandle`]: identical, but records written before
/// `account_id` and `ssh_host_key` became typed fields kept them in `resources`;
/// `From` moves them across so old records stay readable.
#[derive(Deserialize)]
struct RawHandle {
    /// Schema version of this record.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Primary provider-assigned resource id. Empty until `provision` has
    /// recorded one; an empty id still stops via [`identity`](Self::identity).
    #[serde(default)]
    pub id: String,
    /// The provider kind that owns the resource.
    pub provider: AdapterId,
    /// The region the resource runs in.
    pub region: String,
    /// The zone (AZ), when the adapter resolved one; carried so later
    /// zone-addressed calls target the same zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// The offering that was provisioned.
    #[serde(default)]
    pub offering_id: String,
    /// Reference to the credential used for backend operations (name only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<SecretRef>,
    /// The identity the resource is created under.
    #[serde(default)]
    pub identity: ResourceIdentity,
    /// Scope fingerprint (provider, zone/region, project) fixed at `prepare`.
    /// Teardown refuses to trust an "absent" answer unless this matches the
    /// record and the backend scope is reachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Fingerprint of the credential that created the resource
    /// (`amicu-cred-v1:` + hex of a truncated, domain-separated SHA-256 of the
    /// token; never the token). Teardown refuses to treat "absent" as gone under
    /// a different credential: another account's token sees an empty world. Bind
    /// it with [`bind_credential`](Self::bind_credential) right after `prepare`,
    /// before the record is persisted; `provision` binds it if still unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_binding: Option<String>,
    /// Account/project id the backend reported for the scope (OVH project id,
    /// Scaleway project), checked before any absence is trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Organization the backend reported for the scope (Scaleway), recorded at
    /// create next to the project in `account_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    /// Pinned SSH host-key fingerprint (`SHA256:...`) for self-hosted records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host_key: Option<String>,
    /// Further provider resource ids / addressing needed to stop the resource
    /// (e.g. `ssh_host`, `ssh_user`, `ssh_port`, `agent_id` for self-hosted).
    /// Ids and addresses only - never secret values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources: BTreeMap<String, String>,
    /// The resource's lifecycle state.
    #[serde(default = "default_state")]
    pub state: ProvisioningState,
}

impl From<RawHandle> for ProvisioningHandle {
    fn from(mut raw: RawHandle) -> Self {
        if raw.ssh_host_key.is_none() {
            raw.ssh_host_key = raw.resources.remove("ssh_host_key");
        } else {
            raw.resources.remove("ssh_host_key");
        }
        if raw.account_id.is_none() {
            raw.account_id = raw.resources.remove("account_id");
        } else {
            raw.resources.remove("account_id");
        }
        if raw.organization_id.is_none() {
            raw.organization_id = raw.resources.remove("organization_id");
        } else {
            raw.resources.remove("organization_id");
        }
        Self {
            schema_version: raw.schema_version,
            id: raw.id,
            provider: raw.provider,
            region: raw.region,
            zone: raw.zone,
            offering_id: raw.offering_id,
            credentials: raw.credentials,
            identity: raw.identity,
            scope: raw.scope,
            credential_binding: raw.credential_binding,
            account_id: raw.account_id,
            organization_id: raw.organization_id,
            ssh_host_key: raw.ssh_host_key,
            resources: raw.resources,
            state: raw.state,
        }
    }
}

impl ProvisioningHandle {
    /// A freshly prepared record: identity known, no resource created yet.
    #[must_use]
    pub fn prepared(
        provider: AdapterId,
        request: &ProvisionRequest,
        identity: ResourceIdentity,
    ) -> Self {
        Self {
            schema_version: HANDLE_SCHEMA_VERSION,
            id: String::new(),
            provider,
            region: request.region.clone(),
            zone: request.zone.clone(),
            offering_id: request.offering_id.clone(),
            credentials: request.credentials.clone(),
            identity: identity.with_fresh_nonce(),
            scope: None,
            credential_binding: None,
            account_id: None,
            organization_id: None,
            ssh_host_key: None,
            resources: BTreeMap::new(),
            state: ProvisioningState::Requested,
        }
    }

    /// Record the created resource: sets the primary `id`, files it under
    /// `resource_key` in [`resources`](Self::resources), and sets the state.
    #[must_use]
    pub fn completed(
        mut self,
        resource_key: &str,
        id: impl Into<String>,
        state: ProvisioningState,
    ) -> Self {
        let id = id.into();
        self.resources.insert(resource_key.to_string(), id.clone());
        self.id = id;
        self.state = state;
        self
    }

    /// Bind the record to the credential that will operate on it. Idempotent for
    /// the same credential; an error if the record is already bound to a
    /// different one (use [`rebind_credential`](Self::rebind_credential) for a
    /// deliberate change).
    ///
    /// # Errors
    /// [`ProviderError::CredentialsRejected`] when bound to another credential.
    pub fn bind_credential(&mut self, secret: &SecretValue) -> Result<(), AdapterError> {
        let binding = credential_binding(secret);
        match &self.credential_binding {
            Some(existing) if *existing != binding => Err(AdapterError::from_kind(
                ProviderError::CredentialsRejected {
                    detail: "credential differs from the one this record is bound to".into(),
                },
            )),
            _ => {
                self.credential_binding = Some(binding);
                Ok(())
            }
        }
    }

    /// Deliberately re-bind the record to a different credential, e.g. after a
    /// token rotation or when adopting a record whose binding was never set.
    /// This is an explicit operator act: nothing in this crate calls it, and it
    /// must never be done silently, because the old binding is what stops
    /// another account's empty view from being mistaken for "gone".
    pub fn rebind_credential(&mut self, secret: &SecretValue) {
        self.credential_binding = Some(credential_binding(secret));
    }

    /// Whether a provider resource id has been recorded.
    #[must_use]
    pub fn has_resource_id(&self) -> bool {
        !self.id.is_empty()
    }
}

/// The result of a [`Provider::teardown`] that did not fail outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[must_use = "an unconfirmed teardown must keep the record in state"]
pub enum TeardownOutcome {
    /// The backend confirmed every resource of the record is absent. The only
    /// outcome after which the record may leave state.
    ConfirmedGone,
    /// A stop was issued but absence could not be confirmed; keep the record
    /// and retry (teardown is idempotent).
    Unconfirmed {
        /// Safe, secret-free reason.
        reason: String,
    },
}

impl TeardownOutcome {
    /// Whether the resource is confirmed gone.
    #[must_use]
    pub fn is_confirmed_gone(&self) -> bool {
        matches!(self, TeardownOutcome::ConfirmedGone)
    }
}

/// Failures a provider adapter can raise, each mapped to a stable
/// `AMICU-PROVIDER-<NUMBER>` code via [`AdapterErrorKind`].
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Requested capacity is unavailable for the offering in the region.
    /// Transient; maps to `AMICU-PROVIDER-001`.
    #[error("no capacity for offering '{offering}' in region '{region}'")]
    CapacityUnavailable {
        /// The offering id with no capacity.
        offering: String,
        /// The region with no capacity.
        region: String,
    },
    /// The request names an offering the provider does not offer. Permanent;
    /// maps to `AMICU-PROVIDER-010`.
    #[error("unknown offering '{offering}'")]
    UnknownOffering {
        /// The unrecognized offering id.
        offering: String,
    },
    /// The request names a region the provider does not serve. Permanent; maps
    /// to `AMICU-PROVIDER-011`.
    #[error("unknown region '{region}'")]
    UnknownRegion {
        /// The unrecognized region code.
        region: String,
    },
    /// The handle refers to no known provisioned instance. Permanent; maps to
    /// `AMICU-PROVIDER-012`.
    #[error("unknown instance '{id}'")]
    UnknownInstance {
        /// The unrecognized instance id.
        id: String,
    },
    /// The backend failed in a way that may succeed on retry. Maps to
    /// `AMICU-PROVIDER-013`.
    #[error("provider backend error: {detail}")]
    Backend {
        /// A safe, secret-free description of the failure.
        detail: String,
    },
    /// The resolved credential was rejected by the provider (e.g. empty or
    /// malformed). Permanent; maps to `AMICU-PROVIDER-014`.
    #[error("provider rejected the supplied credential: {detail}")]
    CredentialsRejected {
        /// A safe, secret-free description of why it was rejected.
        detail: String,
    },
    /// The operation is permanently unsupported by this adapter (e.g. a
    /// catalog-only adapter with no apply-time backend yet). Never
    /// retryable, unlike [`ProviderError::Backend`] - there is no transient
    /// condition to wait out. Maps to `AMICU-PROVIDER-015`.
    #[error("provider does not support this operation: {detail}")]
    Unsupported {
        /// A safe, secret-free description of what is unsupported.
        detail: String,
    },
}

impl AdapterErrorKind for ProviderError {
    fn code(&self) -> DiagnosticCode {
        match self {
            ProviderError::CapacityUnavailable { .. } => catalog::PROVIDER_CAPACITY_UNAVAILABLE,
            ProviderError::UnknownOffering { .. } => catalog::PROVIDER_UNKNOWN_OFFERING,
            ProviderError::UnknownRegion { .. } => catalog::PROVIDER_UNKNOWN_REGION,
            ProviderError::UnknownInstance { .. } => catalog::PROVIDER_UNKNOWN_INSTANCE,
            ProviderError::Backend { .. } => catalog::PROVIDER_BACKEND,
            ProviderError::CredentialsRejected { .. } => catalog::PROVIDER_CREDENTIALS_REJECTED,
            ProviderError::Unsupported { .. } => catalog::PROVIDER_UNSUPPORTED_OPERATION,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn retryable(&self) -> bool {
        // Capacity may free up and backend faults may clear; unknown
        // offering/region/instance are permanent request/config errors.
        matches!(
            self,
            ProviderError::CapacityUnavailable { .. } | ProviderError::Backend { .. }
        )
    }
}

/// The contract every provider adapter implements.
///
/// Extends the common [`Adapter`] contract (identity + async
/// [`health`](Adapter::health)) and [`DescribeCapabilities`] (so providers are
/// [`Registry::discover`](amicu_core::adapter::Registry::discover)-able)
/// with a pure capability method and the async provisioning surface.
#[async_trait]
pub trait Provider: Adapter + DescribeCapabilities {
    /// This provider's plain-data capability descriptor.
    ///
    /// Planning-time and pure: no side effects, no backend contact. Consistent
    /// with the runtime family, the owned descriptor is returned directly; the
    /// synchronous `fn` (vs the `async` provisioning methods) is the signal
    /// that it runs during `plan`.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Validate `request` and return the stop record for it, under
    /// `identity`, **without contacting the backend or creating anything**.
    /// The caller persists the record, then calls [`provision`](Self::provision).
    ///
    /// # Errors
    /// [`ProviderError::UnknownOffering`] / [`ProviderError::UnknownRegion`]
    /// for a request the provider cannot serve.
    fn prepare(
        &self,
        request: &ProvisionRequest,
        identity: ResourceIdentity,
    ) -> Result<ProvisioningHandle, AdapterError>;

    /// Create the resource described by the (already persisted) `prepared`
    /// record under its deterministic identity, adopting an existing resource
    /// with that identity instead of duplicating it, and return the completed
    /// record (resource ids filled in). Apply-time only.
    ///
    /// On error the resource may still exist: the caller must keep the
    /// prepared record and tear it down.
    ///
    /// # Errors
    /// Capacity, credential and backend failures folded into an
    /// [`AdapterError`].
    async fn provision(
        &self,
        prepared: ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningHandle, AdapterError>;

    /// Observe the live lifecycle state of a provisioned instance. Apply-time
    /// only; re-authenticates via `resolver` (the handle's `credentials`) to
    /// query the backend.
    ///
    /// # Errors
    /// [`ProviderError::UnknownInstance`] when `handle` refers to no known
    /// instance, or the resolver's error when the credential is unavailable -
    /// folded into an [`AdapterError`].
    async fn status(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningState, AdapterError>;

    /// Stop everything the record describes, from the record alone.
    /// Idempotent: an absent resource counts as gone. Returns
    /// [`TeardownOutcome::ConfirmedGone`] only after the backend confirmed
    /// absence; never `Ok` for an unconfirmed stop. Apply-time only.
    ///
    /// # Errors
    /// Credential/backend failures folded into an [`AdapterError`]; the record
    /// must then be kept.
    async fn teardown(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<TeardownOutcome, AdapterError>;
}

/// Test-only convenience: `prepare` + `provision` in one call, for adapter unit
/// tests that exercise the happy path rather than the write-ahead split.
#[cfg(test)]
#[async_trait]
pub(crate) trait ProvisionCompat: Provider {
    async fn provision_request(
        &self,
        request: &ProvisionRequest,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningHandle, AdapterError> {
        let prepared = self.prepare(request, ResourceIdentity::new("deployment", "project"))?;
        self.provision(prepared, resolver).await
    }
}

#[cfg(test)]
impl<T: Provider + ?Sized> ProvisionCompat for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_operation_is_permanent_not_retryable() {
        let err = AdapterError::from_kind(ProviderError::Unsupported {
            detail: "catalog-only adapter".into(),
        });
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-015");
        assert!(!err.is_retryable());
    }

    #[test]
    fn provider_error_normalizes_to_catalog_diagnostic() {
        let adapter_err = AdapterError::from_kind(ProviderError::UnknownOffering {
            offering: "gpu.h100.1x".into(),
        });
        assert_eq!(adapter_err.code(), catalog::PROVIDER_UNKNOWN_OFFERING);

        let diag = adapter_err.to_diagnostic();
        assert_eq!(diag.code.to_string(), "AMICU-PROVIDER-010");
        assert_eq!(diag.severity, Severity::Error);
        assert_eq!(diag.message, "unknown offering 'gpu.h100.1x'");
        // Secret-free: only the offering id the caller supplied is echoed back.
        assert!(!diag.message.contains("credential"));
    }

    #[test]
    fn zone_is_optional_and_additive() {
        // The zone override is additive: existing constructors leave it unset and
        // it is omitted from the serialized request/handle when absent.
        let req = ProvisionRequest::new("gpu.h100.1x", "fr-par");
        assert!(req.zone.is_none());
        assert!(!serde_json::to_string(&req).unwrap().contains("zone"));

        let req = req.with_zone("fr-par-2");
        assert_eq!(req.zone.as_deref(), Some("fr-par-2"));
        assert!(serde_json::to_string(&req).unwrap().contains("fr-par-2"));

        let handle = ProvisioningHandle::prepared(
            AdapterId::new("scaleway"),
            &ProvisionRequest::new("gpu.h100.1x", "fr-par"),
            ResourceIdentity::new("d", "p"),
        );
        assert!(!serde_json::to_string(&handle).unwrap().contains("zone"));
    }

    #[test]
    fn record_roundtrips_and_tolerates_missing_fields() {
        let mut h = ProvisioningHandle::prepared(
            AdapterId::new("hetzner"),
            &ProvisionRequest::new("o", "nbg1"),
            ResourceIdentity::new("dep-1", "proj-1"),
        );
        h.id = "42".into();
        h.resources.insert("server_id".into(), "42".into());
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(
            serde_json::from_str::<ProvisioningHandle>(&json).unwrap(),
            h
        );
        let min: ProvisioningHandle =
            serde_json::from_str(r#"{"provider":"hetzner","region":"nbg1"}"#).unwrap();
        assert_eq!(min.schema_version, HANDLE_SCHEMA_VERSION);
        assert!(min.scope.is_none());
        assert!(!min.has_resource_id());
    }

    #[test]
    fn old_records_with_resource_map_fields_are_migrated() {
        let json = r#"{"provider":"self-hosted","region":"r","resources":
            {"ssh_host_key":"SHA256:abc","account_id":"proj-9","organization_id":"org-9","ssh_host":"h"}}"#;
        let h: ProvisioningHandle = serde_json::from_str(json).unwrap();
        assert_eq!(h.ssh_host_key.as_deref(), Some("SHA256:abc"));
        assert_eq!(h.account_id.as_deref(), Some("proj-9"));
        assert_eq!(h.organization_id.as_deref(), Some("org-9"));
        assert!(!h.resources.contains_key("organization_id"));
        assert!(!h.resources.contains_key("ssh_host_key"));
        assert!(!h.resources.contains_key("account_id"));
        assert_eq!(h.resources.get("ssh_host").map(String::as_str), Some("h"));
    }

    #[test]
    fn credential_binding_is_a_fingerprint_not_the_token() {
        let secret = SecretValue::new("tok-123".to_string());
        let binding = credential_binding(&secret);
        assert!(binding.starts_with("amicu-cred-v1:"));
        assert_eq!(binding.len(), "amicu-cred-v1:".len() + 32);
        assert!(!binding.contains("tok-123"));
        assert_ne!(
            binding,
            credential_binding(&SecretValue::new("tok-124".to_string()))
        );

        let mut h = ProvisioningHandle::prepared(
            AdapterId::new("p"),
            &ProvisionRequest::new("o", "r"),
            ResourceIdentity::new("d", "p"),
        );
        h.bind_credential(&secret).unwrap();
        h.bind_credential(&secret).unwrap(); // same credential: idempotent
        let other = SecretValue::new("tok-124".to_string());
        assert!(h.bind_credential(&other).is_err(), "never rebinds silently");
        h.rebind_credential(&other);
        assert_eq!(h.credential_binding, Some(credential_binding(&other)));
    }

    #[test]
    fn identity_is_unique_per_prepare_dns_safe_and_stable_once_nonced() {
        let req = ProvisionRequest::new("o", "r");
        let a = ProvisioningHandle::prepared(
            AdapterId::new("p"),
            &req,
            ResourceIdentity::new("Dep_1", "My Project"),
        );
        let b = ProvisioningHandle::prepared(
            AdapterId::new("p"),
            &req,
            ResourceIdentity::new("Dep_1", "My Project"),
        );
        // Same inputs, different records: names never collide.
        assert_ne!(a.identity.name, b.identity.name);
        assert_eq!(a.identity.nonce.len(), 32);
        assert!(a.identity.name.len() <= ResourceIdentity::MAX_NAME_LEN);
        assert!(
            a.identity
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        // Re-nonced identity keeps its nonce (stable across retries).
        assert_eq!(a.identity.clone().with_fresh_nonce(), a.identity);
        let long = ResourceIdentity::new("d".repeat(200), "p".repeat(200)).with_fresh_nonce();
        assert!(long.name.len() <= ResourceIdentity::MAX_NAME_LEN);
        assert!(long.name.ends_with(&long.nonce));
    }

    #[test]
    fn every_provider_error_code_is_in_the_catalog() {
        let errors = [
            ProviderError::CapacityUnavailable {
                offering: "o".into(),
                region: "r".into(),
            },
            ProviderError::UnknownOffering {
                offering: "o".into(),
            },
            ProviderError::UnknownRegion { region: "r".into() },
            ProviderError::UnknownInstance { id: "i".into() },
            ProviderError::Backend { detail: "d".into() },
            ProviderError::CredentialsRejected { detail: "d".into() },
            ProviderError::Unsupported { detail: "d".into() },
        ];
        for err in errors {
            assert!(catalog::all().contains(&err.code()));
        }
    }
}
