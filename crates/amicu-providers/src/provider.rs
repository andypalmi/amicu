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
//! [`SecretValue`]: amicu_core::secret::SecretValue

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

/// Identifies a provisioned instance and how it was provisioned.
///
/// Returned by [`Provider::provision`] and passed back to
/// [`Provider::status`] / [`Provider::teardown`]. It carries the credential by
/// [`SecretRef`] (name only) so later operations can re-authenticate; it never
/// stores the resolved [`SecretValue`](amicu_core::secret::SecretValue), so it
/// is safe to serialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisioningHandle {
    /// Opaque, provider-assigned instance id.
    pub id: String,
    /// The provider that provisioned the instance.
    pub provider: AdapterId,
    /// The region the instance runs in.
    pub region: String,
    /// The zone (AZ) the instance was placed in, when the adapter resolved one.
    /// Carried so later zone-addressed `status`/`teardown` calls target the same
    /// zone; `None` for region-addressed providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// The offering that was provisioned.
    pub offering_id: String,
    /// Reference to the credential used for backend operations (name only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<SecretRef>,
    /// The instance's lifecycle state.
    pub state: ProvisioningState,
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

    /// Provision compute per `request`, driving the instance
    /// `Requested` → `Provisioning` → `Ready` and returning its handle.
    /// Apply-time only.
    ///
    /// `resolver` turns `request.credentials` into a
    /// [`SecretValue`](amicu_core::secret::SecretValue) transiently, to
    /// authenticate the backend call. The value is used and dropped here; it is
    /// never placed on the returned handle nor logged.
    ///
    /// # Errors
    /// [`ProviderError::UnknownOffering`] / [`ProviderError::UnknownRegion`]
    /// when the request references an offering or region the provider does not
    /// have; [`ProviderError::CapacityUnavailable`] when there is no capacity;
    /// [`ProviderError::CredentialsRejected`] when the credential is rejected;
    /// or the resolver's error (an `AMICU-POLICY-*` code) when the referenced
    /// secret is unavailable - each folded into an [`AdapterError`].
    async fn provision(
        &self,
        request: &ProvisionRequest,
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

    /// Tear down a previously provisioned instance, driving it
    /// `Stopping` → `Terminated`. Apply-time only; re-authenticates via
    /// `resolver`.
    ///
    /// # Errors
    /// [`ProviderError::UnknownInstance`] when `handle` refers to no known
    /// instance, or the resolver's error when the credential is unavailable -
    /// folded into an [`AdapterError`].
    async fn teardown(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<(), AdapterError>;
}

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

        let handle = ProvisioningHandle {
            id: "srv-1".into(),
            provider: AdapterId::new("scaleway"),
            region: "fr-par".into(),
            zone: None,
            offering_id: "gpu.h100.1x".into(),
            credentials: None,
            state: ProvisioningState::Requested,
        };
        assert!(!serde_json::to_string(&handle).unwrap().contains("zone"));
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
