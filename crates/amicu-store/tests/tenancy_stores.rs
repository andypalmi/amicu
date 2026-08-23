//! Live-database integration tests for the tenancy data layer: the workspace
//! registry, membership store, and invite store.
//!
//! `#[ignore]`d like the other live-DB tests. Run with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://amicu_app:amicu_app@localhost:5432/amicu \
//!     cargo test -p amicu-store -- --ignored
//! ```
//!
//! Connect as the non-superuser, non-BYPASSRLS `amicu_app` role (see
//! andypalmi/amicu-planning#114) to exercise the row-level-security backstop on
//! workspace_members / workspace_invites in addition to the always-on query
//! predicates that make these tests pass under any role.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_auth::Tier;
use amicu_auth::user::{User, UserStore};
use amicu_store::{
    PostgresInviteStore, PostgresMembershipStore, PostgresUserStore, PostgresWorkspaceStore, Role,
    WorkspaceId, connect, run_migrations,
};
use sqlx::PgPool;
use uuid::Uuid;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos}_{n}")
}

const FAR_FUTURE: i64 = 9_999_999_999;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the ignored store tests");
    let pool = connect(&url).await.expect("connect to Postgres");
    run_migrations(&pool).await.expect("apply migrations");
    pool
}

/// Create a fresh global user; return their UUID id text.
async fn seed_user(store: &PostgresUserStore) -> String {
    let id = Uuid::new_v4().to_string();
    let email = format!("{}@example.test", unique("mail"));
    store
        .create(User::new(
            id.clone(),
            email,
            "argon2-phc".to_string(),
            Tier::Community,
        ))
        .await
        .expect("create user");
    id
}

/// A distinct 32-byte token hash per call.
fn token_hash(seed: u8) -> [u8; 32] {
    let mut h = [seed; 32];
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    h[..8].copy_from_slice(&n.to_le_bytes());
    h
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn signup_provisions_personal_owner_workspace_atomically() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared);

    let user = seed_user(&users).await;

    // The user is Owner of exactly one workspace: their personal one, created
    // in the same transaction as the user row.
    let mine = members
        .list_workspaces(&user)
        .await
        .expect("list workspaces");
    assert_eq!(mine.len(), 1, "exactly one personal workspace");
    assert_eq!(mine[0].role, Role::Owner);
    assert_eq!(mine[0].user_id, user);

    let ws = mine[0].workspace_id;
    let record = workspaces.get(ws).await.expect("get").expect("present");
    assert_eq!(record.name, "Personal workspace");
    assert_eq!(record.tier, Tier::Community);

    // role_of confirms the membership from the workspace direction too.
    assert_eq!(
        members.role_of(ws, &user).await.expect("role_of"),
        Some(Role::Owner)
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn workspace_store_create_get_set_tier() {
    let workspaces = PostgresWorkspaceStore::new(pool().await);

    let ws = workspaces
        .create(&unique("Acme"), Tier::Pro)
        .await
        .expect("create workspace");
    let got = workspaces.get(ws).await.expect("get").expect("present");
    assert_eq!(got.id, ws);
    assert_eq!(got.tier, Tier::Pro);
    assert!(got.slug.starts_with("ws-"));

    workspaces
        .set_tier(ws, Tier::Enterprise)
        .await
        .expect("set tier");
    let got = workspaces.get(ws).await.expect("get").expect("present");
    assert_eq!(got.tier, Tier::Enterprise);

    // Unknown workspace: get misses, set_tier errors.
    assert!(
        workspaces
            .get(WorkspaceId::new())
            .await
            .expect("get")
            .is_none()
    );
    assert!(
        workspaces
            .set_tier(WorkspaceId::new(), Tier::Pro)
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn membership_store_add_role_and_remove() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared);

    let ws = workspaces
        .create(&unique("Team"), Tier::Pro)
        .await
        .expect("create workspace");
    let owner = seed_user(&users).await;
    let member = seed_user(&users).await;

    members
        .add(ws, &owner, Role::Owner, None)
        .await
        .expect("add owner");
    members
        .add(ws, &member, Role::Member, Some(&owner))
        .await
        .expect("add member");

    // Duplicate add is rejected.
    assert!(matches!(
        members.add(ws, &member, Role::Member, Some(&owner)).await,
        Err(amicu_auth::AuthError::UserExists)
    ));

    let m = members
        .get(ws, &member)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(m.role, Role::Member);
    assert_eq!(m.invited_by.as_deref(), Some(owner.as_str()));

    // list_members returns both (this workspace only).
    let list = members.list_members(ws).await.expect("list members");
    assert_eq!(list.len(), 2);

    // Promote, then remove.
    members
        .set_role(ws, &member, Role::Admin)
        .await
        .expect("set role");
    assert_eq!(
        members.role_of(ws, &member).await.expect("role_of"),
        Some(Role::Admin)
    );
    members.remove(ws, &member).await.expect("remove");
    assert_eq!(members.role_of(ws, &member).await.expect("role_of"), None);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn membership_store_is_isolated_across_workspaces() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared);

    let ws_a = workspaces
        .create(&unique("A"), Tier::Pro)
        .await
        .expect("ws a");
    let ws_b = workspaces
        .create(&unique("B"), Tier::Pro)
        .await
        .expect("ws b");
    let alice = seed_user(&users).await;
    let bob = seed_user(&users).await;

    members
        .add(ws_a, &alice, Role::Member, None)
        .await
        .expect("alice in a");
    members
        .add(ws_b, &bob, Role::Member, None)
        .await
        .expect("bob in b");

    // A store call scoped to workspace A sees only A's members - never B's.
    let a_members = members.list_members(ws_a).await.expect("list a");
    assert_eq!(a_members.len(), 1);
    assert_eq!(a_members[0].user_id, alice);

    // Bob (a member of B) is invisible from A, and Alice from B.
    assert!(members.get(ws_a, &bob).await.expect("get").is_none());
    assert!(members.get(ws_b, &alice).await.expect("get").is_none());
    assert_eq!(members.role_of(ws_a, &bob).await.expect("role_of"), None);

    // The user-scoped view returns each user's own workspace membership only.
    let bob_ws: Vec<WorkspaceId> = members
        .list_workspaces(&bob)
        .await
        .expect("bob workspaces")
        .into_iter()
        .map(|m| m.workspace_id)
        .collect();
    assert!(bob_ws.contains(&ws_b));
    assert!(!bob_ws.contains(&ws_a));
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn invite_store_create_find_accept_expire() {
    let shared = pool().await;
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let invites = PostgresInviteStore::new(shared);

    let ws = workspaces
        .create(&unique("Invited"), Tier::Pro)
        .await
        .expect("create workspace");
    let email = format!("{}@example.test", unique("invitee"));
    let hash = token_hash(0x11);

    let id = invites
        .create(ws, &email, Role::Admin, &hash, FAR_FUTURE)
        .await
        .expect("create invite");

    // Redeemed by the token alone, without knowing the workspace up front.
    let found = invites
        .find_by_token_hash(&hash)
        .await
        .expect("find")
        .expect("present");
    assert_eq!(found.id, id);
    assert_eq!(found.workspace_id, ws);
    assert_eq!(found.email, email);
    assert_eq!(found.role, Role::Admin);
    assert!(found.accepted_at.is_none());

    // An unknown token maps to nothing.
    assert!(
        invites
            .find_by_token_hash(&token_hash(0x22))
            .await
            .expect("find")
            .is_none()
    );

    // Accept marks it, idempotently.
    invites.accept(ws, id, 1234).await.expect("accept");
    let found = invites
        .find_by_token_hash(&hash)
        .await
        .expect("find")
        .expect("present");
    assert_eq!(found.accepted_at, Some(1234));

    // Expire (revoke) deletes it.
    invites.expire(ws, id).await.expect("expire");
    assert!(
        invites
            .find_by_token_hash(&hash)
            .await
            .expect("find")
            .is_none()
    );
}
