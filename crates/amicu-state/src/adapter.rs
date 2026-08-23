//! The state backend adapter family trait.

use amicu_core::Graph;
use amicu_core::adapter::{Adapter, AdapterError, DescribeCapabilities};
use async_trait::async_trait;

use crate::drift::{self, DriftReport};
use crate::lock::LockHandle;
use crate::snapshot::{StateSnapshot, StateVersion};

/// A backend that stores canonical state and mediates concurrent access to it.
///
/// The contract makes two failure modes explicit and non-silent:
///
/// - **Locking.** [`lock`](Self::lock) under contention returns an
///   `AMICU-STATE-*` error instead of blocking forever, and the mutating
///   operations ([`push`](Self::push), [`rollback`](Self::rollback)) require the
///   [`LockHandle`] it hands back. An implementation must reject a call whose
///   handle does not match the held lock rather than overwrite the state, and
///   MUST mint lock tokens from an unpredictable (random) source so a forged
///   handle cannot satisfy the ownership check (see [`LockHandle`]).
/// - **Corruption.** When recorded state fails its integrity check, methods
///   return a non-retryable corruption error demanding manual recovery; the
///   backend must never silently overwrite corrupt state.
///
/// [`detect_drift`](Self::detect_drift) is a pure synchronous diff and carries a
/// default implementation every backend shares.
#[async_trait]
pub trait StateBackendAdapter: Adapter + DescribeCapabilities {
    /// Read the current recorded snapshot.
    async fn pull(&self) -> Result<StateSnapshot, AdapterError>;

    /// Record `snapshot`, authorized by the held `lock`.
    ///
    /// Implementations MUST reject the push when `lock` does not match the lock
    /// currently guarding the state - the state is never overwritten without
    /// its lock.
    async fn push(&self, snapshot: &StateSnapshot, lock: &LockHandle) -> Result<(), AdapterError>;

    /// Acquire the state lock, returning a [`LockHandle`] proving ownership.
    ///
    /// Under contention this returns an `AMICU-STATE-*` error rather than
    /// waiting indefinitely.
    async fn lock(&self) -> Result<LockHandle, AdapterError>;

    /// Release the lock identified by `handle`.
    async fn unlock(&self, handle: LockHandle) -> Result<(), AdapterError>;

    /// List recorded versions, oldest first.
    async fn history(&self) -> Result<Vec<StateVersion>, AdapterError>;

    /// Roll the recorded state back to `version`, authorized by the held `lock`.
    ///
    /// Like [`push`](Self::push), this mutates recorded state, so implementations
    /// MUST reject the rollback when `lock` does not match the lock currently
    /// guarding the state - a rollback can never race a concurrent push.
    async fn rollback(&self, version: &StateVersion, lock: &LockHandle)
    -> Result<(), AdapterError>;

    /// Diff `desired` against the graph recorded in `actual`. Pure: no locks,
    /// no I/O, no side effects.
    fn detect_drift(&self, desired: &Graph, actual: &StateSnapshot) -> DriftReport {
        drift::detect_drift(desired, actual)
    }
}
