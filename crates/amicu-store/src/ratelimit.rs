//! Postgres-backed shared rate-limit counter (andypalmi/amicu-planning#142).
//!
//! Backs [`crate`] callers (via `amicu-api`'s `PostgresRateLimiter`, see that
//! crate's `ratelimit` module) that need a fixed-window request counter shared
//! by every Cloud Run replica, instead of the process-local
//! `Mutex<HashMap<..>>` that gives each replica its own full quota. There is
//! no Redis in this stack (see andypalmi/amicu-planning#142's design
//! decision), so the shared counter lives in the same Postgres the rest of
//! the control plane already uses, over the one pool per replica
//! (andypalmi/amicu-planning#139).
//!
//! # Atomicity
//!
//! [`PostgresRateLimitStore::hit`] is ONE round trip: a single
//! `INSERT ... ON CONFLICT (key) DO UPDATE ... RETURNING count` statement (see
//! `0014_rate_limit_buckets.sql`). Postgres takes a row-level lock for the
//! duration of the `ON CONFLICT` branch, so two concurrent hits for the same
//! key - whether from the same replica or two different ones - serialize
//! against each other and neither can read-then-write past the other; there
//! is no separate SELECT-then-UPDATE round trip that could race. The CASE
//! expressions decide, from the existing row's `expires_at` versus
//! Postgres's OWN clock (`extract(epoch from now())::bigint`, not a
//! caller-supplied timestamp - see [`PostgresRateLimitStore::hit`]'s docs for
//! why that matters across replicas), whether the window has expired (reset
//! to a fresh window, count 1) or is still open (increment in place) - all
//! inside that same statement.
//!
//! # Cleanup
//!
//! Expired rows are swept opportunistically: every
//! [`PostgresRateLimitStore::cleanup_every`]-th call to `hit` also runs a
//! cheap `DELETE ... WHERE expires_at <= $1` (see [`PostgresRateLimitStore::cleanup_expired`]),
//! synchronously but best-effort - a failed sweep does not fail the `hit`
//! that triggered it. This is safe to run unconditionally on every replica:
//! the DELETE's predicate is idempotent (deleting an already-deleted or
//! not-yet-expired-elsewhere row is a no-op), so concurrent sweeps from
//! multiple replicas never conflict.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::{PgPool, StoreError};

/// Default: roughly one sweep per 128 hits. Small enough to bound table
/// growth under sustained traffic, large enough that the extra `DELETE` round
/// trip is rare on the hot path.
const DEFAULT_CLEANUP_EVERY: u64 = 128;

/// Shared fixed-window counter store over the one Postgres pool every replica
/// already holds. See the module docs for the atomicity and cleanup design.
#[derive(Debug, Clone)]
pub struct PostgresRateLimitStore {
    pool: PgPool,
    cleanup_every: u64,
    hits: std::sync::Arc<AtomicU64>,
}

impl PostgresRateLimitStore {
    /// A store over `pool`, sweeping expired rows roughly every
    /// [`DEFAULT_CLEANUP_EVERY`] hits.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            cleanup_every: DEFAULT_CLEANUP_EVERY,
            hits: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    /// Overrides the cleanup cadence. `0` disables the opportunistic sweep
    /// entirely (tests that want a deterministic, sweep-free `hit` sequence).
    #[must_use]
    pub fn with_cleanup_every(mut self, cleanup_every: u64) -> Self {
        self.cleanup_every = cleanup_every;
        self
    }

    /// Atomically records one hit for `key` in a fixed window of
    /// `window_secs` seconds, returning the count WITHIN THE CURRENT WINDOW
    /// after this hit (i.e. `1` on a fresh or just-reset window). The caller
    /// compares this against its configured max to decide allow/deny - this
    /// method only counts, it does not know the limit.
    ///
    /// The window anchor is Postgres's OWN clock (`extract(epoch from
    /// now())::bigint`, evaluated inside the upsert itself), not a timestamp
    /// the caller supplies: every replica's `hit` therefore agrees on what
    /// "now" is, so the window cannot drift with a replica's own wall clock
    /// (VM clock skew, a slow/fast container clock, NTP still catching up
    /// after a cold start). `now()` is fixed for the whole statement (a
    /// single `INSERT` is one implicit transaction), so every occurrence
    /// below - in the `VALUES` list and in each `CASE` branch of the `ON
    /// CONFLICT` update - reads the exact same instant.
    ///
    /// `window_secs` is clamped to at least 1 so a misconfigured `0` cannot
    /// produce a window that is already expired the instant it is written.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] if the upsert (or, on the
    /// [`Self::with_cleanup_every`]-th call, the opportunistic cleanup) fails.
    /// Callers that want to fail OPEN on a database error (recommended for a
    /// login-availability-over-strict-limiting posture - see
    /// andypalmi/amicu-planning#142) should catch this at the call site rather
    /// than propagate it as a hard failure.
    pub async fn hit(&self, key: &str, window_secs: i64) -> Result<i64, StoreError> {
        let window_secs = window_secs.max(1);

        let count: i64 = sqlx::query_scalar!(
            r#"
            INSERT INTO rate_limit_buckets (key, window_start, count, expires_at)
            VALUES (
                $1,
                extract(epoch FROM now())::bigint,
                1,
                extract(epoch FROM now())::bigint + $2
            )
            ON CONFLICT (key) DO UPDATE SET
                count = CASE
                    WHEN rate_limit_buckets.expires_at <= extract(epoch FROM now())::bigint THEN 1
                    ELSE rate_limit_buckets.count + 1
                END,
                window_start = CASE
                    WHEN rate_limit_buckets.expires_at <= extract(epoch FROM now())::bigint
                        THEN extract(epoch FROM now())::bigint
                    ELSE rate_limit_buckets.window_start
                END,
                expires_at = CASE
                    WHEN rate_limit_buckets.expires_at <= extract(epoch FROM now())::bigint
                        THEN extract(epoch FROM now())::bigint + $2
                    ELSE rate_limit_buckets.expires_at
                END
            RETURNING count
            "#,
            key,
            window_secs,
        )
        .fetch_one(&self.pool)
        .await?;

        if self.cleanup_every > 0 {
            let n = self.hits.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(self.cleanup_every) {
                // Best-effort: a failed sweep must not fail the hit that
                // triggered it, so this error is swallowed (a future sweep
                // catches up). If callers need to observe sweep failures,
                // call `cleanup_expired` directly instead.
                let _ = self.cleanup_expired().await;
            }
        }

        Ok(count)
    }

    /// Deletes every row whose window has already expired as of Postgres's
    /// OWN clock (see [`Self::hit`]'s docs on why the anchor lives in the
    /// database, not the caller). Returns the number of rows removed.
    /// Idempotent and safe to call concurrently from multiple replicas - the
    /// predicate matches the same rows regardless of which replica's sweep
    /// gets there first, so a second sweep over the same expired rows simply
    /// deletes zero.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] if the delete fails.
    /// Current in-window count for `key` WITHOUT counting a hit (0 when there is
    /// no live window). Used for "is this budget already exhausted?" checks.
    ///
    /// # Errors
    /// [`StoreError`] on a database failure.
    pub async fn peek(&self, key: &str) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            "SELECT count FROM rate_limit_buckets \
             WHERE key = $1 AND expires_at > extract(epoch FROM now())::bigint",
            key,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(count.unwrap_or(0))
    }

    pub async fn cleanup_expired(&self) -> Result<u64, StoreError> {
        let result = sqlx::query!(
            "DELETE FROM rate_limit_buckets WHERE expires_at <= extract(epoch FROM now())::bigint"
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Directly backdates `key`'s `expires_at` into the past, relative to
    /// Postgres's own clock - simulating window expiry without waiting on
    /// the real clock. `hit` no longer accepts a caller-supplied `now` (the
    /// window anchor lives inside the atomic upsert, evaluated from
    /// Postgres's clock - see [`PostgresRateLimitStore::hit`]'s docs), so
    /// tests that need to force expiry deterministically reach around it with
    /// a raw `UPDATE` instead.
    async fn expire_now(pool: &PgPool, key: &str) {
        sqlx::query!(
            "UPDATE rate_limit_buckets SET expires_at = extract(epoch FROM now())::bigint - 1 \
             WHERE key = $1",
            key,
        )
        .execute(pool)
        .await
        .expect("backdate expires_at");
    }

    /// Two `hit`s within the same window against the SAME store accumulate;
    /// requires a live Postgres, so this is `#[ignore]`d like the rest of the
    /// crate's live-database tests.
    #[tokio::test]
    #[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
    async fn hits_accumulate_within_a_window() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for the ignored tests");
        let pool = crate::connect(&url).await.expect("connect");
        crate::run_migrations(&pool).await.expect("migrate");

        let store = PostgresRateLimitStore::new(pool).with_cleanup_every(0);
        let key = format!("test|hits_accumulate|{}", uuid::Uuid::new_v4());

        // A generous 60s window comfortably outlasts the test's own runtime,
        // so all three hits land inside the same window on the real clock.
        assert_eq!(store.hit(&key, 60).await.unwrap(), 1);
        assert_eq!(store.hit(&key, 60).await.unwrap(), 2);
        assert_eq!(store.hit(&key, 60).await.unwrap(), 3);
    }

    /// Once the window's `expires_at` is in the past, the next hit resets the
    /// counter to 1 instead of continuing to accumulate.
    #[tokio::test]
    #[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
    async fn window_expiry_resets_the_counter() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for the ignored tests");
        let pool = crate::connect(&url).await.expect("connect");
        crate::run_migrations(&pool).await.expect("migrate");

        let store = PostgresRateLimitStore::new(pool.clone()).with_cleanup_every(0);
        let key = format!("test|window_expiry|{}", uuid::Uuid::new_v4());

        assert_eq!(store.hit(&key, 10).await.unwrap(), 1);
        assert_eq!(store.hit(&key, 10).await.unwrap(), 2);

        expire_now(&pool, &key).await;

        // Past the window: resets to a fresh window.
        assert_eq!(store.hit(&key, 10).await.unwrap(), 1);
    }

    /// `cleanup_expired` removes only rows whose window has already ended,
    /// leaving still-live windows untouched.
    #[tokio::test]
    #[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
    async fn cleanup_removes_only_expired_rows() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for the ignored tests");
        let pool = crate::connect(&url).await.expect("connect");
        crate::run_migrations(&pool).await.expect("migrate");

        let store = PostgresRateLimitStore::new(pool.clone()).with_cleanup_every(0);
        let expired_key = format!("test|cleanup-expired|{}", uuid::Uuid::new_v4());
        let live_key = format!("test|cleanup-live|{}", uuid::Uuid::new_v4());

        store.hit(&expired_key, 500).await.unwrap();
        store.hit(&live_key, 500).await.unwrap();
        expire_now(&pool, &expired_key).await;

        let deleted = store.cleanup_expired().await.unwrap();
        assert!(deleted >= 1, "the backdated key must be swept");

        // The live key survives: a fresh hit still accumulates rather than
        // resetting to 1.
        assert_eq!(store.hit(&live_key, 500).await.unwrap(), 2);
    }
}
