//! Postgres-backed [`StateBackendAdapter`]: the first production
//! implementation of the state-backend family (amicu-planning#123).
//!
//! # Scoping
//!
//! The trait's methods (`pull`, `push`, `lock`, ...) carry no workspace/stack
//! parameter - each [`PostgresStateBackend`] instance is bound to one
//! [`WorkspaceId`] (the tenant boundary) and one [`AdapterId`] (which doubles
//! as the `stack_id`: the named state stream within the workspace this
//! instance manages, so one workspace can run several independent graphs).
//! Every query carries both predicates and runs inside a GUC-bound
//! [`begin_scoped`] transaction, so the `state_*` row-level-security policies
//! (migration `0009_state.sql`) apply.
//!
//! # History vs. current
//!
//! `state_snapshots` is append-only: [`push`](Self::push) inserts a row and
//! nothing ever updates or deletes one. The "current" version is a separate,
//! mutable pointer (`state_current`) so [`rollback`](Self::rollback) can
//! retarget it at an already-recorded version without mutating or duplicating
//! history - matching the in-memory reference implementation
//! (`amicu_state::tests::FakeStateBackend`), whose `rollback` only swaps the
//! `current` pointer.
//!
//! A `pull` before any `push` has ever run finds no `state_current` row; this
//! is treated as "no state yet" and reports a fresh, unpersisted empty graph
//! (`v0`), mirroring how IaC state backends treat a missing state file as
//! empty infrastructure rather than an error.
//!
//! `push` mints its own version id (`v{seq}`) and discards the caller-supplied
//! [`StateSnapshot::version`], exactly like the fake reference backend.
//!
//! # Locking
//!
//! [`state_locks`] holds at most one lease per (workspace, stack). Acquiring
//! is one atomic `INSERT ... ON CONFLICT DO UPDATE ... WHERE <stale>
//! RETURNING`: Postgres resolves the conflict under a row-level lock, so two
//! concurrent [`lock`](Self::lock) calls cannot both win, and a lease whose
//! `expires_at` has passed is treated as stale and re-acquirable even if never
//! explicitly [`unlock`](Self::unlock)ed (crash/timeout recovery). The token
//! is minted from [`OsRng`], never a sequential or derived counter, per the
//! anti-forgery requirement on [`LockHandle`]; it is stored as a `BIGINT` by
//! reinterpreting the `u64`'s bits as `i64` via `cast_signed` (a lossless
//! bit-pattern reinterpretation, not a range-truncating cast).
//!
//! `push` and `rollback` re-check the presented lock inside their own
//! transaction (holder, token, and non-expired) before touching any row, so a
//! stale or forged handle can never authorize a write.

use amicu_core::Graph;
use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_state::{LockHandle, StateBackendAdapter, StateError, StateSnapshot, StateVersion};
use async_trait::async_trait;
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{WorkspaceId, begin_scoped, now_unix_seconds};

/// Default lock lease TTL: long enough to cover a plan/apply cycle, short
/// enough that a crashed holder does not wedge the stack indefinitely.
const DEFAULT_LEASE_TTL_SECONDS: i64 = 300;

/// The version id and (empty) content a backend with no recorded state
/// reports from [`PostgresStateBackend::pull`]. Nothing is persisted for it.
const EMPTY_VERSION: &str = "v0";
const EMPTY_RECORDED_AT: &str = "";

/// Plain-data capability descriptor for [`DescribeCapabilities::describe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PostgresStateCapabilities {
    backend: &'static str,
    versioned_history: bool,
    supports_rollback: bool,
}

/// Postgres implementation of [`StateBackendAdapter`].
pub struct PostgresStateBackend {
    pool: PgPool,
    workspace: WorkspaceId,
    id: AdapterId,
    lease_ttl_seconds: i64,
}

impl PostgresStateBackend {
    /// A backend bound to the single-tenant default workspace, identified by
    /// `id` (also used as the `stack_id` scoping column).
    #[must_use]
    pub fn new(pool: PgPool, id: impl Into<AdapterId>) -> Self {
        Self::with_workspace(pool, WorkspaceId::DEFAULT, id)
    }

    /// A backend bound to a specific workspace and identity (multi-tenant).
    #[must_use]
    pub fn with_workspace(pool: PgPool, workspace: WorkspaceId, id: impl Into<AdapterId>) -> Self {
        Self {
            pool,
            workspace,
            id: id.into(),
            lease_ttl_seconds: DEFAULT_LEASE_TTL_SECONDS,
        }
    }

    /// Overrides the lock lease TTL (seconds). Mainly for tests that need to
    /// observe an expired lease becoming re-acquirable without waiting out
    /// the production default.
    #[must_use]
    pub fn with_lease_ttl_seconds(mut self, seconds: i64) -> Self {
        self.lease_ttl_seconds = seconds;
        self
    }

    /// The `stack_id` scoping column: this backend's own [`AdapterId`].
    fn stack_id(&self) -> &str {
        self.id.as_str()
    }

    /// Begins a GUC-bound transaction scoped to this backend's workspace.
    async fn begin_tx(&self) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, AdapterError> {
        begin_scoped(&self.pool, self.workspace)
            .await
            .map_err(|_| backend_err("failed to begin scoped transaction"))
    }

    /// Confirms `lock` matches the lease currently held for this backend's
    /// (workspace, stack) and that it has not expired. Read-only: callers
    /// still hold `tx` and can roll back cheaply on failure.
    async fn authorize(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        lock: &LockHandle,
    ) -> Result<(), AdapterError> {
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();
        let now = now_unix_seconds();
        let token_col = lock.token().cast_signed();
        let holder = lock.holder().as_str();

        let held = sqlx::query!(
            "SELECT holder, lock_token, expires_at FROM state_locks \
             WHERE workspace_id = $1 AND stack_id = $2",
            wsid,
            stack,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| backend_err("failed to read lock"))?;

        let authorized = held
            .as_ref()
            .is_some_and(|r| r.holder == holder && r.lock_token == token_col && r.expires_at > now);

        if authorized {
            Ok(())
        } else {
            Err(StateError::LockNotHeld.into())
        }
    }
}

/// Folds an opaque `sqlx::Error` into a safe, secret-free [`AdapterError`].
/// The raw driver error (which can embed bind values) is deliberately
/// dropped, mirroring `amicu_store::map_db`'s opacity for the auth stores.
fn backend_err(context: &str) -> AdapterError {
    StateError::Backend {
        detail: context.to_string(),
    }
    .into()
}

#[async_trait]
impl Adapter for PostgresStateBackend {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        // A schema-independent probe, so this never needs the sqlx offline
        // cache: it is the `query()` runtime API, not the `query!` macro.
        match sqlx::query("SELECT 1").fetch_one(&self.pool).await {
            Ok(_) => Ok(HealthStatus::Healthy),
            Err(_) => Ok(HealthStatus::Unavailable {
                detail: "postgres unreachable".to_string(),
            }),
        }
    }
}

impl DescribeCapabilities for PostgresStateBackend {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(PostgresStateCapabilities {
            backend: "postgres",
            versioned_history: true,
            supports_rollback: true,
        })
        .unwrap_or_default()
    }
}

#[async_trait]
impl StateBackendAdapter for PostgresStateBackend {
    async fn pull(&self) -> Result<StateSnapshot, AdapterError> {
        let mut tx = self.begin_tx().await?;
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();

        let row = sqlx::query!(
            r#"
            SELECT s.version, s.recorded_at, s.checksum, s.content
            FROM state_current c
            JOIN state_snapshots s
              ON s.workspace_id = c.workspace_id
             AND s.stack_id = c.stack_id
             AND s.version = c.version
            WHERE c.workspace_id = $1 AND c.stack_id = $2
            "#,
            wsid,
            stack,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to read current state"))?;
        tx.commit()
            .await
            .map_err(|_| backend_err("failed to commit read"))?;

        let Some(row) = row else {
            // No state has ever been pushed for this (workspace, stack): report
            // a fresh, unpersisted empty graph rather than an error.
            return Ok(StateSnapshot::new(
                EMPTY_VERSION,
                EMPTY_RECORDED_AT,
                Graph::new(),
            )?);
        };

        let graph: Graph =
            serde_json::from_value(row.content).map_err(|source| StateError::Corrupted {
                detail: format!("stored graph failed to deserialize: {source}"),
            })?;
        let snapshot = StateSnapshot {
            version: row.version,
            recorded_at: row.recorded_at,
            checksum: row.checksum,
            graph,
        };
        snapshot.verify()?;
        Ok(snapshot)
    }

    async fn push(&self, snapshot: &StateSnapshot, lock: &LockHandle) -> Result<(), AdapterError> {
        let mut tx = self.begin_tx().await?;
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();

        // Never overwrite without the exact, still-live lock guarding the state.
        self.authorize(&mut tx, lock).await?;

        let next = sqlx::query!(
            r#"SELECT (COALESCE(MAX(seq), -1) + 1) AS "next_seq!"
               FROM state_snapshots WHERE workspace_id = $1 AND stack_id = $2"#,
            wsid,
            stack,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to compute next version"))?;
        let seq = next.next_seq;

        let current = sqlx::query!(
            "SELECT version FROM state_current WHERE workspace_id = $1 AND stack_id = $2",
            wsid,
            stack,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to read current version"))?;
        let parent_version = current.map(|r| r.version);

        // Mint our own version id and recompute the checksum fresh, exactly
        // like the fake reference backend: the caller-supplied
        // `snapshot.version` is a placeholder and is discarded.
        let recorded = StateSnapshot::new(
            format!("v{seq}"),
            &snapshot.recorded_at,
            snapshot.graph.clone(),
        )?;
        let content =
            serde_json::to_value(&recorded.graph).map_err(|source| StateError::Corrupted {
                detail: format!("graph failed to serialize: {source}"),
            })?;
        let created_at = now_unix_seconds();

        sqlx::query!(
            "INSERT INTO state_snapshots \
             (workspace_id, stack_id, seq, version, parent_version, label, recorded_at, checksum, content, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            wsid,
            stack,
            seq,
            recorded.version,
            parent_version,
            "push",
            recorded.recorded_at,
            recorded.checksum,
            content,
            created_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to append snapshot"))?;

        sqlx::query!(
            "INSERT INTO state_current (workspace_id, stack_id, version) VALUES ($1, $2, $3) \
             ON CONFLICT (workspace_id, stack_id) DO UPDATE SET version = EXCLUDED.version",
            wsid,
            stack,
            recorded.version,
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to advance current version"))?;

        tx.commit()
            .await
            .map_err(|_| backend_err("failed to commit push"))?;
        Ok(())
    }

    async fn lock(&self) -> Result<LockHandle, AdapterError> {
        let mut tx = self.begin_tx().await?;
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();
        let now = now_unix_seconds();
        let expires_at = now + self.lease_ttl_seconds;
        // Unpredictable by construction (OS CSPRNG), never a sequential or
        // derived counter - see `LockHandle`'s anti-forgery requirement.
        let token = OsRng.next_u64();
        let token_col = token.cast_signed();
        let holder = self.id.as_str();

        // Atomic upsert: only wins the row when absent or stale (expired).
        // Postgres resolves the ON CONFLICT under a row lock, so concurrent
        // callers cannot both win.
        let acquired = sqlx::query!(
            r#"
            INSERT INTO state_locks (workspace_id, stack_id, holder, lock_token, acquired_at, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (workspace_id, stack_id) DO UPDATE
              SET holder = EXCLUDED.holder,
                  lock_token = EXCLUDED.lock_token,
                  acquired_at = EXCLUDED.acquired_at,
                  expires_at = EXCLUDED.expires_at
              WHERE state_locks.expires_at <= $5
            RETURNING lock_token
            "#,
            wsid,
            stack,
            holder,
            token_col,
            now,
            expires_at,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to acquire lock"))?;

        if acquired.is_none() {
            // Contention: read the current holder for the error message. A
            // benign race with a concurrent unlock/expiry here only affects the
            // diagnostic text, never correctness.
            let holder_row = sqlx::query!(
                "SELECT holder FROM state_locks WHERE workspace_id = $1 AND stack_id = $2",
                wsid,
                stack,
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| backend_err("failed to read lock holder"))?;
            tx.commit()
                .await
                .map_err(|_| backend_err("failed to commit"))?;
            let holder = holder_row.map_or_else(|| "unknown".to_string(), |r| r.holder);
            return Err(StateError::LockContention { holder }.into());
        }

        tx.commit()
            .await
            .map_err(|_| backend_err("failed to commit lock acquisition"))?;
        Ok(LockHandle::new(self.id.clone(), token))
    }

    async fn unlock(&self, handle: LockHandle) -> Result<(), AdapterError> {
        let mut tx = self.begin_tx().await?;
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();
        let token_col = handle.token().cast_signed();
        let holder = handle.holder().as_str();

        let result = sqlx::query!(
            "DELETE FROM state_locks \
             WHERE workspace_id = $1 AND stack_id = $2 AND holder = $3 AND lock_token = $4",
            wsid,
            stack,
            holder,
            token_col,
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to release lock"))?;

        tx.commit()
            .await
            .map_err(|_| backend_err("failed to commit unlock"))?;

        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StateError::LockNotHeld.into())
        }
    }

    async fn history(&self) -> Result<Vec<StateVersion>, AdapterError> {
        let mut tx = self.begin_tx().await?;
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();

        let rows = sqlx::query!(
            "SELECT version, parent_version, label FROM state_snapshots \
             WHERE workspace_id = $1 AND stack_id = $2 ORDER BY seq ASC",
            wsid,
            stack,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to list history"))?;
        tx.commit()
            .await
            .map_err(|_| backend_err("failed to commit"))?;

        Ok(rows
            .into_iter()
            .map(|r| StateVersion::new(r.version, r.parent_version, r.label))
            .collect())
    }

    async fn rollback(
        &self,
        version: &StateVersion,
        lock: &LockHandle,
    ) -> Result<(), AdapterError> {
        let mut tx = self.begin_tx().await?;
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();

        // Never roll back without the exact, still-live lock guarding the
        // state - a rollback can never race a concurrent push.
        self.authorize(&mut tx, lock).await?;

        let target = sqlx::query!(
            "SELECT version FROM state_snapshots \
             WHERE workspace_id = $1 AND stack_id = $2 AND version = $3",
            wsid,
            stack,
            version.id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to look up version"))?;

        if target.is_none() {
            return Err(StateError::VersionNotFound {
                version: version.id.clone(),
            }
            .into());
        }

        // Append-only: this never touches `state_snapshots`, only repoints the
        // mutable `state_current` pointer at the already-recorded version.
        sqlx::query!(
            "INSERT INTO state_current (workspace_id, stack_id, version) VALUES ($1, $2, $3) \
             ON CONFLICT (workspace_id, stack_id) DO UPDATE SET version = EXCLUDED.version",
            wsid,
            stack,
            version.id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| backend_err("failed to move current version"))?;

        tx.commit()
            .await
            .map_err(|_| backend_err("failed to commit rollback"))?;
        Ok(())
    }
}
