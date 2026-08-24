//! Postgres-backed [`RefreshStore`].
//!
//! Rotation is a compare-and-revoke: [`consume`](RefreshStore::consume) is a
//! single conditional `UPDATE ... WHERE ... AND NOT revoked ... RETURNING`, so
//! two racing refreshes of the same token cannot both win. The compromised-
//! family flag the in-memory store's TODO wanted persisted lives in a durable
//! `refresh_families` table, keyed by `family_id`.
//!
//! GLOBAL, not workspace-scoped: a refresh token is a session artifact of the
//! global user identity, keyed by a real `user_id` UUID foreign key into
//! `users` (`ON DELETE CASCADE`). No workspace GUC/RLS applies (see the crate
//! and migration docs).

use amicu_auth::refresh::{RefreshConsume, RefreshRecord, RefreshStore};
use amicu_auth::{AuthError, Result};
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{WorkspaceId, map_db};

/// Postgres implementation of [`RefreshStore`].
pub struct PostgresRefreshStore {
    pool: PgPool,
}

impl PostgresRefreshStore {
    /// A refresh-token store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Converts a stored BYTEA hash back into the fixed-size array. A wrong length
/// is a data-integrity fault (the store only ever writes 32-byte hashes).
fn hash_from_bytes(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes.try_into().map_err(|_| AuthError::Backend)
}

/// Parse a domain user id (token `sub`) into the `users.id` UUID. In the global
/// model every issued id is a UUID; anything else is a programming error.
fn parse_user_id(id: &str) -> Result<Uuid> {
    Uuid::parse_str(id).map_err(|_| AuthError::Backend)
}

#[async_trait]
impl RefreshStore for PostgresRefreshStore {
    async fn insert(&self, record: RefreshRecord) -> Result<()> {
        let user_id = parse_user_id(&record.user_id)?;
        let hash = record.token_hash.as_slice();
        sqlx::query!(
            "INSERT INTO refresh_tokens \
             (token_hash, user_id, workspace_id, family_id, expires_at, revoked) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            hash,
            user_id,
            record.workspace_id.as_uuid(),
            record.family_id,
            record.expires_at,
            record.revoked,
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn get(&self, token_hash: &[u8; 32]) -> Result<Option<RefreshRecord>> {
        let hash = token_hash.as_slice();
        let row = sqlx::query!(
            "SELECT token_hash, user_id, workspace_id, family_id, expires_at, revoked \
             FROM refresh_tokens WHERE token_hash = $1",
            hash,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(RefreshRecord {
                token_hash: hash_from_bytes(r.token_hash)?,
                user_id: r.user_id.to_string(),
                workspace_id: WorkspaceId::from_uuid(r.workspace_id),
                family_id: r.family_id,
                expires_at: r.expires_at,
                revoked: r.revoked,
            })),
        }
    }

    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<RefreshConsume> {
        let mut tx = self.pool.begin().await.map_err(map_db)?;
        let hash = token_hash.as_slice();

        // Atomic compare-and-revoke: only a token that is present, live, not
        // expired, and not in a compromised family is revoked here, and exactly
        // one caller can win it. Zero rows means we must classify why.
        let won = sqlx::query!(
            "UPDATE refresh_tokens AS r SET revoked = true \
             WHERE r.token_hash = $1 \
               AND NOT r.revoked AND r.expires_at > $2 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM refresh_families f \
                   WHERE f.family_id = r.family_id AND f.compromised \
               ) \
             RETURNING user_id, workspace_id, family_id",
            hash,
            now,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;

        if let Some(w) = won {
            tx.commit().await.map_err(map_db)?;
            return Ok(RefreshConsume::Consumed {
                user_id: w.user_id.to_string(),
                workspace_id: WorkspaceId::from_uuid(w.workspace_id),
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
             LEFT JOIN refresh_families f ON f.family_id = r.family_id \
             WHERE r.token_hash = $1",
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
        let hash = token_hash.as_slice();
        sqlx::query!(
            "UPDATE refresh_tokens SET revoked = true WHERE token_hash = $1",
            hash,
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn revoke_all_for_user(&self, user_id: &str) -> Result<()> {
        let uid = parse_user_id(user_id)?;
        sqlx::query!(
            "UPDATE refresh_tokens SET revoked = true WHERE user_id = $1",
            uid,
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn compromise_family(&self, family_id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(map_db)?;
        // Revoke every live token in the family and durably flag it, atomically
        // in one transaction so a concurrent rotation cannot mint a survivor.
        sqlx::query!(
            "UPDATE refresh_tokens SET revoked = true WHERE family_id = $1",
            family_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        sqlx::query!(
            "INSERT INTO refresh_families (family_id, compromised) \
             VALUES ($1, true) \
             ON CONFLICT (family_id) DO UPDATE SET compromised = true",
            family_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn is_family_compromised(&self, family_id: &str) -> Result<bool> {
        let flagged = sqlx::query!(
            "SELECT compromised FROM refresh_families WHERE family_id = $1",
            family_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(flagged.is_some_and(|r| r.compromised))
    }
}
