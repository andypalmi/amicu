//! Postgres-backed [`RefreshStore`].
//!
//! Rotation is a compare-and-revoke: [`consume`](RefreshStore::consume) is a
//! single conditional `UPDATE ... WHERE ... AND NOT revoked ... RETURNING`, so
//! two racing refreshes of the same token cannot both win. The compromised-
//! family flag the in-memory store's TODO wanted persisted lives in a durable
//! `refresh_families` table, keyed by `(workspace_id, family_id)`.
//!
//! Every statement is workspace-scoped and runs inside a GUC-bound transaction
//! (see [`crate::begin_scoped`]).

use amicu_auth::refresh::{RefreshConsume, RefreshRecord, RefreshStore};
use amicu_auth::{AuthError, Result};
use async_trait::async_trait;
use sqlx::PgPool;

use crate::{WorkspaceId, begin_scoped, map_db};

/// Postgres implementation of [`RefreshStore`].
pub struct PostgresRefreshStore {
    pool: PgPool,
    workspace: WorkspaceId,
}

impl PostgresRefreshStore {
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

/// Converts a stored BYTEA hash back into the fixed-size array. A wrong length
/// is a data-integrity fault (the store only ever writes 32-byte hashes).
fn hash_from_bytes(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes.try_into().map_err(|_| AuthError::Backend)
}

#[async_trait]
impl RefreshStore for PostgresRefreshStore {
    async fn insert(&self, record: RefreshRecord) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let hash = record.token_hash.as_slice();
        sqlx::query!(
            "INSERT INTO refresh_tokens \
             (workspace_id, token_hash, user_subject, family_id, expires_at, revoked) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            wsid,
            hash,
            record.user_id,
            record.family_id,
            record.expires_at,
            record.revoked,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn get(&self, token_hash: &[u8; 32]) -> Result<Option<RefreshRecord>> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let hash = token_hash.as_slice();
        let row = sqlx::query!(
            "SELECT token_hash, user_subject, family_id, expires_at, revoked \
             FROM refresh_tokens WHERE workspace_id = $1 AND token_hash = $2",
            wsid,
            hash,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(RefreshRecord {
                token_hash: hash_from_bytes(r.token_hash)?,
                user_id: r.user_subject,
                family_id: r.family_id,
                expires_at: r.expires_at,
                revoked: r.revoked,
            })),
        }
    }

    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<RefreshConsume> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let hash = token_hash.as_slice();

        // Atomic compare-and-revoke: only a token that is present, live, not
        // expired, and not in a compromised family is revoked here, and exactly
        // one caller can win it. Zero rows means we must classify why.
        let won = sqlx::query!(
            "UPDATE refresh_tokens AS r SET revoked = true \
             WHERE r.workspace_id = $1 AND r.token_hash = $2 \
               AND NOT r.revoked AND r.expires_at > $3 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM refresh_families f \
                   WHERE f.workspace_id = $1 AND f.family_id = r.family_id \
                     AND f.compromised \
               ) \
             RETURNING user_subject, family_id",
            wsid,
            hash,
            now,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;

        if let Some(w) = won {
            tx.commit().await.map_err(map_db)?;
            return Ok(RefreshConsume::Consumed {
                user_id: w.user_subject,
                family_id: w.family_id,
            });
        }

        // Classify the non-winning outcome for the caller. Reuse (revoked or
        // compromised family) is checked before expiry, matching the in-memory
        // contract.
        let state = sqlx::query!(
            "SELECT r.revoked, r.expires_at, r.family_id, \
                    COALESCE(f.compromised, false) AS \"compromised!\" \
             FROM refresh_tokens r \
             LEFT JOIN refresh_families f \
               ON f.workspace_id = r.workspace_id AND f.family_id = r.family_id \
             WHERE r.workspace_id = $1 AND r.token_hash = $2",
            wsid,
            hash,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;

        let Some(s) = state else {
            return Ok(RefreshConsume::NotFound);
        };
        if s.revoked || s.compromised {
            return Ok(RefreshConsume::Reuse {
                family_id: s.family_id,
            });
        }
        if s.expires_at <= now {
            return Ok(RefreshConsume::Expired);
        }
        // Live, not expired, not compromised, yet the conditional UPDATE missed
        // it: a concurrent consumer revoked it between the two statements. That
        // is exactly the reuse condition.
        Ok(RefreshConsume::Reuse {
            family_id: s.family_id,
        })
    }

    async fn revoke(&self, token_hash: &[u8; 32]) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let hash = token_hash.as_slice();
        sqlx::query!(
            "UPDATE refresh_tokens SET revoked = true \
             WHERE workspace_id = $1 AND token_hash = $2",
            wsid,
            hash,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn revoke_all_for_user(&self, user_id: &str) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        sqlx::query!(
            "UPDATE refresh_tokens SET revoked = true \
             WHERE workspace_id = $1 AND user_subject = $2",
            wsid,
            user_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn compromise_family(&self, family_id: &str) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        // Revoke every live token in the family and durably flag it, atomically
        // in one transaction so a concurrent rotation cannot mint a survivor.
        sqlx::query!(
            "UPDATE refresh_tokens SET revoked = true \
             WHERE workspace_id = $1 AND family_id = $2",
            wsid,
            family_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        sqlx::query!(
            "INSERT INTO refresh_families (workspace_id, family_id, compromised) \
             VALUES ($1, $2, true) \
             ON CONFLICT (workspace_id, family_id) DO UPDATE SET compromised = true",
            wsid,
            family_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn is_family_compromised(&self, family_id: &str) -> Result<bool> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let flagged = sqlx::query!(
            "SELECT compromised FROM refresh_families \
             WHERE workspace_id = $1 AND family_id = $2",
            wsid,
            family_id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(flagged.is_some_and(|r| r.compromised))
    }
}
