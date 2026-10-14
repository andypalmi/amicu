//! The state backend adapter family trait.

use amicu_core::Graph;
use amicu_core::adapter::{Adapter, AdapterError, DescribeCapabilities};
use async_trait::async_trait;

use crate::drift::{self, DriftReport};
use crate::lock::LockHandle;
use crate::runtime::{ConfirmedStop, DeploymentPhase, DeploymentRecord, RuntimeState};
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

    /// Record `snapshot`'s graph, authorized by the held `lock`.
    ///
    /// A push is **graph-only with respect to runtime records**:
    /// `snapshot.runtime` is ignored and the records of the current version are
    /// carried forward unchanged, so a plain graph push can never drop a
    /// deployment's stop identity. Records change only through
    /// [`replace_runtime`](Self::replace_runtime) (or the per-deployment
    /// helpers built on it).
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

    /// Roll the **graph** back to `version`, authorized by the held `lock`.
    ///
    /// Runtime records are never rolled back: the rollback appends a new
    /// version made of `version`'s graph and the *current* records, because an
    /// old version's records describe resources that may since have been
    /// started or stopped.
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

    /// Append a new version with the current graph and exactly `runtime` as its
    /// records, authorized by the held `lock` (same checks as
    /// [`push`](Self::push): holder, token, unexpired lease).
    ///
    /// This is the only path that changes runtime records wholesale; prefer
    /// [`put_deployment_record`](Self::put_deployment_record) and
    /// [`remove_deployment_record`](Self::remove_deployment_record).
    async fn replace_runtime(
        &self,
        runtime: &RuntimeState,
        recorded_at: &str,
        lock: &LockHandle,
    ) -> Result<(), AdapterError>;

    /// Persist (insert or replace) one deployment's runtime record under the
    /// held `lock`, leaving the graph and every other record untouched.
    ///
    /// This is the per-resource write-ahead primitive: record a
    /// [`Pending`](crate::DeploymentPhase::Pending) record with its
    /// [`StopIdentity`](crate::StopIdentity) *before* creating the resource,
    /// then replace it with the `Running` record once the handle is known. It
    /// is a read-modify-write (`pull` then
    /// [`replace_runtime`](Self::replace_runtime)) that is **safe only while the
    /// caller holds a live lease** (ideally kept alive by a
    /// [`LeaseKeeper`](crate::LeaseKeeper)): the lock is what excludes other
    /// writers, and `replace_runtime` re-verifies it, so an expired lease fails
    /// the write. Each call records a new, append-only version stamped with a
    /// fresh `recorded_at`.
    async fn put_deployment_record(
        &self,
        record: DeploymentRecord,
        lock: &LockHandle,
    ) -> Result<(), AdapterError> {
        let mut runtime = self.pull().await?.runtime;
        runtime.upsert(record);
        self.replace_runtime(&runtime, &crate::clock::now_rfc3339(), lock)
            .await
    }

    /// Drop one deployment's runtime record under the held `lock`, once its
    /// stop is confirmed. Returns whether a record was present.
    ///
    /// A deployment leaves state only after its stop is confirmed, so this
    /// requires a [`ConfirmedStop`] for the deployment *and* that the record is
    /// in the [`Stopping`](DeploymentPhase::Stopping) phase; a `Pending` or
    /// `Running` record is refused with
    /// [`StateError::StopNotConfirmed`](crate::StateError::StopNotConfirmed).
    /// To clean up a record whose start never happened, first
    /// [`put`](Self::put_deployment_record) it as `Stopping`, stop idempotently,
    /// then remove. Same lease requirement as
    /// [`put_deployment_record`](Self::put_deployment_record).
    async fn remove_deployment_record(
        &self,
        stopped: &ConfirmedStop,
        lock: &LockHandle,
    ) -> Result<bool, AdapterError> {
        let mut runtime = self.pull().await?.runtime;
        let Some(record) = runtime.get(stopped.deployment()) else {
            return Ok(false);
        };
        if record.phase != DeploymentPhase::Stopping {
            return Err(crate::StateError::StopNotConfirmed {
                deployment: stopped.deployment().to_string(),
                detail: format!("record is still {:?}", record.phase),
            }
            .into());
        }
        runtime.remove(stopped.deployment());
        self.replace_runtime(&runtime, &crate::clock::now_rfc3339(), lock)
            .await?;
        Ok(true)
    }
}
