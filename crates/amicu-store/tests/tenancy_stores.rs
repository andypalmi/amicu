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
    InviteError, OrgMutationError, PostgresInviteStore, PostgresMembershipStore,
    PostgresRefreshStore, PostgresUserStore, PostgresWorkspaceStore, Role, WorkspaceError,
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

/// A distinct 32-byte token hash per call, unique even ACROSS process runs
/// against the same persistent test database: the leading 16 bytes are random
/// (a fresh v4 UUID), so a new run never regenerates a prior run's hash and
/// trips the global `workspace_invites_token_hash` UNIQUE constraint. `seed`
/// tags the remaining bytes for readability at call sites.
fn token_hash(seed: u8) -> [u8; 32] {
    let mut h = [seed; 32];
    h[..16].copy_from_slice(Uuid::new_v4().as_bytes());
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
// Additional-workspace provisioning + invitations
// (andypalmi/amicu-planning#131). Server-authoritative ids, in-transaction actor
// RBAC re-checks, and invite acceptance that binds membership to the INVITE's
// workspace/role only. The RLS backstop (workspace_invites / workspace_members
// under the non-superuser amicu_app role) is exercised alongside the always-on
// workspace_id predicates.
// ---------------------------------------------------------------------------

/// `create_with_owner` provisions an ADDITIONAL workspace with a
/// SERVER-GENERATED id and enrolls the caller as founding Owner, atomically.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn create_with_owner_provisions_an_additional_owned_workspace() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared);

    let user = seed_user(&users).await;
    let personal = personal_ws(&members, &user).await;

    let created = workspaces
        .create_with_owner(&unique("Team"), &user)
        .await
        .expect("create additional workspace");

    // The id is freshly server-generated - never the personal one.
    assert_ne!(created, personal, "a distinct, server-generated workspace");

    // The caller is Owner, at the new workspace's default Community tier.
    let record = workspaces
        .get(created)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(record.tier, Tier::Community);
    assert_eq!(
        members.role_of(created, &user).await.expect("role_of"),
        Some(Role::Owner)
    );

    // list_workspaces now shows both (personal + the additional one).
    let mine = members.list_workspaces(&user).await.expect("list");
    let ids: Vec<WorkspaceId> = mine.iter().map(|m| m.workspace_id).collect();
    assert!(ids.contains(&personal) && ids.contains(&created));
    assert_eq!(mine.len(), 2);
}

/// Invite creation re-checks the ACTOR's CURRENT role inside the mutation's own
/// transaction (the #130 actor re-check, applied to invites): an Owner may
/// invite anyone; an Admin may not invite a privileged role; a
/// demoted-or-removed actor is refused even though they were privileged when the
/// request began.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn invite_create_rechecks_the_actor_role_in_transaction() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let invites = PostgresInviteStore::new(shared);

    let ws = workspaces
        .create(&unique("Org"), Tier::Enterprise)
        .await
        .expect("create workspace");
    let owner = seed_user(&users).await;
    let admin = seed_user(&users).await;
    members
        .add(ws, &owner, Role::Owner, None)
        .await
        .expect("seed owner");
    members
        .add(ws, &admin, Role::Admin, Some(&owner))
        .await
        .expect("seed admin");

    // Owner may invite a plain Member.
    invites
        .create_authorized(
            ws,
            &owner,
            "member@example.test",
            Role::Member,
            &token_hash(0x31),
            FAR_FUTURE,
        )
        .await
        .expect("owner invites a member");

    // Admin may invite a Member, but NOT a privileged (Owner/Admin) role.
    invites
        .create_authorized(
            ws,
            &admin,
            "member2@example.test",
            Role::Member,
            &token_hash(0x32),
            FAR_FUTURE,
        )
        .await
        .expect("admin invites a member");
    assert!(matches!(
        invites
            .create_authorized(
                ws,
                &admin,
                "boss@example.test",
                Role::Admin,
                &token_hash(0x33),
                FAR_FUTURE,
            )
            .await
            .unwrap_err(),
        InviteError::Forbidden(_)
    ));

    // Demote the admin to Member: their in-flight invite attempt is now refused
    // by the fresh in-transaction role read (a stale caller role would let it
    // through).
    members
        .set_role(ws, &admin, Role::Member)
        .await
        .expect("demote admin");
    assert!(matches!(
        invites
            .create_authorized(
                ws,
                &admin,
                "late@example.test",
                Role::Member,
                &token_hash(0x34),
                FAR_FUTURE,
            )
            .await
            .unwrap_err(),
        InviteError::Forbidden(_)
    ));

    // A REMOVED actor is refused entirely ("not a member").
    members.remove(ws, &admin).await.expect("remove ex-admin");
    match invites
        .create_authorized(
            ws,
            &admin,
            "later@example.test",
            Role::Member,
            &token_hash(0x35),
            FAR_FUTURE,
        )
        .await
        .unwrap_err()
    {
        InviteError::Forbidden(msg) => assert!(msg.contains("not a member")),
        other => panic!("expected Forbidden(not a member), got {other:?}"),
    }
}

/// Acceptance binds membership to the INVITE's workspace and role ONLY - never a
/// value the redeemer could assert - and is single-use (a second redemption of
/// the same token is refused).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn invite_accept_binds_membership_to_the_invites_workspace_and_role() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let invites = PostgresInviteStore::new(shared);

    let ws = workspaces
        .create(&unique("Invited"), Tier::Enterprise)
        .await
        .expect("create workspace");
    let owner = seed_user(&users).await;
    members
        .add(ws, &owner, Role::Owner, None)
        .await
        .expect("seed owner");
    let newcomer = seed_user(&users).await;

    let hash = token_hash(0x41);
    invites
        .create_authorized(
            ws,
            &owner,
            "invitee@example.test",
            Role::Admin,
            &hash,
            FAR_FUTURE,
        )
        .await
        .expect("owner invites an admin");

    // The invitee redeems the token (knowing only the token) and is enrolled into
    // the invite's workspace at the invite's role.
    let accepted = invites
        .accept_and_enroll(&newcomer, &hash, 1_000)
        .await
        .expect("accept");
    assert_eq!(accepted.workspace_id, ws);
    assert_eq!(accepted.role, Role::Admin);
    assert_eq!(
        members.role_of(ws, &newcomer).await.expect("role_of"),
        Some(Role::Admin),
        "membership bound to the invite's workspace + role"
    );

    // Single-use: the token no longer redeems (it is now accepted).
    assert!(matches!(
        invites
            .accept_and_enroll(&newcomer, &hash, 1_000)
            .await
            .unwrap_err(),
        InviteError::Invalid
    ));

    // The pending list no longer shows the accepted invite.
    assert!(
        invites
            .list_pending(ws)
            .await
            .expect("list pending")
            .is_empty()
    );
}

/// An expired invite cannot be redeemed, and a user already in the workspace
/// cannot re-join via an invite (the whole redemption rolls back, leaving the
/// invite pending).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn invite_accept_rejects_expired_and_already_member() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let invites = PostgresInviteStore::new(shared);

    let ws = workspaces
        .create(&unique("Org"), Tier::Enterprise)
        .await
        .expect("create workspace");
    let owner = seed_user(&users).await;
    members
        .add(ws, &owner, Role::Owner, None)
        .await
        .expect("seed owner");

    // Expired invite -> Invalid.
    let expired = token_hash(0x51);
    invites
        .create_authorized(ws, &owner, "late@example.test", Role::Member, &expired, 500)
        .await
        .expect("create expiring invite");
    let joiner = seed_user(&users).await;
    assert!(matches!(
        invites
            .accept_and_enroll(&joiner, &expired, 1_000)
            .await
            .unwrap_err(),
        InviteError::Invalid
    ));
    assert_eq!(members.role_of(ws, &joiner).await.expect("role_of"), None);

    // Already a member -> AlreadyMember, invite stays pending (rolled back).
    let member = seed_user(&users).await;
    members
        .add(ws, &member, Role::Member, Some(&owner))
        .await
        .expect("seed member");
    let live = token_hash(0x52);
    let id = invites
        .create_authorized(
            ws,
            &owner,
            "dup@example.test",
            Role::Member,
            &live,
            FAR_FUTURE,
        )
        .await
        .expect("create live invite");
    assert!(matches!(
        invites
            .accept_and_enroll(&member, &live, 1_000)
            .await
            .unwrap_err(),
        InviteError::AlreadyMember
    ));
    // Still pending (accepted_at was rolled back with the failed member insert).
    let pending = invites.list_pending(ws).await.expect("list pending");
    assert!(
        pending
            .iter()
            .any(|i| i.id == id && i.accepted_at.is_none())
    );
}

/// RLS BACKSTOP: pending invites are workspace-scoped even under the
/// non-superuser `amicu_app` role. A listing scoped to workspace A never returns
/// workspace B's invites (the workspace_id predicate + FORCE RLS on
/// workspace_invites), and redemption still finds the one invite the token maps
/// to across workspaces.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn invites_are_isolated_across_workspaces() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let invites = PostgresInviteStore::new(shared);

    let owner_a = seed_user(&users).await;
    let owner_b = seed_user(&users).await;
    let ws_a = workspaces
        .create(&unique("A"), Tier::Enterprise)
        .await
        .expect("ws a");
    let ws_b = workspaces
        .create(&unique("B"), Tier::Enterprise)
        .await
        .expect("ws b");
    members
        .add(ws_a, &owner_a, Role::Owner, None)
        .await
        .expect("owner a");
    members
        .add(ws_b, &owner_b, Role::Owner, None)
        .await
        .expect("owner b");

    let hash_a = token_hash(0x61);
    let hash_b = token_hash(0x62);
    invites
        .create_authorized(
            ws_a,
            &owner_a,
            "a@example.test",
            Role::Member,
            &hash_a,
            FAR_FUTURE,
        )
        .await
        .expect("invite into a");
    invites
        .create_authorized(
            ws_b,
            &owner_b,
            "b@example.test",
            Role::Member,
            &hash_b,
            FAR_FUTURE,
        )
        .await
        .expect("invite into b");

    // A's pending list shows only A's invite; B's shows only B's.
    let a_pending = invites.list_pending(ws_a).await.expect("list a");
    assert_eq!(a_pending.len(), 1);
    assert_eq!(a_pending[0].email, "a@example.test");
    let b_pending = invites.list_pending(ws_b).await.expect("list b");
    assert_eq!(b_pending.len(), 1);
    assert_eq!(b_pending[0].email, "b@example.test");

    // Redemption by token alone finds exactly the one invite it maps to, and its
    // workspace is the correct one - never the other tenant's.
    let found_a = invites
        .find_by_token_hash(&hash_a)
        .await
        .expect("find")
        .expect("present");
    assert_eq!(found_a.workspace_id, ws_a);
    let found_b = invites
        .find_by_token_hash(&hash_b)
        .await
        .expect("find")
        .expect("present");
    assert_eq!(found_b.workspace_id, ws_b);
}

/// Invite REVOCATION (andypalmi/amicu-planning#131): an Owner/Admin may revoke a
/// PENDING invite in their own workspace, re-checked against the actor's CURRENT
/// role in the store's transaction; after revocation the token no longer
/// redeems; a non-manager is refused; and a revoke targeting another workspace's
/// invite, an unknown id, or an already-accepted invite is `NotFound`. Every
/// lifecycle step (create/accept/revoke) also writes its `org_audit` row.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
#[allow(clippy::too_many_lines)] // one end-to-end walk of the revoke lifecycle
async fn invite_revoke_is_authorized_workspace_scoped_and_audited() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let invites = PostgresInviteStore::new(shared);

    let ws = workspaces
        .create(&unique("Org"), Tier::Enterprise)
        .await
        .expect("create workspace");
    let owner = seed_user(&users).await;
    let admin = seed_user(&users).await;
    let member = seed_user(&users).await;
    members
        .add(ws, &owner, Role::Owner, None)
        .await
        .expect("seed owner");
    members
        .add(ws, &admin, Role::Admin, Some(&owner))
        .await
        .expect("seed admin");
    members
        .add(ws, &member, Role::Member, Some(&owner))
        .await
        .expect("seed member");

    // Owner mints an invite; a Member may NOT revoke it (403), then the Owner can.
    let hash = token_hash(0x71);
    let owner_invite = invites
        .create_authorized(
            ws,
            &owner,
            "a@example.test",
            Role::Member,
            &hash,
            FAR_FUTURE,
        )
        .await
        .expect("owner invites");
    assert!(matches!(
        invites
            .revoke_authorized(ws, &member, owner_invite)
            .await
            .unwrap_err(),
        InviteError::Forbidden(_)
    ));
    // Still pending after the refused revoke.
    assert_eq!(invites.list_pending(ws).await.expect("pending").len(), 1);

    invites
        .revoke_authorized(ws, &owner, owner_invite)
        .await
        .expect("owner revokes");
    // Gone from the pending list, and the token no longer redeems.
    assert!(invites.list_pending(ws).await.expect("pending").is_empty());
    let joiner = seed_user(&users).await;
    assert!(matches!(
        invites
            .accept_and_enroll(&joiner, &hash, 1_000)
            .await
            .unwrap_err(),
        InviteError::Invalid
    ));

    // An Admin may also revoke a pending invite in their workspace.
    let admin_hash = token_hash(0x72);
    let admin_invite = invites
        .create_authorized(
            ws,
            &admin,
            "b@example.test",
            Role::Member,
            &admin_hash,
            FAR_FUTURE,
        )
        .await
        .expect("admin invites");
    invites
        .revoke_authorized(ws, &admin, admin_invite)
        .await
        .expect("admin revokes");

    // Revoking an unknown id, or one already accepted, is NotFound.
    assert!(matches!(
        invites
            .revoke_authorized(ws, &owner, Uuid::new_v4())
            .await
            .unwrap_err(),
        InviteError::NotFound
    ));
    let accepted_hash = token_hash(0x73);
    let accepted_invite = invites
        .create_authorized(
            ws,
            &owner,
            "c@example.test",
            Role::Member,
            &accepted_hash,
            FAR_FUTURE,
        )
        .await
        .expect("owner invites c");
    let comer = seed_user(&users).await;
    invites
        .accept_and_enroll(&comer, &accepted_hash, 1_000)
        .await
        .expect("accept c");
    assert!(
        matches!(
            invites
                .revoke_authorized(ws, &owner, accepted_invite)
                .await
                .unwrap_err(),
            InviteError::NotFound
        ),
        "an already-accepted invite is not a pending invite to revoke"
    );

    // A demoted actor cannot revoke on their stale role (in-tx re-check).
    let demoted_hash = token_hash(0x74);
    let demoted_invite = invites
        .create_authorized(
            ws,
            &owner,
            "d@example.test",
            Role::Member,
            &demoted_hash,
            FAR_FUTURE,
        )
        .await
        .expect("owner invites d");
    members
        .set_role(ws, &admin, Role::Member)
        .await
        .expect("demote admin");
    assert!(matches!(
        invites
            .revoke_authorized(ws, &admin, demoted_invite)
            .await
            .unwrap_err(),
        InviteError::Forbidden(_)
    ));

    // The whole lifecycle was audited: create(a,b,c,d) + accept(c) + revoke(a,b).
    let audit = members.org_audit(ws).await.expect("audit");
    let created = audit
        .iter()
        .filter(|e| e.action == amicu_store::OrgAuditAction::InviteCreated)
        .count();
    let accepted = audit
        .iter()
        .filter(|e| e.action == amicu_store::OrgAuditAction::InviteAccepted)
        .count();
    let revoked = audit
        .iter()
        .filter(|e| e.action == amicu_store::OrgAuditAction::InviteRevoked)
        .count();
    assert_eq!(created, 4, "four invites created were each audited");
    assert_eq!(accepted, 1, "the one acceptance was audited");
    assert_eq!(revoked, 2, "the two revocations were audited");
}

/// RLS BACKSTOP for revocation: a revoke scoped to workspace A can NEVER remove
/// workspace B's invite. B's invite stays pending and still redeems, and the
/// A-scoped call reports the same indistinct `NotFound` it would for any unknown
/// id (no cross-tenant existence leak).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn invite_revoke_cannot_touch_another_workspaces_invite() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let invites = PostgresInviteStore::new(shared);

    let owner_a = seed_user(&users).await;
    let owner_b = seed_user(&users).await;
    let ws_a = workspaces
        .create(&unique("A"), Tier::Enterprise)
        .await
        .expect("ws a");
    let ws_b = workspaces
        .create(&unique("B"), Tier::Enterprise)
        .await
        .expect("ws b");
    members
        .add(ws_a, &owner_a, Role::Owner, None)
        .await
        .expect("owner a");
    members
        .add(ws_b, &owner_b, Role::Owner, None)
        .await
        .expect("owner b");

    let hash_b = token_hash(0x81);
    let invite_b = invites
        .create_authorized(
            ws_b,
            &owner_b,
            "b@example.test",
            Role::Member,
            &hash_b,
            FAR_FUTURE,
        )
        .await
        .expect("invite into b");

    // A's Owner tries to revoke B's invite (naming B's real invite id) while
    // scoped to A: nothing is deleted, reported as NotFound.
    assert!(matches!(
        invites
            .revoke_authorized(ws_a, &owner_a, invite_b)
            .await
            .unwrap_err(),
        InviteError::NotFound
    ));

    // B's invite is untouched: still pending and still redeems.
    assert_eq!(
        invites.list_pending(ws_b).await.expect("pending b").len(),
        1
    );
    let comer = seed_user(&users).await;
    let accepted = invites
        .accept_and_enroll(&comer, &hash_b, 1_000)
        .await
        .expect("B's invite still redeems");
    assert_eq!(accepted.workspace_id, ws_b);
}

/// CONCURRENCY (andypalmi/amicu-planning#131): two redemptions of the SAME token
/// fired at once (each its own connection/transaction from the pool) must resolve
/// to EXACTLY ONE `Ok` - the other losing the atomic
/// `UPDATE ... WHERE accepted_at IS NULL ... RETURNING` claim (`Invalid`) or the
/// membership uniqueness (`AlreadyMember`) - and the joiner must end with
/// EXACTLY ONE membership row. Looped so a regression of the single-use claim
/// trips reliably.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn concurrent_double_accept_enrolls_exactly_once() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let invites = PostgresInviteStore::new(shared);

    let ws = workspaces
        .create(&unique("Race"), Tier::Enterprise)
        .await
        .expect("create workspace");
    let owner = seed_user(&users).await;
    members
        .add(ws, &owner, Role::Owner, None)
        .await
        .expect("seed owner");

    for i in 0..10u8 {
        // A fresh invite + token + joiner each iteration.
        let hash = token_hash(0x90 ^ i);
        invites
            .create_authorized(
                ws,
                &owner,
                "race@example.test",
                Role::Member,
                &hash,
                FAR_FUTURE,
            )
            .await
            .expect("create invite");
        let joiner = seed_user(&users).await;

        // Two concurrent redemptions of the SAME token, each its own tx.
        let (r1, r2) = tokio::join!(
            invites.accept_and_enroll(&joiner, &hash, 1_000),
            invites.accept_and_enroll(&joiner, &hash, 1_000),
        );

        // Exactly one succeeds; the other is refused (Invalid = lost the claim, or
        // AlreadyMember = lost the membership insert).
        let successes = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            successes, 1,
            "exactly one concurrent accept must succeed (r1={r1:?}, r2={r2:?})"
        );
        assert!(
            [&r1, &r2]
                .iter()
                .any(|r| matches!(r, Err(InviteError::Invalid | InviteError::AlreadyMember))),
            "the losing accept must be Invalid or AlreadyMember (r1={r1:?}, r2={r2:?})"
        );

        // The joiner has EXACTLY ONE membership row in the invite's workspace.
        let member_rows = members
            .list_members(ws)
            .await
            .expect("list members")
            .into_iter()
            .filter(|m| m.user_id == joiner)
            .count();
        assert_eq!(
            member_rows, 1,
            "the joiner must be enrolled exactly once (r1={r1:?}, r2={r2:?})"
        );
    }
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

// ---------------------------------------------------------------------------
// N1: per-user workspace-creation cap (andypalmi/amicu-planning#132). There is
// no `DELETE /workspaces`, so this cap is a hard ceiling, enforced INSIDE
// `create_with_owner`'s own transaction under a per-owner advisory lock so it
// is exact under concurrency.
// ---------------------------------------------------------------------------

/// Creating up to the cap succeeds; the (cap+1)th `create_with_owner` for the
/// SAME owner is refused with `LimitReached` and inserts nothing; a DIFFERENT
/// owner is entirely unaffected by the first owner's cap.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn create_with_owner_enforces_the_per_user_cap() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    // A small cap (2) to exercise the limit cheaply.
    let workspaces = PostgresWorkspaceStore::new(shared).with_max_workspaces_per_user(2);

    // Sign-up already made `owner` the founding Owner of ONE (personal)
    // workspace, so they start at 1/2.
    let owner = seed_user(&users).await;
    let other = seed_user(&users).await;

    // One more additional workspace brings the owner to exactly the cap (2/2)
    // and must still succeed.
    let second = workspaces
        .create_with_owner(&unique("Team"), &owner)
        .await
        .expect("create up to the cap");
    assert_eq!(
        members.role_of(second, &owner).await.expect("role_of"),
        Some(Role::Owner)
    );

    // The (cap+1)th create for the SAME owner is refused, and NOTHING is
    // inserted - `LimitReached` rolls the whole transaction back.
    let before = members.list_workspaces(&owner).await.expect("list before");
    assert_eq!(
        workspaces
            .create_with_owner(&unique("Team"), &owner)
            .await
            .unwrap_err(),
        WorkspaceError::LimitReached { max: 2 }
    );
    let after = members.list_workspaces(&owner).await.expect("list after");
    assert_eq!(
        before.len(),
        after.len(),
        "a refused create must insert nothing"
    );

    // A DIFFERENT owner (0/2 of their own) is entirely unaffected.
    let other_ws = workspaces
        .create_with_owner(&unique("Other"), &other)
        .await
        .expect("a different owner is unaffected by the first owner's cap");
    assert_eq!(
        members.role_of(other_ws, &other).await.expect("role_of"),
        Some(Role::Owner)
    );
}

// ---------------------------------------------------------------------------
// N3: close the tier-downgrade staleness window on org MUTATIONS
// (andypalmi/amicu-planning#132). The claim-based Enterprise gate keys on the
// access token's `claims.tier`, so a workspace DOWNGRADED from Enterprise
// still passes it until the token expires (<=5 min). Every audited mutation
// re-reads the workspace's CURRENT tier inside its own transaction and rejects
// with Forbidden if it no longer meets Enterprise - even for the Owner.
// `accept_and_enroll` is deliberately untested here: it is NOT gated (joining
// via invite must not depend on the workspace's tier).
// ---------------------------------------------------------------------------
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
#[allow(clippy::too_many_lines)] // one end-to-end walk of all five audited mutations
async fn audited_mutations_reject_on_a_downgraded_workspace_tier() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let members = PostgresMembershipStore::new(shared.clone());
    let workspaces = PostgresWorkspaceStore::new(shared.clone());
    let invites = PostgresInviteStore::new(shared.clone());

    let ws = workspaces
        .create(&unique("Org"), Tier::Enterprise)
        .await
        .expect("create enterprise workspace");
    let owner = seed_user(&users).await;
    let target = seed_user(&users).await;
    members
        .add(ws, &owner, Role::Owner, None)
        .await
        .expect("seed owner");
    members
        .add(ws, &target, Role::Member, Some(&owner))
        .await
        .expect("seed target");

    // Still Enterprise: the Owner's audited mutations all succeed normally.
    let newbie = seed_user(&users).await;
    members
        .add_member_audited(ws, &owner, &newbie, Role::Member)
        .await
        .expect("enterprise: add succeeds");
    let stale_invite = invites
        .create_authorized(
            ws,
            &owner,
            "still-pending@example.test",
            Role::Member,
            &token_hash(0x50),
            FAR_FUTURE,
        )
        .await
        .expect("enterprise: invite create succeeds");
    let revoked_invite = invites
        .create_authorized(
            ws,
            &owner,
            "revoked@example.test",
            Role::Member,
            &token_hash(0x51),
            FAR_FUTURE,
        )
        .await
        .expect("enterprise: second invite create succeeds");
    invites
        .revoke_authorized(ws, &owner, revoked_invite)
        .await
        .expect("enterprise: revoke succeeds");

    // Downgrade the workspace's tier directly in the DB - simulating a stale
    // Enterprise access token whose claim has not yet expired (the accepted
    // <=5-min window on the claim-based gate).
    sqlx::query!(
        "UPDATE workspaces SET tier = 'pro' WHERE id = $1",
        ws.as_uuid(),
    )
    .execute(&shared)
    .await
    .expect("downgrade to pro");

    // Every audited mutation is now refused - even for the Owner - and
    // workspace state is left unchanged.
    let candidate = seed_user(&users).await;
    assert!(matches!(
        members
            .add_member_audited(ws, &owner, &candidate, Role::Member)
            .await
            .unwrap_err(),
        OrgMutationError::Forbidden(_)
    ));
    assert_eq!(
        members.role_of(ws, &candidate).await.expect("role_of"),
        None
    );

    assert!(matches!(
        members
            .set_role_audited(ws, &owner, &target, Role::Admin)
            .await
            .unwrap_err(),
        OrgMutationError::Forbidden(_)
    ));
    assert_eq!(
        members.role_of(ws, &target).await.expect("role_of"),
        Some(Role::Member),
        "the refused role change must not have landed"
    );

    assert!(matches!(
        members
            .remove_member_audited(ws, &owner, &target)
            .await
            .unwrap_err(),
        OrgMutationError::Forbidden(_)
    ));
    assert_eq!(
        members.role_of(ws, &target).await.expect("role_of"),
        Some(Role::Member),
        "the refused removal must not have landed"
    );

    assert!(matches!(
        invites
            .create_authorized(
                ws,
                &owner,
                "late@example.test",
                Role::Member,
                &token_hash(0x52),
                FAR_FUTURE,
            )
            .await
            .unwrap_err(),
        InviteError::Forbidden(_)
    ));

    assert!(matches!(
        invites
            .revoke_authorized(ws, &owner, stale_invite)
            .await
            .unwrap_err(),
        InviteError::Forbidden(_)
    ));
    assert_eq!(
        invites.list_pending(ws).await.expect("list pending").len(),
        1,
        "the refused revoke must not have landed - still exactly one pending invite"
    );

    // Re-upgrading to Hosted (>= Enterprise) restores mutation access.
    sqlx::query!(
        "UPDATE workspaces SET tier = 'hosted' WHERE id = $1",
        ws.as_uuid(),
    )
    .execute(&shared)
    .await
    .expect("upgrade to hosted");
    members
        .set_role_audited(ws, &owner, &target, Role::Admin)
        .await
        .expect("hosted: role change succeeds again");
    invites
        .revoke_authorized(ws, &owner, stale_invite)
        .await
        .expect("hosted: revoke succeeds again");
    assert!(
        invites
            .list_pending(ws)
            .await
            .expect("list pending")
            .is_empty()
    );
}
