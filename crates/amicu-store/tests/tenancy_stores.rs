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

use amicu_auth::refresh::{RefreshStore, mint};
use amicu_auth::user::{User, UserStore};
use amicu_auth::{AuthError, Tier};
use amicu_store::{
    OrgMutationError, PostgresInviteStore, PostgresMembershipStore, PostgresRefreshStore,
    PostgresUserStore, PostgresWorkspaceStore, Role, WorkspaceId, connect, run_migrations,
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

/// andypalmi/amicu-planning#129: the token issuer's membership seam over
/// Postgres. `context` resolves the caller's role + the WORKSPACE's tier for a
/// mint/refresh; `list_for_user` backs `GET /workspaces`. Both are the
/// server-side authority - a removal/role-change is visible immediately, which
/// is what refresh re-validation relies on.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn membership_context_and_list_resolve_role_and_tier() {
    use amicu_auth::membership::MembershipStore as _;

    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared);

    let user = seed_user(&users).await;
    let personal = personal_ws(&members, &user).await;

    // context: Owner of the personal workspace, at that workspace's tier.
    let ctx = members
        .context(personal, &user)
        .await
        .expect("context")
        .expect("member of personal");
    assert_eq!(ctx.role, Role::Owner);
    assert_eq!(ctx.tier, Tier::Community);

    // Not a member of an unrelated workspace -> None (no token would be minted).
    let other = workspaces
        .create("Team", Tier::Pro)
        .await
        .expect("create ws");
    assert!(
        members
            .context(other, &user)
            .await
            .expect("context")
            .is_none()
    );

    // Joined as Admin: context reflects the role AND the target workspace's tier.
    members
        .add(other, &user, Role::Admin, None)
        .await
        .expect("add member");
    let ctx2 = members
        .context(other, &user)
        .await
        .expect("context")
        .expect("member of other");
    assert_eq!(ctx2.role, Role::Admin);
    assert_eq!(ctx2.tier, Tier::Pro);

    // A role change is visible at once (refresh would re-mint with it).
    members
        .set_role(other, &user, Role::Viewer)
        .await
        .expect("set role");
    assert_eq!(
        members
            .context(other, &user)
            .await
            .expect("context")
            .expect("still a member")
            .role,
        Role::Viewer
    );

    // list_for_user: exactly the caller's two memberships, carrying names.
    let mine = members.list_for_user(&user).await.expect("list");
    assert_eq!(mine.len(), 2);
    assert!(mine.iter().all(|m| !m.name.is_empty()));

    // Removal drops it from context and the list (refresh would then fail).
    members.remove(other, &user).await.expect("remove");
    assert!(
        members
            .context(other, &user)
            .await
            .expect("context")
            .is_none()
    );
    let after = members.list_for_user(&user).await.expect("list");
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].workspace_id, personal);
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

/// The personal workspace id a freshly-seeded user is Owner of.
async fn personal_ws(members: &PostgresMembershipStore, user: &str) -> WorkspaceId {
    let mut mine = members
        .list_workspaces(user)
        .await
        .expect("list workspaces");
    assert_eq!(mine.len(), 1, "fresh user has exactly one workspace");
    mine.pop().expect("one workspace").workspace_id
}

// ---------------------------------------------------------------------------
// FIX S1: UserStore::delete is fail-CLOSED.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn delete_plain_single_workspace_user_succeeds_and_cascades_tokens() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let refresh = PostgresRefreshStore::new(shared);

    let user = seed_user(&users).await;
    let ws = personal_ws(&members, &user).await;

    // Give them a refresh token so we can confirm the cascade.
    let token = mint(&user, ws, FAR_FUTURE);
    let hash = token.record.token_hash;
    refresh.insert(token.record).await.expect("insert refresh");

    // A user who only owns their own (unshared) personal workspace deletes fine.
    users.delete(&user).await.expect("delete succeeds");

    // User, their token, and the vacated personal workspace are all gone.
    assert!(users.get_by_id(&user).await.expect("lookup").is_none());
    assert!(refresh.get(&hash).await.expect("get").is_none());
    assert!(workspaces.get(ws).await.expect("get").is_none());
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn delete_is_refused_when_user_owns_a_second_shared_workspace() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared);

    let owner = seed_user(&users).await;
    let other = seed_user(&users).await;

    // A second, SHARED workspace the user is sole Owner of.
    let team = workspaces
        .create(&unique("Team"), Tier::Pro)
        .await
        .expect("create workspace");
    members
        .add(team, &owner, Role::Owner, None)
        .await
        .expect("owner of team");
    members
        .add(team, &other, Role::Member, Some(&owner))
        .await
        .expect("other member");

    // Deleting the owner would orphan the shared workspace -> refused, nothing
    // deleted. The user and the other member's membership both survive.
    assert!(matches!(
        users.delete(&owner).await,
        Err(AuthError::SoleOwner)
    ));
    assert!(users.get_by_id(&owner).await.expect("lookup").is_some());
    assert_eq!(
        members.role_of(team, &other).await.expect("role_of"),
        Some(Role::Member),
        "the other member must NOT be cascade-deleted"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn delete_is_refused_when_personal_workspace_has_another_member() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared);

    let user = seed_user(&users).await;
    let guest = seed_user(&users).await;
    let ws = personal_ws(&members, &user).await;

    // Someone else was added into the user's personal workspace.
    members
        .add(ws, &guest, Role::Member, Some(&user))
        .await
        .expect("guest joins personal ws");

    // Deleting the user would cascade the guest's membership away (and drop a
    // still-occupied workspace) -> refused by the RESTRICT FK, nothing deleted.
    assert!(matches!(
        users.delete(&user).await,
        Err(AuthError::SoleOwner)
    ));
    assert!(users.get_by_id(&user).await.expect("lookup").is_some());
    assert_eq!(
        members.role_of(ws, &guest).await.expect("role_of"),
        Some(Role::Member),
        "the guest's membership must NOT be cascade-deleted"
    );
}

// ---------------------------------------------------------------------------
// Org console: audited mutations re-check RBAC in the mutation's own
// transaction, write their audit record atomically, and stay workspace-scoped
// (andypalmi/amicu-planning#130). Carries the #117 in-memory TOCTOU fix onto
// Postgres.
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn org_console_audited_mutations_recheck_roles_and_audit_atomically() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared);

    let ws = workspaces
        .create(&unique("Org"), Tier::Enterprise)
        .await
        .expect("create org workspace");
    let owner = seed_user(&users).await;
    let admin = seed_user(&users).await;
    let target = seed_user(&users).await;
    members
        .add(ws, &owner, Role::Owner, None)
        .await
        .expect("seed owner");
    members
        .add(ws, &admin, Role::Admin, Some(&owner))
        .await
        .expect("seed admin");
    members
        .add(ws, &target, Role::Member, Some(&owner))
        .await
        .expect("seed target member");

    // Admin may add a plain Member (authorized against the freshly-read Admin
    // role), and it is audited.
    let newbie = seed_user(&users).await;
    let assigned = members
        .add_member_audited(ws, &admin, &newbie, Role::Member)
        .await
        .expect("admin adds a member");
    assert_eq!(assigned, Role::Member);
    assert_eq!(
        members.role_of(ws, &newbie).await.expect("role_of"),
        Some(Role::Member)
    );

    // Admin may NOT grant a privileged role.
    let candidate = seed_user(&users).await;
    assert!(matches!(
        members
            .add_member_audited(ws, &admin, &candidate, Role::Admin)
            .await
            .unwrap_err(),
        OrgMutationError::Forbidden(_)
    ));

    // The mutation re-reads the ACTOR fresh: demote the admin to member, then
    // their in-flight privileged mutation is refused - the in-transaction
    // re-read, not a stale caller role, decides.
    members
        .set_role(ws, &admin, Role::Member)
        .await
        .expect("demote admin");
    assert!(matches!(
        members
            .add_member_audited(ws, &admin, &candidate, Role::Member)
            .await
            .unwrap_err(),
        OrgMutationError::Forbidden(_)
    ));

    // A REMOVED actor is refused entirely ("not a member").
    members.remove(ws, &admin).await.expect("remove ex-admin");
    match members
        .add_member_audited(ws, &admin, &candidate, Role::Member)
        .await
        .unwrap_err()
    {
        OrgMutationError::Forbidden(msg) => assert!(msg.contains("not a member")),
        other => panic!("expected Forbidden(not a member), got {other:?}"),
    }

    // Owner promotes the target to Admin (audited), then the last-Owner guard
    // refuses demoting/removing the sole Owner.
    members
        .set_role_audited(ws, &owner, &target, Role::Admin)
        .await
        .expect("owner promotes target");
    assert!(matches!(
        members
            .set_role_audited(ws, &owner, &owner, Role::Member)
            .await
            .unwrap_err(),
        OrgMutationError::LastOwner
    ));
    assert!(matches!(
        members
            .remove_member_audited(ws, &owner, &owner)
            .await
            .unwrap_err(),
        OrgMutationError::LastOwner
    ));

    // Exactly the two SUCCESSFUL audited mutations landed a record (the admin's
    // add of `newbie`, and the owner's promotion of `target`); every refused
    // mutation wrote nothing. Most-recent first, so the promotion leads.
    let audit = members.org_audit(ws).await.expect("audit");
    assert_eq!(
        audit.len(),
        2,
        "only the two successful audited mutations recorded"
    );
    assert_eq!(
        audit.first().unwrap().action,
        amicu_store::OrgAuditAction::RoleChanged
    );
    assert_eq!(audit[1].action, amicu_store::OrgAuditAction::MemberAdded);
    assert!(
        audit
            .iter()
            .all(|e| e.actor_id == owner || e.actor_id == admin)
    );

    // A different workspace's audit log is empty (workspace_id predicate + RLS).
    let ws2 = workspaces
        .create(&unique("Org2"), Tier::Enterprise)
        .await
        .expect("create ws2");
    assert!(members.org_audit(ws2).await.expect("audit ws2").is_empty());
}

// ---------------------------------------------------------------------------
// Org console: the last-Owner invariant holds under CONCURRENT self-mutations
// (andypalmi/amicu-planning#130). Two Owners self-removing at the same instant
// lock DISJOINT membership rows, so without the per-workspace serializing
// advisory lock both `owner_count()` reads see 2, both pass the `== 1` guard,
// and both commit -> 0 Owners, workspace orphaned. The advisory lock taken
// first in every audited mutation forces the two read-modify-writes to run one
// at a time, so at most one self-removal succeeds. Looped so the timing-
// dependent race trips reliably if the lock regresses.
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn concurrent_self_removal_never_orphans_the_last_owner() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared);

    for _ in 0..10 {
        // A fresh workspace with EXACTLY two Owners.
        let ws = workspaces
            .create(&unique("Race"), Tier::Enterprise)
            .await
            .expect("create org workspace");
        let o1 = seed_user(&users).await;
        let o2 = seed_user(&users).await;
        members
            .add(ws, &o1, Role::Owner, None)
            .await
            .expect("seed owner 1");
        members
            .add(ws, &o2, Role::Owner, Some(&o1))
            .await
            .expect("seed owner 2");

        // Both Owners try to remove THEMSELVES at the same time. Each call opens
        // its own transaction/connection from the pool.
        let (r1, r2) = tokio::join!(
            members.remove_member_audited(ws, &o1, &o1),
            members.remove_member_audited(ws, &o2, &o2),
        );

        // At least one Owner must remain: at most one self-removal succeeds, the
        // other is refused with `LastOwner`. (Without the advisory lock, both can
        // commit and this reaches 0.)
        let remaining_owners = members
            .list_members(ws)
            .await
            .expect("list members")
            .into_iter()
            .filter(|m| m.role == Role::Owner)
            .count();
        assert!(
            remaining_owners >= 1,
            "the workspace was orphaned: {remaining_owners} Owners remain (r1={r1:?}, r2={r2:?})"
        );

        // Exactly one succeeded and exactly one saw LastOwner (both cannot
        // succeed, and both cannot fail - the workspace started with two Owners).
        let successes = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            successes, 1,
            "exactly one self-removal must succeed (r1={r1:?}, r2={r2:?})"
        );
        assert!(
            [&r1, &r2]
                .iter()
                .any(|r| matches!(r, Err(OrgMutationError::LastOwner))),
            "the losing self-removal must be refused with LastOwner (r1={r1:?}, r2={r2:?})"
        );
    }
}
