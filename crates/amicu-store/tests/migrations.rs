//! Live-database integration test for the store scaffold.
//!
//! This test is `#[ignore]`d so `cargo test --workspace` skips it when no
//! Postgres is available (as in CI). Run it against a live database with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://amicu_app:amicu_app@localhost:5432/amicu \
//!     cargo test -p amicu-store -- --ignored
//! ```

use amicu_store::{connect, run_migrations};

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn connects_and_applies_migrations() {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the ignored migration test");

    let pool = connect(&url).await.expect("connect to Postgres");

    // Idempotent: applying twice must succeed.
    run_migrations(&pool).await.expect("apply migrations");
    run_migrations(&pool)
        .await
        .expect("re-apply migrations is a no-op");
}
