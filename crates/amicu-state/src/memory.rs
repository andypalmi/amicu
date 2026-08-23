//! In-memory [`StateBackendAdapter`]: the dev/single-node fallback used when
//! no durable backend is configured.
//!
//! This mirrors the role [`InMemoryUserStore`](https://docs.rs/amicu-auth)
//! (and friends) plays for the auth stores, and
//! [`InMemoryConfigStore`](https://docs.rs/amicu-store) plays for config: a
//! zero-configuration, process-local implementation of the trait so local
//! runs, tests, and pre-database self-host work without a database. All
//! state is held in an in-process [`Mutex`] and is lost on restart - this is
//! never appropriate for a real deployment managing real infrastructure; see
//! [`PostgresStateBackend`](https://docs.rs/amicu-store) for the durable,
//! production implementation selected when `DATABASE_URL` is set.
//!
//! Semantics deliberately match [`PostgresStateBackend`](https://docs.rs/amicu-store)
//! so callers cannot observe which backend they are talking to: a
//! fresh backend with nothing ever pushed reports an empty, unpersisted `v0`
//! graph from [`pull`](StateBackendAdapter::pull) rather than an error, and
//! `push`/`rollback` reject any lock that does not match the one currently
//! held.

use std::collections::HashMap;
use std::sync::Mutex;

use amicu_core::Graph;
use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use async_trait::async_trait;
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

use crate::adapter::StateBackendAdapter;
use crate::error::StateError;
use crate::lock::LockHandle;
use crate::snapshot::{StateSnapshot, StateVersion};

/// The version id and (empty) content a backend with no recorded state
/// reports from [`InMemoryStateBackend::pull`]. Nothing is persisted for it -
/// matches [`PostgresStateBackend`](https://docs.rs/amicu-store)'s "no state
/// yet" convention.
const EMPTY_VERSION: &str = "v0";
const EMPTY_RECORDED_AT: &str = "";

/// Plain-data capability descriptor for [`DescribeCapabilities::describe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InMemoryCapabilities {
    backend: &'static str,
    versioned_history: bool,
    supports_rollback: bool,
}

struct Inner {
    current: Option<StateSnapshot>,
    history: Vec<StateVersion>,
    snapshots: HashMap<String, StateSnapshot>,
    lock: Option<u64>,
    next_version: u64,
    corrupt: bool,
}

/// Dev/single-node in-memory [`StateBackendAdapter`]. Zero configuration, all
/// state lost on process restart - safe for local runs, tests, and
/// pre-database self-host, never a production deployment holding real
/// infrastructure state.
pub struct InMemoryStateBackend {
    id: AdapterId,
    inner: Mutex<Inner>,
}

impl InMemoryStateBackend {
    /// A fresh backend with no recorded state: [`pull`](StateBackendAdapter::pull)
    /// reports an empty, unpersisted `v0` graph until the first
    /// [`push`](StateBackendAdapter::push).
    #[must_use]
    pub fn new(id: impl Into<AdapterId>) -> Self {
        Self {
            id: id.into(),
            inner: Mutex::new(Inner {
                current: None,
                history: Vec::new(),
                snapshots: HashMap::new(),
                lock: None,
                next_version: 0,
                corrupt: false,
            }),
        }
    }

    /// A backend pre-seeded with `initial` as its current, recorded state
    /// (also its sole history entry). Test-only seam so contract tests can
    /// exercise pull/drift/rollback against a known starting graph without
    /// first going through `lock` + `push`; not part of the
    /// [`StateBackendAdapter`] contract.
    #[doc(hidden)]
    #[must_use]
    pub fn with_initial_snapshot_for_test(
        id: impl Into<AdapterId>,
        initial: StateSnapshot,
    ) -> Self {
        let version = initial.version.clone();
        let mut snapshots = HashMap::new();
        snapshots.insert(version.clone(), initial.clone());
        Self {
            id: id.into(),
            inner: Mutex::new(Inner {
                current: Some(initial),
                history: vec![StateVersion::new(version, None, "initial")],
                snapshots,
                lock: None,
                next_version: 1,
                corrupt: false,
            }),
        }
    }

    /// Test-only seam to simulate a corrupted store. Not part of the
    /// [`StateBackendAdapter`] contract.
    #[doc(hidden)]
    pub fn set_corrupt_for_test(&self) {
        self.inner.lock().expect("state backend poisoned").corrupt = true;
    }
}

#[async_trait]
impl Adapter for InMemoryStateBackend {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(HealthStatus::Healthy)
    }
}

impl DescribeCapabilities for InMemoryStateBackend {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(InMemoryCapabilities {
            backend: "memory",
            versioned_history: true,
            supports_rollback: true,
        })
        .unwrap_or_default()
    }
}

#[async_trait]
impl StateBackendAdapter for InMemoryStateBackend {
    async fn pull(&self) -> Result<StateSnapshot, AdapterError> {
        let inner = self.inner.lock().expect("state backend poisoned");
        if inner.corrupt {
            return Err(StateError::Corrupted {
                detail: "backend flagged the store as corrupt".into(),
            }
            .into());
        }
        match &inner.current {
            Some(snapshot) => {
                snapshot.verify()?;
                Ok(snapshot.clone())
            }
            // No state has ever been pushed: report a fresh, unpersisted
            // empty graph rather than an error (see module docs).
            None => Ok(StateSnapshot::new(
                EMPTY_VERSION,
                EMPTY_RECORDED_AT,
                Graph::new(),
            )?),
        }
    }

    async fn push(&self, snapshot: &StateSnapshot, lock: &LockHandle) -> Result<(), AdapterError> {
        let mut inner = self.inner.lock().expect("state backend poisoned");
        if inner.corrupt {
            return Err(StateError::Corrupted {
                detail: "refusing to overwrite corrupt state".into(),
            }
            .into());
        }
        // Never overwrite without the exact lock guarding the state.
        let held = inner.lock.ok_or(StateError::LockNotHeld)?;
        if lock.holder() != &self.id || lock.token() != held {
            return Err(StateError::LockNotHeld.into());
        }

        let version = format!("v{}", inner.next_version);
        inner.next_version += 1;
        let parent = inner.current.as_ref().map(|s| s.version.clone());
        let recorded = StateSnapshot::new(&version, &snapshot.recorded_at, snapshot.graph.clone())?;
        inner.snapshots.insert(version.clone(), recorded.clone());
        inner
            .history
            .push(StateVersion::new(version, parent, "push"));
        inner.current = Some(recorded);
        Ok(())
    }

    async fn lock(&self) -> Result<LockHandle, AdapterError> {
        let mut inner = self.inner.lock().expect("state backend poisoned");
        if let Some(held) = inner.lock {
            return Err(StateError::LockContention {
                holder: format!("{}#{held}", self.id),
            }
            .into());
        }
        // Minted from OsRng, never a sequential/derived counter, so a stale
        // or forged handle cannot satisfy the ownership check (see
        // `LockHandle`'s security note) - matches `PostgresStateBackend`.
        let token = OsRng.next_u64();
        inner.lock = Some(token);
        Ok(LockHandle::new(self.id.clone(), token))
    }

    async fn unlock(&self, handle: LockHandle) -> Result<(), AdapterError> {
        let mut inner = self.inner.lock().expect("state backend poisoned");
        match inner.lock {
            Some(held) if held == handle.token() && handle.holder() == &self.id => {
                inner.lock = None;
                Ok(())
            }
            _ => Err(StateError::LockNotHeld.into()),
        }
    }

    async fn history(&self) -> Result<Vec<StateVersion>, AdapterError> {
        Ok(self
            .inner
            .lock()
            .expect("state backend poisoned")
            .history
            .clone())
    }

    async fn rollback(
        &self,
        version: &StateVersion,
        lock: &LockHandle,
    ) -> Result<(), AdapterError> {
        let mut inner = self.inner.lock().expect("state backend poisoned");
        if inner.corrupt {
            return Err(StateError::Corrupted {
                detail: "refusing to roll back corrupt state".into(),
            }
            .into());
        }
        // Never roll back without the exact lock guarding the state.
        let held = inner.lock.ok_or(StateError::LockNotHeld)?;
        if lock.holder() != &self.id || lock.token() != held {
            return Err(StateError::LockNotHeld.into());
        }
        let target = inner.snapshots.get(&version.id).cloned().ok_or_else(|| {
            StateError::VersionNotFound {
                version: version.id.clone(),
            }
        })?;
        inner.current = Some(target);
        Ok(())
    }
}
