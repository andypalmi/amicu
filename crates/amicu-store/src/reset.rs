//! Postgres-backed [`ResetStore`].
//!
//! Single-use consumption is a compare-and-mark: [`consume`](ResetStore::consume)
//! is one conditional `UPDATE ... WHERE ... AND NOT consumed ... RETURNING`, so a
//! reset token cannot be redeemed twice even under a race.
//!
//! GLOBAL, not workspace-scoped: a password reset acts on the global user
//! identity (the password lives on `users`), keyed by a real `user_id` UUID
//! foreign key into `users` (`ON DELETE CASCADE`). No workspace GUC/RLS applies.
//!
//! [`insert_and_notify`](ResetStore::insert_and_notify) additionally writes the
//! delivery into the `email_outbox` queue inside the SAME transaction as the
//! token row (andypalmi/amicu-planning#141), so a minted reset token and the
//! email that carries it commit together or not at all.

use amicu_auth::notify::{Notification, Notifier};
use amicu_auth::reset::{ResetConsume, ResetRecord, ResetStore};
use amicu_auth::{AuthError, Result};
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::map_db;
use crate::outbox::{NewOutboxEmail, enqueue, now_unix};

/// Postgres implementation of [`ResetStore`].
pub struct PostgresResetStore {
    pool: PgPool,
}

impl PostgresResetStore {
    /// A reset-token store over `pool`.
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
impl ResetStore for PostgresResetStore {
    async fn insert(&self, record: ResetRecord) -> Result<()> {
        let user_id = parse_user_id(&record.user_id)?;
        let hash = record.token_hash.as_slice();
        sqlx::query!(
            "INSERT INTO reset_tokens (token_hash, user_id, expires_at, consumed) \
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

    /// Writes the token row and the queued email in ONE transaction.
    ///
    /// `notifier` is deliberately ignored: on this path the durable outbox IS
    /// the delivery mechanism, and routing through the `Notifier` seam would put
    /// the enqueue in its own transaction - exactly the atomicity this override
    /// exists to provide. (The trait's default implementation does use it, which
    /// is what keeps the in-memory dev path working with the `LoggingNotifier`.)
    async fn insert_and_notify(
        &self,
        record: ResetRecord,
        message: Notification,
        _notifier: &dyn Notifier,
    ) -> Result<()> {
        let user_id = parse_user_id(&record.user_id)?;
        let hash = record.token_hash.as_slice();
        let queued = NewOutboxEmail::from_notification(&message)?;

        let mut tx = self.pool.begin().await.map_err(map_db)?;
        sqlx::query!(
            "INSERT INTO reset_tokens (token_hash, user_id, expires_at, consumed) \
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

    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<ResetConsume> {
        let mut tx = self.pool.begin().await.map_err(map_db)?;
        let hash = token_hash.as_slice();

        // Atomic compare-and-mark: only a present, unconsumed, unexpired token is
        // marked here, and exactly one caller can win it.
        let won = sqlx::query!(
            "UPDATE reset_tokens SET consumed = true \
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
            return Ok(ResetConsume::Consumed {
                user_id: w.user_id.to_string(),
            });
        }

        // Classify the non-winning outcome. Reuse (already consumed) is checked
        // before expiry, matching the in-memory contract.
        let state = sqlx::query!(
            "SELECT consumed, expires_at FROM reset_tokens WHERE token_hash = $1",
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
