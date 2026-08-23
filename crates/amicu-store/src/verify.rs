//! Postgres-backed [`VerifyStore`].
//!
//! Single-use consumption is a compare-and-mark: [`consume`](VerifyStore::consume)
//! is one conditional `UPDATE ... WHERE ... AND NOT consumed ... RETURNING`, so a
//! verify token cannot be redeemed twice even under a race.
//!
//! Every statement is workspace-scoped and runs inside a GUC-bound transaction
//! (see [`crate::begin_scoped`]).

use amicu_auth::Result;
use amicu_auth::verify::{VerifyConsume, VerifyRecord, VerifyStore};
use async_trait::async_trait;
use sqlx::PgPool;

use crate::{WorkspaceId, begin_scoped, map_db};

/// Postgres implementation of [`VerifyStore`].
pub struct PostgresVerifyStore {
    pool: PgPool,
    workspace: WorkspaceId,
}

impl PostgresVerifyStore {
    /// A store bound to the single-tenant default workspace.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            workspace: WorkspaceId::DEFAULT,
        }
    }

    /// A store bound to a specific workspace (multi-tenant, future use).
    #[must_use]
    pub fn with_workspace(pool: PgPool, workspace: WorkspaceId) -> Self {
        Self { pool, workspace }
    }
}

#[async_trait]
impl VerifyStore for PostgresVerifyStore {
    async fn insert(&self, record: VerifyRecord) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let hash = record.token_hash.as_slice();
        sqlx::query!(
            "INSERT INTO verify_tokens \
             (workspace_id, token_hash, user_subject, expires_at, consumed) \
             VALUES ($1, $2, $3, $4, $5)",
            wsid,
            hash,
            record.user_id,
            record.expires_at,
            record.consumed,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<VerifyConsume> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let hash = token_hash.as_slice();

        // Atomic compare-and-mark: only a present, unconsumed, unexpired token is
        // marked here, and exactly one caller can win it.
        let won = sqlx::query!(
            "UPDATE verify_tokens SET consumed = true \
             WHERE workspace_id = $1 AND token_hash = $2 \
               AND NOT consumed AND expires_at > $3 \
             RETURNING user_subject",
            wsid,
            hash,
            now,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;

        if let Some(w) = won {
            tx.commit().await.map_err(map_db)?;
            return Ok(VerifyConsume::Consumed {
                user_id: w.user_subject,
            });
        }

        // Classify the non-winning outcome. Reuse (already consumed) is checked
        // before expiry, matching the in-memory contract.
        let state = sqlx::query!(
            "SELECT consumed, expires_at FROM verify_tokens \
             WHERE workspace_id = $1 AND token_hash = $2",
            wsid,
            hash,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;

        let Some(s) = state else {
            return Ok(VerifyConsume::NotFound);
        };
        if s.consumed {
            return Ok(VerifyConsume::Reuse);
        }
        if s.expires_at <= now {
            return Ok(VerifyConsume::Expired);
        }
        // Live and unexpired yet the conditional UPDATE missed it: a concurrent
        // consumer marked it between the two statements. That is reuse.
        Ok(VerifyConsume::Reuse)
    }
}
