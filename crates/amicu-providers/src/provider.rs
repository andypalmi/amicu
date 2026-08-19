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
//! - Health is inherited from [`Adapter::health`] — reachability without side
//!   effects.
//!
//! The provisioning request/handle/state types here are intentionally minimal:
//! TASK-014 fleshes out the full lifecycle state machine and credentials model
//! and extends these types. Every fallible method returns [`AdapterError`];
//! adapters never panic.

use amicu_core::adapter::{
    Adapter, AdapterError, AdapterErrorKind, AdapterId, DescribeCapabilities,
};
use amicu_core::diagnostic::{Category, DiagnosticCode, Severity, catalog};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::ProviderCapabilities;

/// `AMICU-PROVIDER-010` — the request names an offering the provider does not
/// offer.
const PROVIDER_UNKNOWN_OFFERING: DiagnosticCode = DiagnosticCode::new(Category::Provider, 10);
/// `AMICU-PROVIDER-011` — the request names a region the provider does not
/// serve.
const PROVIDER_UNKNOWN_REGION: DiagnosticCode = DiagnosticCode::new(Category::Provider, 11);
/// `AMICU-PROVIDER-012` — the handle refers to no known provisioned instance.
const PROVIDER_UNKNOWN_INSTANCE: DiagnosticCode = DiagnosticCode::new(Category::Provider, 12);
/// `AMICU-PROVIDER-013` — the provider backend failed transiently.
const PROVIDER_BACKEND: DiagnosticCode = DiagnosticCode::new(Category::Provider, 13);

/// The provisioning request handed to [`Provider::provision`].
///
/// It names *what* to provision by referencing a [`ComputeOffering`] id and a
/// [`Region`] code from the provider's [`ProviderCapabilities`]. Kept minimal
/// on purpose: TASK-014 extends it with the credentials binding, sizing, tags,
/// and deployment linkage without changing the method signature.
///
/// [`ComputeOffering`]: crate::capability::ComputeOffering
/// [`Region`]: crate::capability::Region
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionRequest {
    /// The [`ComputeOffering`](crate::capability::ComputeOffering) id to
    /// provision, e.g. `"gpu.h100.1x"`.
    pub offering_id: String,
    /// The [`Region`](crate::capability::Region) code to provision in, e.g.
    /// `"eu-central-1"`.
    pub region: String,
}

impl ProvisionRequest {
    /// A request to provision `offering_id` in `region`.
    #[must_use]
    pub fn new(offering_id: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            offering_id: offering_id.into(),
            region: region.into(),
        }
    }
}

/// The coarse lifecycle state of a provisioned instance.
///
/// Deliberately minimal — three states covering provision, ready, and teardown.
/// TASK-014 replaces this with the full lifecycle state machine (stopping,
/// hibernated, failed, ...); downstream code should treat it as extensible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProvisioningState {
    /// The instance is being brought up.
    Provisioning {
        /// Safe, secret-free progress summary.
        detail: String,
    },
    /// The instance is provisioned and ready to serve.
    Ready {
        /// Safe, secret-free readiness summary.
        detail: String,
    },
    /// The instance has been torn down.
    Terminated {
        /// Safe, secret-free summary of the teardown.
        detail: String,
    },
}

impl ProvisioningState {
    /// Whether the instance is [`ProvisioningState::Ready`].
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, ProvisioningState::Ready { .. })
    }
}

/// Identifies a provisioned instance and how it was provisioned.
///
/// Returned by [`Provider::provision`] and passed back to
/// [`Provider::teardown`]. Minimal for now (id, provider, region, offering,
/// coarse state); TASK-014 attaches addressing, resolved credentials handles,
/// and the richer lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisioningHandle {
    /// Opaque, provider-assigned instance id.
    pub id: String,
    /// The provider that provisioned the instance.
    pub provider: AdapterId,
    /// The region the instance runs in.
    pub region: String,
    /// The offering that was provisioned.
    pub offering_id: String,
    /// The instance's coarse lifecycle state.
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
}

impl AdapterErrorKind for ProviderError {
    fn code(&self) -> DiagnosticCode {
        match self {
            ProviderError::CapacityUnavailable { .. } => catalog::PROVIDER_CAPACITY_UNAVAILABLE,
            ProviderError::UnknownOffering { .. } => PROVIDER_UNKNOWN_OFFERING,
            ProviderError::UnknownRegion { .. } => PROVIDER_UNKNOWN_REGION,
            ProviderError::UnknownInstance { .. } => PROVIDER_UNKNOWN_INSTANCE,
            ProviderError::Backend { .. } => PROVIDER_BACKEND,
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

    /// Provision compute per `request`, returning a handle to the instance.
    /// Apply-time only.
    ///
    /// # Errors
    /// [`ProviderError::UnknownOffering`] / [`ProviderError::UnknownRegion`]
    /// when the request references an offering or region the provider does not
    /// have, or [`ProviderError::CapacityUnavailable`] when there is no
    /// capacity — each folded into an [`AdapterError`].
    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisioningHandle, AdapterError>;

    /// Tear down a previously provisioned instance. Apply-time only.
    ///
    /// # Errors
    /// [`ProviderError::UnknownInstance`] when `handle` refers to no known
    /// instance, folded into an [`AdapterError`].
    async fn teardown(&self, handle: &ProvisioningHandle) -> Result<(), AdapterError>;
}
