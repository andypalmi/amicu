//! Postgres-backed [`amicu_auth::throttle::LoginThrottle`],
//! shared by every Cloud Run replica.
//!
//! [`PostgresLoginThrottle::begin_attempt`] is ONE round trip: a single atomic
//! upsert that counts the attempt, arms the lock, and records (in
//! `last_admitted`, read back via `RETURNING`) whether the PRIOR lock admitted
//! it. Concurrent attempts for the same key serialize on the row lock, so N
//! parallel guesses cannot all slip in before a lock arms. The
//! backoff schedule is computed in SQL from the same constants as
//! [`amicu_auth::throttle::backoff_secs`] (a live test pins them together).
//!
//! Expired rows are swept opportunistically, like `ratelimit`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use amicu_auth::error::{AuthError, Result};
use amicu_auth::throttle::{
    Admission, FREE_ATTEMPTS, LoginThrottle, MAX_BACKOFF_SECS, RESET_AFTER_SECS,
};
use async_trait::async_trait;

use crate::PgPool;

const CLEANUP_EVERY: u64 = 128;

/// Shared login-backoff store over the one Postgres pool.
#[derive(Debug, Clone)]
pub struct PostgresLoginThrottle {
    pool: PgPool,
    calls: Arc<AtomicU64>,
}

impl PostgresLoginThrottle {
    /// A throttle over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            calls: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn cleanup(&self) {
        let reset = i64::try_from(RESET_AFTER_SECS).unwrap_or(i64::MAX);
        let _ = sqlx::query!(
            "DELETE FROM login_failures \
             WHERE updated_at <= extract(epoch FROM now())::bigint - $1 \
               AND locked_until <= extract(epoch FROM now())::bigint",
            reset,
        )
        .execute(&self.pool)
        .await;
    }
}

#[async_trait]
impl LoginThrottle for PostgresLoginThrottle {
    async fn begin_attempt(&self, key: &str) -> Result<Admission> {
        let free = i64::from(FREE_ATTEMPTS);
        let max = i64::try_from(MAX_BACKOFF_SECS).unwrap_or(i64::MAX);
        let reset = i64::try_from(RESET_AFTER_SECS).unwrap_or(i64::MAX);
        // `$1` key, `$2` free attempts, `$3` max backoff, `$4` idle reset.
        // A fresh row is attempt #1, which is always within the free attempts.
        let row = sqlx::query!(
            r#"
            INSERT INTO login_failures AS l (key, failures, locked_until, updated_at, last_admitted)
            VALUES ($1, 1, 0, extract(epoch FROM now())::bigint, TRUE)
            ON CONFLICT (key) DO UPDATE SET
                failures = CASE
                    WHEN l.locked_until > extract(epoch FROM now())::bigint THEN l.failures
                    WHEN l.updated_at + $4 <= extract(epoch FROM now())::bigint THEN 1
                    ELSE l.failures + 1
                END,
                locked_until = CASE
                    WHEN l.locked_until > extract(epoch FROM now())::bigint THEN l.locked_until
                    ELSE extract(epoch FROM now())::bigint + CASE
                        WHEN (CASE WHEN l.updated_at + $4 <= extract(epoch FROM now())::bigint
                                   THEN 1 ELSE l.failures + 1 END) <= $2 THEN 0
                        ELSE LEAST($3, (1::bigint << LEAST(
                            (CASE WHEN l.updated_at + $4 <= extract(epoch FROM now())::bigint
                                  THEN 1 ELSE l.failures + 1 END) - $2, 20)::int))
                    END
                END,
                updated_at = CASE
                    WHEN l.locked_until > extract(epoch FROM now())::bigint THEN l.updated_at
                    ELSE extract(epoch FROM now())::bigint
                END,
                last_admitted = (l.locked_until <= extract(epoch FROM now())::bigint)
            RETURNING
                last_admitted,
                (locked_until - extract(epoch FROM now())::bigint)::bigint AS "remaining!"
            "#,
            key,
            free,
            max,
            reset,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|_| AuthError::Backend)?;

        if self
            .calls
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(CLEANUP_EVERY)
        {
            // Off the request path: not covered by the caller's timeout.
            let this = self.clone();
            tokio::spawn(async move { this.cleanup().await });
        }

        Ok(if row.last_admitted {
            Admission::Allowed
        } else {
            Admission::Locked {
                retry_after_secs: u64::try_from(row.remaining).unwrap_or(0).max(1),
            }
        })
    }

    async fn clear(&self, key: &str) -> Result<()> {
        sqlx::query!("DELETE FROM login_failures WHERE key = $1", key)
            .execute(&self.pool)
            .await
            .map_err(|_| AuthError::Backend)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> PgPool {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for the ignored tests");
        let pool = crate::connect(&url).await.expect("connect");
        crate::run_migrations(&pool).await.expect("migrate");
        pool
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
    async fn matches_the_in_memory_schedule_and_clears() {
        let pool = pool().await;
        let th = PostgresLoginThrottle::new(pool.clone());
        let key = format!("test|throttle|{}", uuid::Uuid::new_v4());
        for _ in 0..3 {
            assert_eq!(th.begin_attempt(&key).await.unwrap(), Admission::Allowed);
        }
        // 4th is admitted and arms a 2s lock (backoff_secs(4)).
        assert_eq!(th.begin_attempt(&key).await.unwrap(), Admission::Allowed);
        match th.begin_attempt(&key).await.unwrap() {
            Admission::Locked { retry_after_secs } => assert!((1..=2).contains(&retry_after_secs)),
            Admission::Allowed => panic!("expected a lock"),
        }
        // Another key is independent.
        let other = format!("test|throttle-other|{}", uuid::Uuid::new_v4());
        assert_eq!(th.begin_attempt(&other).await.unwrap(), Admission::Allowed);
        // Backdate the lock so the next attempt is admitted and arms 4s.
        sqlx::query!(
            "UPDATE login_failures SET locked_until = extract(epoch FROM now())::bigint - 1 \
             WHERE key = $1",
            key
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(th.begin_attempt(&key).await.unwrap(), Admission::Allowed);
        let failures: i64 =
            sqlx::query_scalar!("SELECT failures FROM login_failures WHERE key = $1", key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(failures, 5);
        match th.begin_attempt(&key).await.unwrap() {
            Admission::Locked { retry_after_secs } => assert!((3..=4).contains(&retry_after_secs)),
            Admission::Allowed => panic!("expected a lock"),
        }
        th.clear(&key).await.unwrap();
        assert_eq!(th.begin_attempt(&key).await.unwrap(), Admission::Allowed);
    }

    #[tokio::test]
    #[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
    async fn concurrent_attempts_cannot_all_slip_in() {
        let th = PostgresLoginThrottle::new(pool().await);
        let key = format!("test|throttle-conc|{}", uuid::Uuid::new_v4());
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let th = th.clone();
            let key = key.clone();
            tasks.push(tokio::spawn(async move {
                th.begin_attempt(&key).await.unwrap()
            }));
        }
        let mut allowed = 0;
        for t in tasks {
            if t.await.unwrap() == Admission::Allowed {
                allowed += 1;
            }
        }
        // 3 free + the one that arms the lock; the rest are refused.
        assert_eq!(allowed, 4);
    }
}
