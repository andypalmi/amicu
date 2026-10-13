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
//! 3. **Deterministic identity.** `provision` names/tags the resource with the
//!    identity derived from deployment id + project id, and adopts an existing
//!    resource carrying that identity rather than creating a duplicate. If the
//!    process dies between the create call and persisting the returned ids, or
//!    `provision` fails midway, [`Provider::teardown`] finds the resource by its
//!    identity.
//! 4. **Teardown works from the record alone**, never from adapter
//!    configuration, and is idempotent: an absent resource is confirmed gone.
//! 5. **Confirmed or not.** [`Provider::teardown`] returns
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
use amicu_core::secret::{SecretRef, SecretResolver};
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
pub const HANDLE_SCHEMA_VERSION: u32 = 1;

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

/// The deterministic identity a resource is created under.
///
/// Derived from the deployment id and project id, so it is known before any
/// API call and lets teardown find a resource whose provider id was never
/// recorded (crash between create and persist).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResourceIdentity {
    /// The deployment this resource belongs to.
    #[serde(default)]
    pub deployment_id: String,
    /// The project this resource belongs to.
    #[serde(default)]
    pub project_id: String,
    /// The provider-safe resource name/tag derived from the two ids
    /// (lowercase alphanumerics and `-`, at most 63 characters).
    #[serde(default)]
    pub name: String,
}

impl ResourceIdentity {
    /// Maximum length of the derived name (a DNS label).
    pub const MAX_NAME_LEN: usize = 63;

    /// Derive the identity for `deployment_id` in `project_id`. Pure and
    /// deterministic: the same inputs always yield the same name, and distinct
    /// inputs yield distinct names.
    #[must_use]
    pub fn new(deployment_id: impl Into<String>, project_id: impl Into<String>) -> Self {
        let deployment_id = deployment_id.into();
        let project_id = project_id.into();
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
        // FNV-1a over the raw ids: keeps names unique even when slugging or
        // truncation would collapse distinct ids.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in project_id
            .bytes()
            .chain(std::iter::once(0))
            .chain(deployment_id.bytes())
        {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        let suffix = format!("{:08x}", hash & 0xffff_ffff);
        let mut base = format!("amicu-{}-{}", slug(&project_id), slug(&deployment_id));
        let budget = Self::MAX_NAME_LEN - suffix.len() - 1;
        base.truncate(budget);
        let name = format!("{}-{suffix}", base.trim_end_matches('-'));
        Self {
            deployment_id,
            project_id,
            name,
        }
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
    /// The deterministic identity the resource is created under.
    #[serde(default)]
    pub identity: ResourceIdentity,
    /// Further provider resource ids / addressing needed to stop the resource
    /// (e.g. `ssh_host`, `ssh_user`, `ssh_port`, `agent_id` for self-hosted).
    /// Ids and addresses only - never secret values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resources: BTreeMap<String, String>,
    /// The resource's lifecycle state.
    #[serde(default = "default_state")]
    pub state: ProvisioningState,
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
            identity,
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
        assert!(!min.has_resource_id());
    }

    #[test]
    fn identity_is_deterministic_dns_safe_and_collision_resistant() {
        let a = ResourceIdentity::new("Dep_1", "My Project");
        assert_eq!(a, ResourceIdentity::new("Dep_1", "My Project"));
        assert!(a.name.len() <= ResourceIdentity::MAX_NAME_LEN);
        assert!(
            a.name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        assert_ne!(a.name, ResourceIdentity::new("dep-1", "my-project").name);
        let long = ResourceIdentity::new("d".repeat(200), "p".repeat(200));
        assert!(long.name.len() <= ResourceIdentity::MAX_NAME_LEN);
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
