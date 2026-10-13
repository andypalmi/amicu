//! The state backend adapter family trait.

use amicu_core::Graph;
use amicu_core::adapter::{Adapter, AdapterError, DescribeCapabilities};
use async_trait::async_trait;

use crate::drift::{self, DriftReport};
use crate::lock::LockHandle;
use crate::runtime::DeploymentRecord;
use crate::snapshot::{StateSnapshot, StateVersion};
use amicu_core::id::DeploymentId;

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

    /// Extend the lease on the held `lock`, so an operation longer than the
    /// backend's lease TTL keeps exclusive access.
    ///
    /// Like [`push`](Self::push), implementations MUST verify that `lock` is the
    /// lock currently guarding the state *and* that its lease has not already
    /// expired (judged by the backend's own clock): an expired lease cannot be
    /// resurrected by renewing it, because another holder may already have
    /// taken over. Failure is [`StateError::LockNotHeld`](crate::StateError::LockNotHeld).
    /// See [`LeaseKeeper`](crate::LeaseKeeper) for periodic renewal.
    async fn renew(&self, lock: &LockHandle) -> Result<(), AdapterError>;

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

    /// Persist (insert or replace) one deployment's runtime record under the
    /// held `lock`, leaving the graph and every other record untouched.
    ///
    /// This is the per-resource write-ahead primitive: record a
    /// [`Pending`](crate::DeploymentPhase::Pending) record with its
    /// [`StopIdentity`](crate::StopIdentity) *before* creating the resource,
    /// then replace it with the `Running` record once the handle is known. It
    /// is a read-modify-write (`pull` then `push`) made atomic by the lock: no
    /// other writer can interleave while the lease is live, and `push`
    /// re-verifies the lease, so an expired lease fails the write. Each call
    /// records a new, append-only snapshot version.
    async fn put_deployment_record(
        &self,
        record: DeploymentRecord,
        lock: &LockHandle,
    ) -> Result<(), AdapterError> {
        let current = self.pull().await?;
        let mut runtime = current.runtime;
        runtime.upsert(record);
        let next = StateSnapshot::with_runtime(
            current.version,
            current.recorded_at,
            current.graph,
            runtime,
        )?;
        self.push(&next, lock).await
    }

    /// Drop one deployment's runtime record under the held `lock`. Returns
    /// whether a record was present.
    ///
    /// Call only after the stop is confirmed: a deployment leaves state only
    /// once its resource is known to be gone.
    async fn remove_deployment_record(
        &self,
        deployment: &DeploymentId,
        lock: &LockHandle,
    ) -> Result<bool, AdapterError> {
        let current = self.pull().await?;
        let mut runtime = current.runtime;
        if runtime.remove(deployment).is_none() {
            return Ok(false);
        }
        let next = StateSnapshot::with_runtime(
            current.version,
            current.recorded_at,
            current.graph,
            runtime,
        )?;
        self.push(&next, lock).await?;
        Ok(true)
    }
}
