//! Postgres-backed [`ResetStore`].
//!
//! Single-use consumption is a compare-and-mark: [`consume`](ResetStore::consume)
//! is one conditional `UPDATE ... WHERE ... AND NOT consumed ... RETURNING`, so a
//! reset token cannot be redeemed twice even under a race.
//!
//! Every statement is workspace-scoped and runs inside a GUC-bound transaction
//! (see [`crate::begin_scoped`]).

use amicu_auth::Result;
use amicu_auth::reset::{ResetConsume, ResetRecord, ResetStore};
use async_trait::async_trait;
use sqlx::PgPool;

use crate::{WorkspaceId, begin_scoped, map_db};

/// Postgres implementation of [`ResetStore`].
pub struct PostgresResetStore {
    pool: PgPool,
    workspace: WorkspaceId,
}

impl PostgresResetStore {
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
impl ResetStore for PostgresResetStore {
    async fn insert(&self, record: ResetRecord) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let hash = record.token_hash.as_slice();
        sqlx::query!(
            "INSERT INTO reset_tokens \
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

    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<ResetConsume> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let hash = token_hash.as_slice();

        // Atomic compare-and-mark: only a present, unconsumed, unexpired token is
        // marked here, and exactly one caller can win it.
        let won = sqlx::query!(
            "UPDATE reset_tokens SET consumed = true \
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
            return Ok(ResetConsume::Consumed {
                user_id: w.user_subject,
            });
        }

        // Classify the non-winning outcome. Reuse (already consumed) is checked
        // before expiry, matching the in-memory contract.
        let state = sqlx::query!(
            "SELECT consumed, expires_at FROM reset_tokens \
             WHERE workspace_id = $1 AND token_hash = $2",
            wsid,
            hash,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;

        let Some(s) = state else {
            return Ok(ResetConsume::NotFound);
        };
        if s.consumed {
            return Ok(ResetConsume::Reuse);
        }
        if s.expires_at <= now {
            return Ok(ResetConsume::Expired);
        }
        // Live and unexpired yet the conditional UPDATE missed it: a concurrent
        // consumer marked it between the two statements. That is reuse.
        Ok(ResetConsume::Reuse)
    }
}
