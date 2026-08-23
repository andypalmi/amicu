//! The common adapter contract shared by every adapter family.
//!
//! Every external integration is an adapter behind a Rust trait, held as a
//! trait object (`Arc<dyn ...>`) and selected at runtime from a typed
//! [`Registry`]. Because stable Rust `async fn` in traits is not
//! dyn-compatible, boundary traits use [`async_trait`] for object-safe async
//! dispatch.
//!
//! The contract encodes four design rules:
//!
//! - **Identity & capabilities.** Each adapter exposes an [`AdapterId`] and a
//!   plain-data, serde-serializable capability descriptor (see
//!   [`DescribeCapabilities`]) consumed without side effects.
//! - **Planning vs apply.** Read/estimate methods are pure synchronous `fn`s
//!   returning owned data ([`Planned`]) and run during `plan`; side-effecting
//!   methods are `async fn`s that run only during `apply`. The type of a
//!   method - sync-and-owned vs `async` - is the load-bearing signal.
//! - **Uniform errors.** Every method returns the concrete [`AdapterError`]
//!   carrying a stable `AMICU-<CATEGORY>-<NUMBER>` code, severity, and a
//!   `retryable` flag; adapters never panic. See [`AdapterErrorKind`] for the
//!   per-family classification bridge.
//! - **Health.** [`Adapter::health`] reports reachability without side
//!   effects.

mod error;
mod registry;

pub use error::{AdapterError, AdapterErrorKind};
pub use registry::{AdapterDescriptor, Registry};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A typed, string-backed adapter identifier used to select adapters from a
/// [`Registry`]. Serializes transparently as its underlying string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdapterId(pub String);

impl AdapterId {
    /// Construct an id from anything string-like.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for AdapterId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for AdapterId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl std::fmt::Display for AdapterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reachability of an adapter's backend, reported without side effects.
///
/// Details are safe, human-readable summaries and must never contain secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HealthStatus {
    /// The backend is reachable and serving normally.
    Healthy,
    /// The backend is reachable but impaired.
    Degraded {
        /// Safe summary of the impairment.
        detail: String,
    },
    /// The backend is unreachable.
    Unavailable {
        /// Safe summary of why it is unreachable.
        detail: String,
    },
}

impl HealthStatus {
    /// Whether the status is [`HealthStatus::Healthy`].
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        matches!(self, HealthStatus::Healthy)
    }
}

/// Owned output of a pure planning method.
///
/// Wrapping planning results in `Planned` documents, at the type level, that a
/// value was computed during `plan` without side effects and is safe to
/// inspect before any `apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Planned<T>(pub T);

impl<T> Planned<T> {
    /// Wrap a value as a planning result.
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Unwrap the planned value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

/// Exposes an adapter's plain-data capability descriptor for discovery.
///
/// Implemented alongside a family trait so [`Registry::discover`] can list
/// capabilities uniformly across families. The descriptor is a serde value
/// computed without side effects.
pub trait DescribeCapabilities {
    /// A serde-serialized snapshot of this adapter's capabilities.
    fn describe(&self) -> serde_json::Value;
}

/// The base trait every adapter implements.
///
/// Family traits (runtime, provider, secret manager, ...) extend this with
/// their own planning and apply methods. Held as `Arc<dyn Adapter>` (or
/// `Arc<dyn FamilyAdapter>`) and dispatched dynamically.
#[async_trait]
pub trait Adapter: Send + Sync {
    /// The adapter's stable identity, used as its registry key.
    fn id(&self) -> AdapterId;

    /// Report backend reachability without side effects.
    async fn health(&self) -> Result<HealthStatus, AdapterError>;
}

#[cfg(test)]
mod tests;
