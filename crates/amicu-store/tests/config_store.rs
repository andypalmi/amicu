//! Live-database integration tests for the Postgres config store.
//!
//! These are `#[ignore]`d so `cargo test --workspace` skips them when no
//! Postgres is available (as in CI). Run them against a live database with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://amicu:amicu@localhost:5432/amicu \
//!     cargo test -p amicu-store -- --ignored
//! ```
//!
//! Every test uses a freshly-random config name so they can share one
//! database without colliding.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_store::config::ConfigError;
use amicu_store::{PostgresConfigStore, WorkspaceId, connect, run_migrations};
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

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn config_create_get_append_and_list_versions() {
    let store = PostgresConfigStore::new(pool().await);
    let name = unique("cfg");
    let author = Uuid::new_v4();

    let created = store
        .create(&name, "hcl v1", Some(author), Some("initial"))
        .await
        .expect("create config");
    assert_eq!(created.current_version, 1);
    assert_eq!(created.name, name);

    let current = store
        .get_current(created.id)
        .await
        .expect("get_current")
        .expect("config present");
    assert_eq!(current.content, "hcl v1");
    assert_eq!(current.current_version, 1);
    assert_eq!(
        current.content_sha256,
        <[u8; 32]>::from(sha2::Sha256::digest(b"hcl v1"))
    );

    let listed = store.list().await.expect("list");
    assert!(
        listed
            .iter()
            .any(|c| c.id == created.id && c.name == name && c.current_version == 1)
    );

    let appended = store
        .append_version(created.id, "hcl v2", Some(author), Some("second"))
        .await
        .expect("append_version");
    assert_eq!(appended.version_no, 2);
    assert_eq!(appended.created_by, Some(author));
    assert_eq!(appended.message.as_deref(), Some("second"));

    let current = store
        .get_current(created.id)
        .await
        .expect("get_current")
        .expect("config present");
    assert_eq!(current.content, "hcl v2");
    assert_eq!(current.current_version, 2);

    let versions = store
        .list_versions(created.id)
        .await
        .expect("list_versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].version_no, 1);
    assert_eq!(versions[1].version_no, 2);

    let v1 = store
        .get_version(created.id, 1)
        .await
        .expect("get_version")
        .expect("version 1 present");
    assert_eq!(v1.content, "hcl v1");
    assert_eq!(v1.message.as_deref(), Some("initial"));

    // An unknown config id is None/empty, not an error.
    let unknown = Uuid::new_v4();
    assert!(
        store
            .get_current(unknown)
            .await
            .expect("get_current unknown")
            .is_none()
    );
    assert!(
        store
            .list_versions(unknown)
            .await
            .expect("list_versions unknown")
            .is_empty()
    );
    assert!(
        store
            .get_version(unknown, 1)
            .await
            .expect("get_version unknown")
            .is_none()
    );

    // append_version against an unknown config id is an explicit error.
    assert!(matches!(
        store
            .append_version(unknown, "x", None, None)
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
        .create(&name, "hcl", None, None)
        .await
        .expect("create config");

    let dup = store.create(&name, "other hcl", None, None).await;
    assert!(matches!(dup, Err(ConfigError::DuplicateName(n)) if n == name));
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn config_rollback_creates_new_version_and_preserves_history() {
    let store = PostgresConfigStore::new(pool().await);
    let name = unique("cfg");

    let created = store
        .create(&name, "hcl v1", None, Some("v1"))
        .await
        .expect("create config");
    store
        .append_version(created.id, "hcl v2", None, Some("v2"))
        .await
        .expect("append v2");
    store
        .append_version(created.id, "hcl v3", None, Some("v3"))
        .await
        .expect("append v3");

    let before = store
        .list_versions(created.id)
        .await
        .expect("list_versions before rollback");
    assert_eq!(before.len(), 3);

    // Roll back to v1's content: this must mint a brand-new version 4, not
    // touch or remove versions 1-3.
    let rolled = store
        .rollback(created.id, 1)
        .await
        .expect("rollback to version 1");
    assert_eq!(rolled.version_no, 4);

    let after = store
        .list_versions(created.id)
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
        .get_version(created.id, 1)
        .await
        .expect("get_version 1")
        .expect("v1 still present");
    assert_eq!(v1.content, "hcl v1");

    // The new current version (4) carries v1's content, copied, not moved.
    let current = store
        .get_current(created.id)
        .await
        .expect("get_current")
        .expect("config present");
    assert_eq!(current.current_version, 4);
    assert_eq!(current.content, "hcl v1");

    // Rolling back to a version that never existed is a distinct error.
    assert!(matches!(
        store.rollback(created.id, 99).await.unwrap_err(),
        ConfigError::VersionNotFound(99)
    ));
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn config_is_invisible_to_another_workspace() {
    let shared = pool().await;
    let name = unique("cfg");

    // Create into the default (single-tenant) workspace.
    let default_store = PostgresConfigStore::new(shared.clone());
    let created = default_store
        .create(&name, "hcl", None, None)
        .await
        .expect("create config");

    // A store bound to a different workspace must not see the other tenant's
    // config at all: the workspace_id predicate (and the RLS backstop) scope
    // it out. configs.workspace_id FKs to workspaces(id), so the other
    // workspace needs a real row before it can write anything.
    let other_workspace = WorkspaceId::new();
    sqlx::query!(
        "INSERT INTO workspaces (id, slug, name, tier, created_at) \
         VALUES ($1, $2, 'other tenant', 'community', 0)",
        other_workspace.as_uuid(),
        unique("ws"),
    )
    .execute(&shared)
    .await
    .expect("seed other workspace");
    let other_store = PostgresConfigStore::with_workspace(shared, other_workspace);
    assert!(
        other_store
            .get_current(created.id)
            .await
            .expect("cross-tenant get_current")
            .is_none()
    );
    assert!(
        other_store
            .list_versions(created.id)
            .await
            .expect("cross-tenant list_versions")
            .is_empty()
    );
    assert!(
        !other_store
            .list()
            .await
            .expect("cross-tenant list")
            .iter()
            .any(|c| c.id == created.id)
    );

    // A same-named config can be created in the other workspace: the
    // UNIQUE (workspace_id, name) constraint is per-tenant, not global.
    other_store
        .create(&name, "other tenant hcl", None, None)
        .await
        .expect("create same name in other workspace");

    // Writes from the other workspace cannot touch the original tenant's row.
    assert!(matches!(
        other_store
            .append_version(created.id, "x", None, None)
            .await
            .unwrap_err(),
        ConfigError::NotFound
    ));
    // rollback looks up the source version first, scoped to this workspace;
    // since no such row exists here, it fails closed as VersionNotFound
    // rather than ever reading the other tenant's content.
    assert!(matches!(
        other_store.rollback(created.id, 1).await.unwrap_err(),
        ConfigError::VersionNotFound(1)
    ));

    // The config is still live for its own workspace.
    let current = default_store
        .get_current(created.id)
        .await
        .expect("get_current")
        .expect("config present");
    assert_eq!(current.content, "hcl");
}
