//! Live-database integration tests for the Postgres config store.
//!
//! These are `#[ignore]`d so `cargo test --workspace` skips them when no
//! Postgres is available (as in CI). Run them against a live database with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://amicu_app:amicu_app@localhost:5432/amicu \
//!     cargo test -p amicu-store -- --ignored
//! ```
//!
//! Every test uses a freshly-random config name so they can share one
//! database without colliding.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_store::config::ConfigError;
use amicu_store::{ConfigStore, PostgresConfigStore, WorkspaceId, connect, run_migrations};
use sha2::Digest;
use sqlx::PgPool;
use uuid::Uuid;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A collision-resistant unique suffix for per-test identifiers.
fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos}_{n}")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the ignored store tests");
    let pool = connect(&url).await.expect("connect to Postgres");
    run_migrations(&pool).await.expect("apply migrations");
    pool
}

/// The single-tenant default workspace (seeded by migration 0010), used by the
/// tests that do not exercise cross-workspace isolation. The store is no longer
/// pinned to a workspace: it is passed per call (andypalmi/amicu-planning#130).
const WS: WorkspaceId = WorkspaceId::DEFAULT;

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn config_create_get_append_and_list_versions() {
    let store = PostgresConfigStore::new(pool().await);
    let name = unique("cfg");
    let author = Uuid::new_v4();

    let created = store
        .create(WS, &name, "hcl v1", Some(author), Some("initial"))
        .await
        .expect("create config");
    assert_eq!(created.current_version, 1);
    assert_eq!(created.name, name);

    let current = store
        .get_current(WS, created.id)
        .await
        .expect("get_current")
        .expect("config present");
    assert_eq!(current.content, "hcl v1");
    assert_eq!(current.current_version, 1);
    assert_eq!(
        current.content_sha256,
        <[u8; 32]>::from(sha2::Sha256::digest(b"hcl v1"))
    );

    let listed = store.list(WS).await.expect("list");
    assert!(
        listed
            .iter()
            .any(|c| c.id == created.id && c.name == name && c.current_version == 1)
    );

    let appended = store
        .append_version(WS, created.id, "hcl v2", Some(author), Some("second"))
        .await
        .expect("append_version");
    assert_eq!(appended.version_no, 2);
    assert_eq!(appended.created_by, Some(author));
    assert_eq!(appended.message.as_deref(), Some("second"));

    let current = store
        .get_current(WS, created.id)
        .await
        .expect("get_current")
        .expect("config present");
    assert_eq!(current.content, "hcl v2");
    assert_eq!(current.current_version, 2);

    let versions = store
        .list_versions(WS, created.id)
        .await
        .expect("list_versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].version_no, 1);
    assert_eq!(versions[1].version_no, 2);

    let v1 = store
        .get_version(WS, created.id, 1)
        .await
        .expect("get_version")
        .expect("version 1 present");
    assert_eq!(v1.content, "hcl v1");
    assert_eq!(v1.message.as_deref(), Some("initial"));

    // An unknown config id is None/empty, not an error.
    let unknown = Uuid::new_v4();
    assert!(
        store
            .get_current(WS, unknown)
            .await
            .expect("get_current unknown")
            .is_none()
    );
    assert_eq!(
        store
            .list_versions(WS, unknown)
            .await
            .expect("list_versions unknown"),
        Vec::new()
    );
    assert!(
        store
            .get_version(WS, unknown, 1)
            .await
            .expect("get_version unknown")
            .is_none()
    );

    // append_version against an unknown config id is an explicit error.
    assert!(matches!(
        store
            .append_version(WS, unknown, "x", None, None)
            .await
            .unwrap_err(),
        ConfigError::NotFound
    ));
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn config_duplicate_name_is_rejected() {
    let store = PostgresConfigStore::new(pool().await);
    let name = unique("cfg");

    store
        .create(WS, &name, "hcl", None, None)
        .await
        .expect("create config");

    let dup = store.create(WS, &name, "other hcl", None, None).await;
    assert!(matches!(dup, Err(ConfigError::DuplicateName(n)) if n == name));
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn config_rollback_creates_new_version_and_preserves_history() {
    let store = PostgresConfigStore::new(pool().await);
    let name = unique("cfg");

    let created = store
        .create(WS, &name, "hcl v1", None, Some("v1"))
        .await
        .expect("create config");
    store
        .append_version(WS, created.id, "hcl v2", None, Some("v2"))
        .await
        .expect("append v2");
    store
        .append_version(WS, created.id, "hcl v3", None, Some("v3"))
        .await
        .expect("append v3");

    let before = store
        .list_versions(WS, created.id)
        .await
        .expect("list_versions before rollback");
    assert_eq!(before.len(), 3);

    // Roll back to v1's content: this must mint a brand-new version 4, not
    // touch or remove versions 1-3. created_by attributes who triggered the
    // rollback itself, not the copied content's original author.
    let rollback_actor = Uuid::new_v4();
    let rolled = store
        .rollback(WS, created.id, 1, Some(rollback_actor))
        .await
        .expect("rollback to version 1");
    assert_eq!(rolled.version_no, 4);
    assert_eq!(rolled.created_by, Some(rollback_actor));

    // The attribution is durably recorded on the new version row, not just
    // returned in-memory.
    let v4 = store
        .get_version(WS, created.id, 4)
        .await
        .expect("get_version 4")
        .expect("v4 present");
    assert_eq!(v4.created_by, Some(rollback_actor));

    let after = store
        .list_versions(WS, created.id)
        .await
        .expect("list_versions after rollback");
    assert_eq!(after.len(), 4);
    // History is untouched: v1, v2, v3 are all still there, in order.
    assert_eq!(after[0].version_no, 1);
    assert_eq!(after[1].version_no, 2);
    assert_eq!(after[2].version_no, 3);
    assert_eq!(after[3].version_no, 4);

    // v1 is still independently fetchable with its original content.
    let v1 = store
        .get_version(WS, created.id, 1)
        .await
        .expect("get_version 1")
        .expect("v1 still present");
    assert_eq!(v1.content, "hcl v1");

    // The new current version (4) carries v1's content, copied, not moved.
    let current = store
        .get_current(WS, created.id)
        .await
        .expect("get_current")
        .expect("config present");
    assert_eq!(current.current_version, 4);
    assert_eq!(current.content, "hcl v1");

    // Rolling back to a version that never existed is a distinct error.
    assert!(matches!(
        store.rollback(WS, created.id, 99, None).await.unwrap_err(),
        ConfigError::VersionNotFound(99)
    ));
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn config_is_invisible_to_another_workspace() {
    let shared = pool().await;
    let name = unique("cfg");

    // One store handle serves every tenant now; the workspace is passed per call
    // and drives a `begin_scoped` transaction, so the workspace_id predicate (and
    // the RLS backstop, fail-closed via NULLIF since migration 0012) isolates.
    let store = PostgresConfigStore::new(shared.clone());
    let created = store
        .create(WS, &name, "hcl", None, None)
        .await
        .expect("create config");

    // configs.workspace_id FKs to workspaces(id), so the other workspace needs a
    // real row before it can write anything.
    let other = WorkspaceId::new();
    sqlx::query!(
        "INSERT INTO workspaces (id, slug, name, tier, created_at) \
         VALUES ($1, $2, 'other tenant', 'community', 0)",
        other.as_uuid(),
        unique("ws"),
    )
    .execute(&shared)
    .await
    .expect("seed other workspace");

    // The other workspace must not see the first tenant's config at all.
    assert!(
        store
            .get_current(other, created.id)
            .await
            .expect("cross-tenant get_current")
            .is_none()
    );
    assert_eq!(
        store
            .list_versions(other, created.id)
            .await
            .expect("cross-tenant list_versions"),
        Vec::new()
    );
    assert!(
        !store
            .list(other)
            .await
            .expect("cross-tenant list")
            .iter()
            .any(|c| c.id == created.id)
    );

    // A same-named config can be created in the other workspace: the
    // UNIQUE (workspace_id, name) constraint is per-tenant, not global.
    store
        .create(other, &name, "other tenant hcl", None, None)
        .await
        .expect("create same name in other workspace");

    // Writes from the other workspace cannot touch the original tenant's row.
    assert!(matches!(
        store
            .append_version(other, created.id, "x", None, None)
            .await
            .unwrap_err(),
        ConfigError::NotFound
    ));
    // rollback looks up the source version first, scoped to this workspace;
    // since no such row exists here, it fails closed as VersionNotFound
    // rather than ever reading the other tenant's content.
    assert!(matches!(
        store
            .rollback(other, created.id, 1, None)
            .await
            .unwrap_err(),
        ConfigError::VersionNotFound(1)
    ));

    // The config is still live for its own workspace.
    let current = store
        .get_current(WS, created.id)
        .await
        .expect("get_current")
        .expect("config present");
    assert_eq!(current.content, "hcl");
}

/// N2 fail-closed backport (andypalmi/amicu-planning#130, migration 0012): with
/// NO `app.current_workspace` bound - or the empty string a reverted SET LOCAL
/// leaves on a pooled connection - the config/state RLS policies must match
/// NOTHING (fail-closed via NULLIF), never ERROR with `invalid input syntax for
/// type uuid: ""`. Runs as the non-superuser `amicu_app` role, so RLS is engaged.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn config_rls_fails_closed_without_or_with_empty_workspace_context() {
    let shared = pool().await;

    // Seed a real config in the default workspace so there IS a row RLS could
    // leak if it were not fail-closed.
    let store = PostgresConfigStore::new(shared.clone());
    store
        .create(WS, &unique("cfg"), "hcl", None, None)
        .await
        .expect("create config");

    // (a) No context bound at all: the policy yields NULL -> matches nothing,
    // and the query MUST NOT error.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM configs")
        .fetch_one(&shared)
        .await
        .expect("counting configs with no workspace context must not error");
    assert_eq!(count, 0, "unset workspace GUC must fail closed (no rows)");

    // (b) The empty-string revert case: bind '' explicitly (as a pooled
    // connection reads after a prior SET LOCAL reverts) and confirm it also
    // fails closed rather than throwing on ''::uuid.
    let mut tx = shared.begin().await.expect("begin");
    sqlx::query("SELECT set_config('app.current_workspace', '', true)")
        .execute(&mut *tx)
        .await
        .expect("bind empty workspace GUC");
    let count_empty: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM configs")
        .fetch_one(&mut *tx)
        .await
        .expect("empty-string workspace GUC must not error");
    assert_eq!(
        count_empty, 0,
        "empty-string GUC must fail closed (no rows)"
    );
    // The same fail-closed guarantee holds for the state tables (same 0012 pass).
    let state_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM state_snapshots")
        .fetch_one(&mut *tx)
        .await
        .expect("empty-string workspace GUC on state_snapshots must not error");
    assert_eq!(state_count, 0);
    tx.rollback().await.expect("rollback");
}
