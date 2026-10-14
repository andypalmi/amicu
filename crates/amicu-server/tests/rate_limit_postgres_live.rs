//! Live-database coverage for the Postgres-backed shared rate limiter:
//! concurrency across two simulated replicas
//! sharing one database, window expiry, and fail-open on a database error.
//!
//! `#[ignore]`d like the other live-DB tests in this workspace (see
//! `crates/amicu-server/tests/rls_guard_live.rs`), and written against the
//! same kind of throwaway `initdb`-created cluster (a different port -
//! 55443, not rls_guard_live.rs's 55442 - so both files' recipes can run
//! side by side without colliding):
//!
//! ```sh
//! initdb -D /private/tmp/amicu-pg-ratelimit --auth-local=trust --auth-host=trust -U postgres
//! pg_ctl -D /private/tmp/amicu-pg-ratelimit \
//!     -o "-p 55443 -k /private/tmp/amicu-pg-ratelimit -h ''" \
//!     -l /private/tmp/amicu-pg-ratelimit/log start
//!
//! SOCK=/private/tmp/amicu-pg-ratelimit
//! psql -h $SOCK -p 55443 -U postgres -d postgres -c "CREATE ROLE amicu_app LOGIN;"
//! psql -h $SOCK -p 55443 -U postgres -d postgres -c "CREATE DATABASE amicu OWNER amicu_app;"
//!
//! export DATABASE_URL="postgres://%2Fprivate%2Ftmp%2Famicu-pg-ratelimit:55443/amicu?user=amicu_app"
//! cargo test -p amicu-server --test rate_limit_postgres_live -- --ignored
//!
//! pg_ctl -D /private/tmp/amicu-pg-ratelimit stop
//! rm -rf /private/tmp/amicu-pg-ratelimit
//! ```

use std::time::Duration;

use amicu_api::{PostgresRateLimiter, RateLimiter};
use amicu_store::PostgresRateLimitStore;

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the ignored tests - see this file's module docs")
}

fn unique_key(test_name: &str) -> String {
    format!("test|{test_name}|{}", uuid::Uuid::new_v4())
}

/// Two `PostgresRateLimiter` instances - each over its OWN pool, exactly as
/// two separate Cloud Run replicas each holding their own shared pool would
/// - sharing ONE database. N+1 `check` calls
/// interleaved across the two instances for the SAME key admit exactly N: the
/// atomic upsert (`crates/amicu-store/migrations/0014_rate_limit_buckets.sql`)
/// is what makes the budget genuinely shared rather than each replica
/// granting its own N, which is exactly the bug this PR fixes (see
/// `crates/amicu-api/src/ratelimit.rs`'s module docs).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; see this file's module docs"]
async fn shared_across_two_replica_instances_enforces_one_shared_budget() {
    const MAX: u32 = 5;

    let url = database_url();

    let pool_a = amicu_store::connect(&url).await.expect("connect replica a");
    amicu_store::run_migrations(&pool_a).await.expect("migrate");
    let pool_b = amicu_store::connect(&url).await.expect("connect replica b");

    let limiter_a = PostgresRateLimiter::new(
        PostgresRateLimitStore::new(pool_a),
        MAX,
        Duration::from_secs(60),
    );
    let limiter_b = PostgresRateLimiter::new(
        PostgresRateLimitStore::new(pool_b),
        MAX,
        Duration::from_secs(60),
    );

    let key = unique_key("shared_budget");
    let mut allowed = 0;
    let mut denied = 0;
    // N+1 = 6 requests, alternating which "replica" handles each one.
    for i in 0..=(MAX as usize) {
        let ok = if i % 2 == 0 {
            limiter_a.check(&key).await
        } else {
            limiter_b.check(&key).await
        };
        if ok {
            allowed += 1;
        } else {
            denied += 1;
        }
    }

    assert_eq!(
        allowed, MAX as usize,
        "exactly the configured max must be allowed across both replicas combined"
    );
    assert_eq!(
        denied, 1,
        "the (max+1)th request, on either replica, must be denied"
    );
}

/// Once the window elapses, a fresh hit is allowed again - proves the
/// Postgres-backed limiter's window actually expires and resets, not just
/// that it counts correctly within one window.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; see this file's module docs"]
async fn window_expires_and_resets_the_budget() {
    const MAX: u32 = 2;

    let url = database_url();
    let pool = amicu_store::connect(&url).await.expect("connect");
    amicu_store::run_migrations(&pool).await.expect("migrate");

    let limiter = PostgresRateLimiter::new(
        PostgresRateLimitStore::new(pool),
        MAX,
        Duration::from_secs(1),
    );
    let key = unique_key("window_expiry");

    assert!(limiter.check(&key).await, "1st request within budget");
    assert!(limiter.check(&key).await, "2nd request within budget");
    assert!(!limiter.check(&key).await, "3rd request over budget");

    tokio::time::sleep(Duration::from_millis(1_100)).await;

    assert!(
        limiter.check(&key).await,
        "window elapsed: budget refreshed"
    );
}

/// A `PostgresRateLimiter` over a CLOSED pool fails OPEN: the database hit
/// errors, but `check` still returns `true` (allows the request) and bumps
/// [`PostgresRateLimiter::fail_open_count`] - the availability-over-strict-
/// limiting trade-off documented in `crates/amicu-api/src/ratelimit.rs`'s
/// module docs.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; see this file's module docs"]
async fn fails_open_on_a_database_error() {
    let url = database_url();
    let pool = amicu_store::connect(&url).await.expect("connect");
    amicu_store::run_migrations(&pool).await.expect("migrate");

    let limiter = PostgresRateLimiter::new(
        PostgresRateLimitStore::new(pool.clone()),
        1,
        Duration::from_secs(60),
    );
    pool.close().await;

    assert_eq!(limiter.fail_open_count(), 0);
    let key = unique_key("fail_open");
    assert!(
        limiter.check(&key).await,
        "a database error must fail OPEN (allow), not lock every caller out"
    );
    assert_eq!(
        limiter.fail_open_count(),
        1,
        "the fail-open counter must observe the failure"
    );
}
