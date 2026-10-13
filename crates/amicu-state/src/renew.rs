//! Background lease renewal for long-running operations.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::adapter::StateBackendAdapter;
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
/// Transient (retryable) renew errors are retried on the next tick.
///
/// Must be spawned from within a Tokio runtime.
pub struct LeaseKeeper {
    lost: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl LeaseKeeper {
    /// Start renewing `lock` on `backend` every `interval`.
    #[must_use]
    pub fn spawn(
        backend: Arc<dyn StateBackendAdapter>,
        lock: LockHandle,
        interval: Duration,
    ) -> Self {
        let lost = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&lost);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if let Err(error) = backend.renew(&lock).await
                    && !error.is_retryable()
                {
                    flag.store(true, Ordering::SeqCst);
                    return;
                }
            }
        });
        Self { lost, task }
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
