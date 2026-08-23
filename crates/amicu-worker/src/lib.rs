//! Scheduled/background jobs.
//!
//! [`CapabilitySyncWorker`] is the async driver behind the pure
//! [`amicu_providers::sync`] logic: on a fixed interval it runs one
//! capability/pricing sync over the registered providers and stores the latest
//! [`SyncReport`] for readers. The wall clock lives only at this edge - each
//! tick mints `now` from [`SystemTime`] and hands it to the pure `sync_all`, so
//! the sync logic stays deterministic and testable.
//!
//! The infinite interval loop cannot be unit-tested, so the one-shot
//! [`tick`](CapabilitySyncWorker::tick) is public and the loop merely calls it.

use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use amicu_core::adapter::Registry;
use amicu_providers::provider::Provider;
use amicu_providers::sync::{self, SyncReport};

/// Periodically syncs provider capability/pricing metadata into a shared store.
///
/// Cloning is cheap and shares the same store: clone a reader handle before
/// spawning [`run`](Self::run) (which consumes `self`) to keep calling
/// [`latest`](Self::latest) while the loop runs.
#[derive(Clone)]
pub struct CapabilitySyncWorker {
    registry: Arc<Registry<dyn Provider>>,
    latest: Arc<RwLock<Option<SyncReport>>>,
}

impl CapabilitySyncWorker {
    /// A worker syncing the providers in `registry`, with no report yet.
    #[must_use]
    pub fn new(registry: Arc<Registry<dyn Provider>>) -> Self {
        Self {
            registry,
            latest: Arc::new(RwLock::new(None)),
        }
    }

    /// The most recent [`SyncReport`], or `None` before the first tick.
    #[must_use]
    pub fn latest(&self) -> Option<SyncReport> {
        self.latest
            .read()
            .expect("sync store lock poisoned")
            .clone()
    }

    /// Run one sync-and-store pass, minting `now` from the wall clock, and
    /// return the freshly stored report.
    pub async fn tick(&self) -> SyncReport {
        let report = sync::sync_all(&self.registry, now_epoch_secs()).await;
        *self.latest.write().expect("sync store lock poisoned") = Some(report.clone());
        report
    }

    /// Drive the sync on a fixed interval forever, ticking immediately and then
    /// every `period`. Runs until the task is dropped/aborted.
    pub async fn run(self, period: Duration) -> ! {
        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            self.tick().await;
        }
    }
}

/// Current wall-clock time as UNIX epoch seconds (clamped to 0 before the
/// epoch). The sole point where wall-clock time enters the sync path.
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::adapter::{AdapterId, HealthStatus};
    use amicu_providers::FakeProvider;
    use amicu_providers::sync::Freshness;

    #[tokio::test]
    async fn tick_stores_a_retrievable_report() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(FakeProvider::new("healthy-cloud")));
        registry.register(Arc::new(FakeProvider::new("down-cloud").with_health(
            HealthStatus::Unavailable {
                detail: "unreachable".into(),
            },
        )));
        let worker = CapabilitySyncWorker::new(Arc::new(registry));

        assert!(worker.latest().is_none());

        let ticked = worker.tick().await;
        let stored = worker.latest().expect("report stored after tick");
        assert_eq!(ticked, stored);
        assert_eq!(stored.entries.len(), 2);

        let down = stored
            .entries
            .iter()
            .find(|e| e.provider == AdapterId::new("down-cloud"))
            .unwrap();
        assert_eq!(down.freshness, Freshness::Unavailable);
        assert!(down.capabilities.is_none());
        assert_eq!(stored.diagnostics.len(), 1);
    }

    #[tokio::test]
    async fn clone_shares_the_store() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(FakeProvider::new("healthy-cloud")));
        let worker = CapabilitySyncWorker::new(Arc::new(registry));
        let reader = worker.clone();

        worker.tick().await;
        // The clone observes the report the original stored - the store is shared.
        assert_eq!(reader.latest().expect("shared report").entries.len(), 1);
    }
}
