//! Postgres-backed [`VerifyStore`].
//!
//! Single-use consumption is a compare-and-mark: [`consume`](VerifyStore::consume)
//! is one conditional `UPDATE ... WHERE ... AND NOT consumed ... RETURNING`, so a
//! verify token cannot be redeemed twice even under a race.
//!
//! GLOBAL, not workspace-scoped: email verification confirms the global user's
//! email (`email_verified_at` lives on `users`), keyed by a real `user_id` UUID
//! foreign key into `users` (`ON DELETE CASCADE`). No workspace GUC/RLS applies.
//!
//! [`insert_and_notify`](VerifyStore::insert_and_notify) additionally writes the
//! delivery into the `email_outbox` queue inside the SAME transaction as the
//! token row - see [`crate::reset`] for the full
//! rationale.

use amicu_auth::notify::{Notification, Notifier};
use amicu_auth::verify::{VerifyConsume, VerifyRecord, VerifyStore};
use amicu_auth::{AuthError, Result};
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::map_db;
use crate::outbox::{NewOutboxEmail, enqueue, now_unix};

/// Postgres implementation of [`VerifyStore`].
pub struct PostgresVerifyStore {
    pool: PgPool,
}

impl PostgresVerifyStore {
    /// A verify-token store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Parse a domain user id into the `users.id` UUID (see [`crate::refresh`]).
fn parse_user_id(id: &str) -> Result<Uuid> {
    Uuid::parse_str(id).map_err(|_| AuthError::Backend)
}

#[async_trait]
impl VerifyStore for PostgresVerifyStore {
    async fn insert(&self, record: VerifyRecord) -> Result<()> {
        let user_id = parse_user_id(&record.user_id)?;
        let hash = record.token_hash.as_slice();
        sqlx::query!(
            "INSERT INTO verify_tokens (token_hash, user_id, expires_at, consumed) \
             VALUES ($1, $2, $3, $4)",
            hash,
            user_id,
            record.expires_at,
            record.consumed,
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(())
    }

    /// Writes the token row and the queued email in ONE transaction; ignores
    /// `notifier` for the same reason
    /// [`PostgresResetStore`](crate::reset::PostgresResetStore) does.
    async fn insert_and_notify(
        &self,
        record: VerifyRecord,
        message: Notification,
        _notifier: &dyn Notifier,
    ) -> Result<()> {
        let user_id = parse_user_id(&record.user_id)?;
        let hash = record.token_hash.as_slice();
        let queued = NewOutboxEmail::from_notification(&message)?;

        let mut tx = self.pool.begin().await.map_err(map_db)?;
        sqlx::query!(
            "INSERT INTO verify_tokens (token_hash, user_id, expires_at, consumed) \
             VALUES ($1, $2, $3, $4)",
            hash,
            user_id,
            record.expires_at,
            record.consumed,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        enqueue(&mut *tx, &queued, now_unix())
            .await
            .map_err(|_| AuthError::Backend)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<VerifyConsume> {
        let mut tx = self.pool.begin().await.map_err(map_db)?;
        let hash = token_hash.as_slice();

        // Atomic compare-and-mark: only a present, unconsumed, unexpired token is
        // marked here, and exactly one caller can win it.
        let won = sqlx::query!(
            "UPDATE verify_tokens SET consumed = true \
             WHERE token_hash = $1 AND NOT consumed AND expires_at > $2 \
             RETURNING user_id",
            hash,
            now,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;

        if let Some(w) = won {
            tx.commit().await.map_err(map_db)?;
            return Ok(VerifyConsume::Consumed {
                user_id: w.user_id.to_string(),
            });
        }

        // Classify the non-winning outcome. Reuse (already consumed) is checked
        // before expiry, matching the in-memory contract.
        let state = sqlx::query!(
            "SELECT consumed, expires_at FROM verify_tokens WHERE token_hash = $1",
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
