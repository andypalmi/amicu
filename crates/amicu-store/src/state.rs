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
//! stale or forged handle can never authorize a write. That check
//! (`authorize`) takes the `state_locks` row with `SELECT ... FOR UPDATE`,
//! held until the enclosing transaction commits or rolls back: a concurrent
//! `lock()` targeting the same row (its `INSERT ... ON CONFLICT DO UPDATE`)
//! must acquire that same row lock to proceed, so it blocks until the
//! authorizing push/rollback finishes. Without this, a plain `SELECT` would
//! let a lease that expires *during* an open push/rollback transaction be
//! legitimately reacquired and pushed by another holder while the first
//! transaction is still in flight; when it later resumed and committed (still
//! under READ COMMITTED, so its later statements see the interim commit) it
//! could silently rewrite `state_current` back over the second holder's
//! change. The row lock closes that window: a stale lease cannot be
//! reacquired by anyone else until the transaction that authorized against it
//! has fully finished.
//!
//! Lease staleness is judged by Postgres's own clock (`clock_timestamp()`),
//! not each caller's local wall clock, so multi-instance clock skew cannot
//! let a fast-clocked node treat a still-live lease as expired.
//!
//! # Encryption at rest
//!
//! [`PostgresStateBackend`] accepts an optional
//! [`StateCipher`](amicu_state::StateCipher) (via
//! [`with_cipher`](PostgresStateBackend::with_cipher)), applied at the exact
//! point a snapshot's `content` crosses the persistence boundary:
//! `push_impl` encrypts the graph's canonical JSON before the `INSERT`, and
//! `pull` decrypts it after the `SELECT`. See [`encode_content`]/
//! [`decode_content`] for the on-disk wrapper format, chosen so `content`
//! stays `JSONB` with no migration - encrypted and plaintext rows are told
//! apart by a marker key, never by a schema difference.
//!
//! With no cipher configured - the default, and every self-host deployment -
//! this backend never calls into the seam at all, so `content` is written
//! and read exactly as it always has been. **Self-host operators are
//! responsible for their own database's encryption at rest** (disk/volume
//! encryption, filesystem encryption, or their Postgres provider's own
//! offering); this crate does not provide it. The seam exists so a
//! SaaS-hosted deployment can plug in envelope/KMS encryption (per-workspace
//! data keys) without touching this self-host path - see
//! `amicu_state::cipher`'s module docs for the full scope, including why
//! this protects control-plane metadata and secret *references*, never
//! plaintext secret material (state can never carry a `SecretValue`, only a
//! `SecretRef` pointer).

use amicu_core::Graph;
use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_state::{
    LockHandle, StateBackendAdapter, StateCipher, StateError, StateSnapshot, StateVersion,
};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

use crate::{WorkspaceId, begin_scoped, now_unix_seconds};

/// Marker key present only on the JSON wrapper [`encode_content`] writes
/// when a [`StateCipher`] is configured. Its presence, not any schema/column
/// difference, is what tells encrypted content apart from a plain graph on
/// read - chosen to never collide with a real `Graph` field name.
const CIPHER_MARKER_KEY: &str = "__amicu_state_ciphertext__";

/// The wrapper format version [`encode_content`] currently writes.
const CIPHER_WRAPPER_VERSION: u8 = 1;

/// The wrapper [`encode_content`] writes into `state_snapshots.content` in
/// place of the plain graph JSON when a [`StateCipher`] is configured.
/// `content` remains `JSONB` either way; only the JSON value inside it
/// changes shape, so no column type or query change is needed to add
/// encryption.
#[derive(Debug, Serialize, Deserialize)]
struct EncryptedContent {
    /// Wrapper format version, so a future incompatible change is
    /// detectable rather than silently misread.
    #[serde(rename = "__amicu_state_cipher_v__")]
    version: u8,
    /// Base64 (standard, padded) encoding of the ciphertext the configured
    /// [`StateCipher`] produced. Its key ([`CIPHER_MARKER_KEY`]) is the
    /// marker that identifies this JSON value as wrapped ciphertext rather
    /// than a plain graph.
    #[serde(rename = "__amicu_state_ciphertext__")]
    ciphertext: String,
}

/// Serializes `graph` to the JSON value stored in `state_snapshots.content`,
/// transparently encrypting it through `cipher` when one is configured.
///
/// With `cipher: None` (the default, self-host path) this returns exactly
/// `serde_json::to_value(graph)` - byte-for-byte what every version of this
/// backend before the encryption seam wrote, so existing rows and tests are
/// unaffected.
async fn encode_content(
    graph: &Graph,
    cipher: Option<&Arc<dyn StateCipher>>,
) -> Result<serde_json::Value, AdapterError> {
    let Some(cipher) = cipher else {
        return serde_json::to_value(graph).map_err(|source| {
            backend_state_err(StateError::Corrupted {
                detail: format!("graph failed to serialize: {source}"),
            })
        });
    };

    let plaintext = serde_json::to_vec(graph).map_err(|source| {
        backend_state_err(StateError::Corrupted {
            detail: format!("graph failed to serialize: {source}"),
        })
    })?;
    let ciphertext = cipher
        .encrypt(&plaintext)
        .await
        .map_err(backend_state_err)?;
    let wrapper = EncryptedContent {
        version: CIPHER_WRAPPER_VERSION,
        ciphertext: BASE64.encode(ciphertext),
    };
    serde_json::to_value(wrapper).map_err(|source| {
        backend_state_err(StateError::Corrupted {
            detail: format!("encrypted content wrapper failed to serialize: {source}"),
        })
    })
}

/// Recovers the [`Graph`] from a `state_snapshots.content` value, decrypting
/// it through `cipher` first when it carries [`CIPHER_MARKER_KEY`] (i.e. was
/// written by [`encode_content`] with a cipher configured).
///
/// A row with no marker is read as a plain graph regardless of whether a
/// cipher is configured now, so pre-encryption history stays readable after
/// a cipher is turned on. A row that *is* wrapped but no cipher is
/// configured to decrypt it is reported as [`StateError::Corrupted`] -
/// explicit, non-retryable failure, never a silent misread.
async fn decode_content(
    content: serde_json::Value,
    cipher: Option<&Arc<dyn StateCipher>>,
) -> Result<Graph, AdapterError> {
    if !content
        .as_object()
        .is_some_and(|obj| obj.contains_key(CIPHER_MARKER_KEY))
    {
        return serde_json::from_value(content).map_err(|source| {
            backend_state_err(StateError::Corrupted {
                detail: format!("stored graph failed to deserialize: {source}"),
            })
        });
    }

    let Some(cipher) = cipher else {
        return Err(backend_state_err(StateError::Corrupted {
            detail: "snapshot content is encrypted but no StateCipher is configured".to_string(),
        }));
    };

    let wrapper: EncryptedContent = serde_json::from_value(content).map_err(|source| {
        backend_state_err(StateError::Corrupted {
            detail: format!("encrypted content wrapper failed to deserialize: {source}"),
        })
    })?;
    if wrapper.version != CIPHER_WRAPPER_VERSION {
        return Err(backend_state_err(StateError::Corrupted {
            detail: format!(
                "encrypted content wrapper version {} is not supported (expected {})",
                wrapper.version, CIPHER_WRAPPER_VERSION
            ),
        }));
    }
    let ciphertext = BASE64.decode(wrapper.ciphertext).map_err(|source| {
        backend_state_err(StateError::Corrupted {
            detail: format!("encrypted content ciphertext is not valid base64: {source}"),
        })
    })?;
    let plaintext = cipher
        .decrypt(&ciphertext)
        .await
        .map_err(backend_state_err)?;
    serde_json::from_slice(&plaintext).map_err(|source| {
        backend_state_err(StateError::Corrupted {
            detail: format!("decrypted graph failed to deserialize: {source}"),
        })
    })
}

/// Folds a [`StateError`] into an [`AdapterError`], for use from the
/// (de)serialization/encryption helpers above.
fn backend_state_err(error: StateError) -> AdapterError {
    error.into()
}

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
    /// Optional encryption-at-rest seam for `state_snapshots.content` - see
    /// the module docs' "Encryption at rest" section. `None` (the default)
    /// means this backend never touches the seam: content is written and
    /// read exactly as it always has been.
    cipher: Option<Arc<dyn StateCipher>>,
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
            cipher: None,
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

    /// Configures the encryption-at-rest seam: `push`/`pull` transparently
    /// encrypt/decrypt `state_snapshots.content` through `cipher` (see the
    /// module docs' "Encryption at rest" section). Not called by self-host
    /// deployments, which get the no-op default (`cipher: None`) instead.
    #[must_use]
    pub fn with_cipher(mut self, cipher: Arc<dyn StateCipher>) -> Self {
        self.cipher = Some(cipher);
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
    /// (workspace, stack) and that it has not expired, and holds a row lock
    /// on it (`FOR UPDATE`) through the rest of the caller's transaction.
    ///
    /// The row lock is load-bearing, not incidental: without it, a concurrent
    /// `lock()` could reacquire this exact lease (once genuinely stale) and
    /// complete its own push/rollback and commit *while this transaction is
    /// still open*, and this transaction's later statements (under READ
    /// COMMITTED) would then observe that interim commit and could silently
    /// clobber it. Taking the lock here forces any concurrent `lock()` on the
    /// same row to block until this transaction ends, closing that window.
    async fn authorize(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        lock: &LockHandle,
    ) -> Result<(), AdapterError> {
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();
        let token_col = lock.token().cast_signed();
        let holder = lock.holder().as_str();

        // `FOR UPDATE` blocks a concurrent `lock()` targeting the same row
        // until this transaction commits or rolls back (see the doc comment
        // above). Staleness is judged by Postgres's own clock
        // (`clock_timestamp()`), not this process's local clock, so
        // multi-instance clock skew cannot matter here.
        let held = sqlx::query!(
            r#"SELECT holder, lock_token, expires_at,
                      extract(epoch FROM clock_timestamp())::bigint AS "now!"
               FROM state_locks WHERE workspace_id = $1 AND stack_id = $2
               FOR UPDATE"#,
            wsid,
            stack,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| backend_err("failed to read lock"))?;

        let authorized = held.as_ref().is_some_and(|r| {
            r.holder == holder && r.lock_token == token_col && r.expires_at > r.now
        });

        if authorized {
            Ok(())
        } else {
            Err(StateError::LockNotHeld.into())
        }
    }

    /// Core of [`push`](StateBackendAdapter::push): authorizes `lock`,
    /// appends a snapshot, and advances the current-version pointer, all in
    /// one transaction. `delay`, slept immediately after authorizing (with
    /// that transaction's `state_locks` row lock still held), lets a test
    /// deterministically hold the transaction open across a concurrent
    /// `lock()` attempt from another handle. Production callers always pass
    /// [`Duration::ZERO`], which never awaits.
    async fn push_impl(
        &self,
        snapshot: &StateSnapshot,
        lock: &LockHandle,
        delay: Duration,
    ) -> Result<(), AdapterError> {
        let mut tx = self.begin_tx().await?;
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();

        // Never overwrite without the exact, still-live lock guarding the state.
        self.authorize(&mut tx, lock).await?;

        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

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
        let content = encode_content(&recorded.graph, self.cipher.as_ref()).await?;
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

    /// Test-only seam over [`push_impl`](Self::push_impl), exposing its
    /// injectable delay. Not part of the [`StateBackendAdapter`] contract;
    /// exists solely so the lock/TOCTOU regression test can deterministically
    /// hold a push transaction open across a concurrent `lock()` from another
    /// handle. `#[doc(hidden)]` because it is not meant for production use.
    #[doc(hidden)]
    pub async fn push_with_delay_for_test(
        &self,
        snapshot: &StateSnapshot,
        lock: &LockHandle,
        delay: Duration,
    ) -> Result<(), AdapterError> {
        self.push_impl(snapshot, lock, delay).await
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

        let graph = decode_content(row.content, self.cipher.as_ref()).await?;
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
        self.push_impl(snapshot, lock, Duration::ZERO).await
    }

    async fn lock(&self) -> Result<LockHandle, AdapterError> {
        let mut tx = self.begin_tx().await?;
        let wsid = self.workspace.as_uuid();
        let stack = self.stack_id();
        let ttl = self.lease_ttl_seconds;
        // Unpredictable by construction (OS CSPRNG), never a sequential or
        // derived counter - see `LockHandle`'s anti-forgery requirement.
        let token = OsRng.next_u64();
        let token_col = token.cast_signed();
        let holder = self.id.as_str();

        // Atomic upsert: only wins the row when absent or stale (expired).
        // Postgres resolves the ON CONFLICT under a row lock, so concurrent
        // callers cannot both win, and this blocks entirely while a push or
        // rollback holds this row via `authorize`'s `FOR UPDATE` (see the
        // module docs). Staleness and the new lease's `expires_at` are both
        // computed from Postgres's own clock (`clock_timestamp()`), not this
        // process's, so multi-instance clock skew cannot matter here; the DO
        // UPDATE branch's `WHERE` is only evaluated once any such contending
        // lock is released, so it reflects the actual current time then, not
        // whenever this statement was first issued.
        let acquired = sqlx::query!(
            r#"
            INSERT INTO state_locks (workspace_id, stack_id, holder, lock_token, acquired_at, expires_at)
            VALUES (
                $1, $2, $3, $4,
                extract(epoch FROM clock_timestamp())::bigint,
                extract(epoch FROM clock_timestamp())::bigint + $5
            )
            ON CONFLICT (workspace_id, stack_id) DO UPDATE
              SET holder = EXCLUDED.holder,
                  lock_token = EXCLUDED.lock_token,
                  acquired_at = EXCLUDED.acquired_at,
                  expires_at = EXCLUDED.expires_at
              WHERE state_locks.expires_at <= extract(epoch FROM clock_timestamp())::bigint
            RETURNING lock_token
            "#,
            wsid,
            stack,
            holder,
            token_col,
            ttl,
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

#[cfg(test)]
mod content_cipher_tests {
    //! Pure, DB-free tests for the `encode_content`/`decode_content`
    //! encryption-at-rest seam (see the module docs' "Encryption at rest"
    //! section). Live-database round trips through `PostgresStateBackend`
    //! itself live in `tests/state_backend.rs` alongside the crate's other
    //! `#[ignore]`d integration tests.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use amicu_core::Graph;
    use amicu_core::id::ModelId;
    use amicu_core::resource::{
        Artifact, ArtifactFormat, Modality, Model, ModelSource, SourceRegistry,
    };
    use amicu_state::StateCipher;
    use async_trait::async_trait;

    use super::{CIPHER_MARKER_KEY, decode_content, encode_content};

    fn sample_graph() -> Graph {
        Graph {
            models: vec![Model {
                id: ModelId::new("m1"),
                source: ModelSource {
                    registry: SourceRegistry::HuggingFace,
                    reference: "org/model".into(),
                },
                architecture: "llama".into(),
                modalities: vec![Modality::Text],
                artifact: Artifact {
                    format: ArtifactFormat::Gguf,
                    quantization: None,
                },
                context_length: None,
                variants: vec![],
                n_layers: None,
                n_kv_heads: None,
                head_dim: None,
            }],
            ..Graph::new()
        }
    }

    /// A reversible (XOR, not real cryptography) test-only [`StateCipher`],
    /// good enough to prove the seam actually transforms bytes on the way in
    /// and reverses that transform on the way out - a cipher that merely
    /// echoed its input, like [`amicu_state::IdentityCipher`], could not
    /// distinguish "the seam runs" from "the seam is a no-op". Also counts
    /// calls, so tests can confirm the seam is invoked exactly when a cipher
    /// is configured and never otherwise.
    struct XorTestCipher {
        key: u8,
        encrypt_calls: AtomicUsize,
        decrypt_calls: AtomicUsize,
    }

    impl XorTestCipher {
        fn new(key: u8) -> Self {
            Self {
                key,
                encrypt_calls: AtomicUsize::new(0),
                decrypt_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl StateCipher for XorTestCipher {
        async fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, amicu_state::StateError> {
            self.encrypt_calls.fetch_add(1, Ordering::SeqCst);
            Ok(plaintext.iter().map(|b| b ^ self.key).collect())
        }

        async fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, amicu_state::StateError> {
            self.decrypt_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ciphertext.iter().map(|b| b ^ self.key).collect())
        }
    }

    #[tokio::test]
    async fn no_cipher_encodes_identically_to_pre_seam_serde_json_to_value() {
        let graph = sample_graph();
        let via_seam = encode_content(&graph, None).await.unwrap();
        let pre_seam_equivalent = serde_json::to_value(&graph).unwrap();
        assert_eq!(
            via_seam, pre_seam_equivalent,
            "with no cipher configured, encode_content must be byte-for-byte what \
             every version of this backend wrote before the encryption seam existed"
        );
        // And it round-trips straight back with no cipher, exactly as `pull`
        // always has.
        let decoded = decode_content(via_seam, None).await.unwrap();
        assert_eq!(decoded, graph);
    }

    #[tokio::test]
    async fn cipher_round_trip_encrypt_store_decrypt_is_identical_to_original() {
        let cipher: Arc<dyn StateCipher> = Arc::new(XorTestCipher::new(0x5A));
        let graph = sample_graph();

        let stored = encode_content(&graph, Some(&cipher)).await.unwrap();

        // The wrapper is genuinely opaque: the plain graph's own fields
        // (e.g. the model id/reference) must not appear verbatim in what
        // gets persisted.
        let stored_str = stored.to_string();
        assert!(
            !stored_str.contains("org/model"),
            "encrypted content must not leak plaintext graph fields: {stored_str}"
        );
        assert!(
            stored.get(CIPHER_MARKER_KEY).is_some(),
            "encrypted content must carry the cipher marker key"
        );

        let recovered = decode_content(stored, Some(&cipher)).await.unwrap();
        assert_eq!(
            recovered, graph,
            "encrypt -> store -> decrypt must recover the exact original graph"
        );
    }

    #[tokio::test]
    async fn cipher_is_only_invoked_when_configured() {
        let cipher = Arc::new(XorTestCipher::new(0x11));
        let dyn_cipher: Arc<dyn StateCipher> = cipher.clone();
        let graph = sample_graph();

        let _ = encode_content(&graph, None).await.unwrap();
        assert_eq!(
            cipher.encrypt_calls.load(Ordering::SeqCst),
            0,
            "no cipher configured must never invoke encrypt"
        );

        let stored = encode_content(&graph, Some(&dyn_cipher)).await.unwrap();
        assert_eq!(cipher.encrypt_calls.load(Ordering::SeqCst), 1);

        let _ = decode_content(stored, Some(&dyn_cipher)).await.unwrap();
        assert_eq!(cipher.decrypt_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn wrapped_content_without_a_configured_cipher_is_a_corrupted_error_not_a_silent_misread()
    {
        let cipher: Arc<dyn StateCipher> = Arc::new(XorTestCipher::new(0x42));
        let graph = sample_graph();
        let stored = encode_content(&graph, Some(&cipher)).await.unwrap();

        // Same content, but now decoded with no cipher configured - e.g. the
        // backend's cipher was removed/misconfigured after these rows were
        // written. This must fail loudly, never silently return garbage or
        // the wrapper's raw fields mistaken for a graph.
        let err = decode_content(stored, None).await.unwrap_err();
        assert!(
            err.to_string().contains("no StateCipher is configured")
                || format!("{err:?}").contains("no StateCipher is configured"),
            "must explain the encrypted-but-no-cipher condition: {err}"
        );
    }

    #[tokio::test]
    async fn plain_legacy_content_still_decodes_when_a_cipher_is_configured() {
        // A row written before encryption was ever turned on (or by a
        // no-cipher instance) has no wrapper at all. A later instance with a
        // cipher configured must still read it as a plain graph, not fail or
        // try to decrypt it.
        let graph = sample_graph();
        let legacy_content = serde_json::to_value(&graph).unwrap();

        let cipher: Arc<dyn StateCipher> = Arc::new(XorTestCipher::new(0x99));
        let decoded = decode_content(legacy_content, Some(&cipher))
            .await
            .expect("unwrapped legacy content must decode as a plain graph");
        assert_eq!(decoded, graph);
    }

    #[tokio::test]
    async fn wrapper_with_an_unsupported_version_is_a_corrupted_error_not_a_silent_misread() {
        // A wrapper carrying the marker key but a version this backend does
        // not understand (e.g. written by a future, incompatible format)
        // must fail loudly rather than being blindly base64-decoded and
        // decrypted under the current format's assumptions.
        let cipher: Arc<dyn StateCipher> = Arc::new(XorTestCipher::new(0x77));
        let graph = sample_graph();
        let mut stored = encode_content(&graph, Some(&cipher)).await.unwrap();
        stored["__amicu_state_cipher_v__"] = serde_json::json!(u8::MAX);

        let err = decode_content(stored, Some(&cipher)).await.unwrap_err();
        assert!(
            err.to_string().contains("not supported")
                || format!("{err:?}").contains("not supported"),
            "must explain the unsupported-version condition: {err}"
        );
    }
}
