//! Background lease renewal for long-running operations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use crate::adapter::StateBackendAdapter;
use crate::error::StateError;
use crate::lock::LockHandle;

/// Keeps a held state lock alive by renewing its lease on an interval until
/// dropped.
///
/// A server-driven apply can outlive the backend's lease TTL. Spawn a
/// `LeaseKeeper` right after [`lock`](StateBackendAdapter::lock) and keep it
/// alive for the whole operation; drop it (before
/// [`unlock`](StateBackendAdapter::unlock)) when done. Pick an `interval`
/// comfortably below the TTL (a third of it is a good default).
///
/// Renewal failure is never swallowed: if the backend reports the lease is no
/// longer held (expired, or taken over), the keeper stops and
/// [`is_lost`](Self::is_lost) turns `true`. The operation should check it
/// before irreversible steps; independently, any `push` made without a live
/// lease is rejected by the backend, so a lost lease can never write state.
/// Transient (retryable) renew errors are retried on the next tick, but only
/// while the lease could still be live: once `ttl` has passed since the last
/// successful renewal the keeper declares the lease lost rather than retrying
/// forever. A panic in the task also marks the lease lost.
///
/// Must be spawned from within a Tokio runtime.
pub struct LeaseKeeper {
    lost: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl LeaseKeeper {
    /// Start renewing `lock` on `backend` every `interval`, for a lease of
    /// length `ttl` (the backend's lease TTL).
    ///
    /// # Errors
    /// Rejects a zero `interval` and any `interval >= ttl` (the lease would
    /// lapse between renewals) with [`StateError::Backend`].
    pub fn spawn(
        backend: Arc<dyn StateBackendAdapter>,
        lock: LockHandle,
        interval: Duration,
        ttl: Duration,
    ) -> Result<Self, StateError> {
        if interval.is_zero() || interval >= ttl {
            return Err(StateError::Backend {
                detail: "lease renewal interval must be non-zero and shorter than the lease ttl"
                    .into(),
            });
        }
        let lost = Arc::new(AtomicBool::new(false));
        let guard = LostOnExit(Arc::clone(&lost));
        let task = tokio::spawn(async move {
            // Sets `lost` however this task ends: definitive failure, expiry
            // of the retry window, or a panic.
            let _guard = guard;
            let mut last_ok = Instant::now();
            loop {
                tokio::time::sleep(interval).await;
                let attempt = Instant::now();
                match backend.renew(&lock).await {
                    Ok(()) => last_ok = attempt,
                    Err(error) if error.is_retryable() && last_ok.elapsed() < ttl => {}
                    Err(_) => return,
                }
            }
        });
        Ok(Self { lost, task })
    }

    /// Whether renewal definitively failed: the lease is no longer ours.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Marks the lease lost when the renewal task ends for any reason.
struct LostOnExit(Arc<AtomicBool>);

impl Drop for LostOnExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
