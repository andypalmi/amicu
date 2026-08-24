//! Live-database HTTP integration tests for additional-workspace provisioning
//! and workspace invitations (andypalmi/amicu-planning#131), the closing
//! vertical of the authenticated multi-tenant workspaces epic.
//!
//! These exercise the REAL router - `POST /workspaces`, `POST /org/invites`,
//! `GET /org/invites`, `POST /org/invites/accept` - end-to-end over the durable
//! Postgres tenancy stores, connected as the non-superuser `amicu_app` role so
//! FORCE row-level security engages behind the always-on workspace predicates
//! exactly as in production.
//!
//! `#[ignore]`d like the other live-DB tests. Run with:
//!
//! ```sh
//! DATABASE_URL=postgres://amicu_app:amicu_app@localhost:5432/amicu \
//!     cargo test -p amicu-api --test tenancy_http -- --ignored
//! ```

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]
// Test bindings share short stems (owner/owner_token, joiner/joined, ws/ws_a);
// the pedantic similar-names lint is noise here.
#![allow(clippy::similar_names)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_api::{OrgApiState, PostgresTenancyStore, SessionApiState, org_router, session_router};
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::service::AuthService;
use amicu_auth::user::{User, UserStore};
use amicu_auth::verifier::StaticJwksProvider;
use amicu_auth::{
    IssuerConfig, Keyring, Role, SigningKey, Tier, TokenIssuer, Verifier, VerifierConfig,
    WorkspaceId,
};
use amicu_store::{
    PostgresInviteStore, PostgresMembershipStore, PostgresRefreshStore, PostgresResetStore,
    PostgresUserStore, PostgresVerifyStore, PostgresWorkspaceStore, connect, run_migrations,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos}_{n}")
}

/// Everything a test needs: the merged router, an access-token minter, and the
/// backing stores (to seed workspaces/memberships and assert persisted state).
struct Harness {
    app: Router,
    issuer: TokenIssuer,
    mint_keyring: Keyring,
    users: PostgresUserStore,
    members: PostgresMembershipStore,
    workspaces: PostgresWorkspaceStore,
}

impl Harness {
    /// Mint an access token for `sub`, scoped to `ws` at `role`/`tier` - exactly
    /// what the issuer would mint after a sign-in/switch, so the gate accepts it
    /// and the claim carries the tenancy the handlers read. Signed by the same
    /// key the router's verifier trusts.
    fn token(&self, sub: &str, ws: WorkspaceId, role: Role, tier: Tier) -> String {
        self.issuer
            .issue_access(self.mint_keyring.active(), sub, ws, role, tier, Vec::new())
            .expect("mint token")
    }

    /// Create a global user (provisions their personal workspace); return the id.
    async fn new_user(&self) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let email = format!("{}@example.test", unique("mail"));
        self.users
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

    /// Provision an Enterprise workspace with `owner` as its Owner; return its id.
    async fn enterprise_ws_owned_by(&self, owner: &str) -> WorkspaceId {
        let ws = self
            .workspaces
            .create(&unique("Org"), Tier::Enterprise)
            .await
            .expect("create workspace");
        self.members
            .add(ws, owner, Role::Owner, None)
            .await
            .expect("seed owner");
        ws
    }

    /// The personal workspace a freshly-created user owns.
    async fn personal_ws(&self, user: &str) -> WorkspaceId {
        let mut mine = self
            .members
            .list_workspaces(user)
            .await
            .expect("list workspaces");
        assert_eq!(mine.len(), 1, "fresh user has one workspace");
        mine.pop().expect("one").workspace_id
    }
}

async fn harness() -> Harness {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for the ignored HTTP tests");
    let pool = connect(&url).await.expect("connect to Postgres");
    run_migrations(&pool).await.expect("apply migrations");

    let users = Arc::new(PostgresUserStore::new(pool.clone()));
    let members = Arc::new(PostgresMembershipStore::new(pool.clone()));

    // The keyring the harness mints with, and the JWKS the router's verifier
    // trusts. The AuthService is built with its own throwaway keyring: these
    // tests never hit a route that signs a token through the service (only the
    // tenancy + org handlers, which read the verifier), so the service's keyring
    // is inert - what matters is that the verifier trusts `mint_keyring`.
    let mint_keyring = Keyring::new(SigningKey::generate().expect("generate mint key"));
    let jwks = mint_keyring.jwks();
    let mint_issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));

    let service = Arc::new(
        AuthService::new(
            Keyring::new(SigningKey::generate().expect("generate service key")),
            TokenIssuer::new(IssuerConfig::new(ISS, AUD)),
            users.clone(),
            Arc::new(PostgresRefreshStore::new(pool.clone())),
            Arc::new(PostgresResetStore::new(pool.clone())),
            Arc::new(PostgresVerifyStore::new(pool.clone())),
            Arc::new(LoggingNotifier),
            ISS,
        )
        .with_membership_store(members.clone()),
    );

    let verifier = Arc::new(Verifier::new(
        StaticJwksProvider::new(jwks),
        VerifierConfig::new(ISS, AUD),
    ));

    let tenancy = Arc::new(PostgresTenancyStore::new(
        PostgresWorkspaceStore::new(pool.clone()),
        PostgresInviteStore::new(pool.clone()),
    ));

    let org_state =
        OrgApiState::new(verifier.clone(), members.clone()).with_tenancy(tenancy.clone());
    let session_state = SessionApiState::new(service, verifier).with_tenancy(tenancy);

    let app = org_router(org_state).merge(session_router(session_state));

    Harness {
        app,
        issuer: mint_issuer,
        mint_keyring,
        users: PostgresUserStore::new(pool.clone()),
        members: PostgresMembershipStore::new(pool.clone()),
        workspaces: PostgresWorkspaceStore::new(pool),
    }
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.expect("request");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, body)
}

fn get(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn post(path: &str, token: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn delete(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// POST /workspaces: server-generated id; caller-supplied id is ignored.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn create_workspace_ignores_caller_supplied_id_and_owns_the_result() {
    let h = harness().await;
    let user = h.new_user().await;
    let personal = h.personal_ws(&user).await;
    let token = h.token(&user, personal, Role::Owner, Tier::Community);

    // The caller tries to dictate the id (and even names an EXISTING workspace);
    // it must be ignored - the server generates a fresh id.
    let attacker_id = WorkspaceId::new();
    let (status, body) = send(
        &h.app,
        post(
            "/workspaces",
            &token,
            &json!({ "name": "Second Team", "workspace_id": attacker_id.to_string() }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["role"], "owner");
    let created: WorkspaceId = body["workspace_id"]
        .as_str()
        .expect("workspace_id string")
        .parse()
        .expect("valid workspace id");
    assert_ne!(created, attacker_id, "caller-supplied id must be ignored");
    assert_ne!(created, personal, "a fresh, distinct workspace");

    // The caller is the Owner of the newly-created workspace, and nothing landed
    // in the id they tried to supply.
    assert_eq!(
        h.members.role_of(created, &user).await.expect("role_of"),
        Some(Role::Owner)
    );
    assert_eq!(
        h.members
            .role_of(attacker_id, &user)
            .await
            .expect("role_of"),
        None,
        "the caller never lands in the id they asked for"
    );
}

// ---------------------------------------------------------------------------
// Invite lifecycle: create (Owner) -> list -> accept (binds to invite ws/role).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn invite_create_list_and_accept_binds_to_the_invites_workspace_and_role() {
    let h = harness().await;
    let owner = h.new_user().await;
    let ws = h.enterprise_ws_owned_by(&owner).await;
    let owner_token = h.token(&owner, ws, Role::Owner, Tier::Enterprise);

    // Owner mints an invite granting Admin.
    let email = format!("{}@example.test", unique("invitee"));
    let (status, body) = send(
        &h.app,
        post(
            "/org/invites",
            &owner_token,
            &json!({ "email": email, "role": "admin" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let token = body["token"].as_str().expect("plaintext token").to_string();
    assert!(!token.is_empty());
    assert_eq!(body["role"], "admin");

    // The pending list shows exactly this invite, and never the token/hash.
    let (ls, list) = send(&h.app, get("/org/invites", &owner_token)).await;
    assert_eq!(ls, StatusCode::OK);
    let invites = list["invites"].as_array().expect("invites array");
    assert_eq!(invites.len(), 1);
    assert_eq!(invites[0]["email"], email);
    assert_eq!(invites[0]["role"], "admin");
    assert!(invites[0].get("token").is_none() && invites[0].get("token_hash").is_none());

    // A DIFFERENT user accepts. Their access token is scoped to their OWN
    // personal workspace - acceptance must bind to the INVITE's workspace, not
    // theirs, and to the INVITE's role (admin), not anything they assert. Extra
    // body fields naming another workspace/role are inert.
    let joiner = h.new_user().await;
    let joiner_personal = h.personal_ws(&joiner).await;
    let joiner_token = h.token(&joiner, joiner_personal, Role::Owner, Tier::Community);
    let (acc, joined) = send(
        &h.app,
        post(
            "/org/invites/accept",
            &joiner_token,
            &json!({
                "token": token,
                // Tampering attempts - must be ignored (server binds to invite):
                "workspace_id": joiner_personal.to_string(),
                "role": "owner",
            }),
        ),
    )
    .await;
    assert_eq!(acc, StatusCode::OK);
    assert_eq!(
        joined["workspace_id"],
        ws.to_string(),
        "bound to the invite's ws"
    );
    assert_eq!(joined["role"], "admin", "bound to the invite's role");

    // Persisted: the joiner is an Admin of the invite's workspace, and their own
    // personal workspace was NOT touched.
    assert_eq!(
        h.members.role_of(ws, &joiner).await.expect("role_of"),
        Some(Role::Admin)
    );
    assert_eq!(
        h.members
            .role_of(joiner_personal, &joiner)
            .await
            .expect("role_of"),
        Some(Role::Owner),
        "the joiner's personal workspace is unchanged"
    );

    // The invite is consumed: the list is empty and a replay is refused.
    let (_, list2) = send(&h.app, get("/org/invites", &owner_token)).await;
    assert!(list2["invites"].as_array().unwrap().is_empty());
    let (replay, _) = send(
        &h.app,
        post(
            "/org/invites/accept",
            &joiner_token,
            &json!({ "token": token }),
        ),
    )
    .await;
    assert_eq!(replay, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// RBAC + tier gates on invite creation.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn invite_create_denied_below_enterprise_and_for_non_managers() {
    let h = harness().await;
    let owner = h.new_user().await;
    let ws = h.enterprise_ws_owned_by(&owner).await;

    // A Pro-tier token for the same workspace fails the Enterprise tier gate.
    let pro_token = h.token(&owner, ws, Role::Owner, Tier::Pro);
    let (pro_status, _) = send(
        &h.app,
        post(
            "/org/invites",
            &pro_token,
            &json!({ "email": "x@example.test", "role": "member" }),
        ),
    )
    .await;
    assert_eq!(pro_status, StatusCode::FORBIDDEN);

    // A Member of the workspace (Enterprise) may not invite: RBAC 403, nothing
    // stored.
    let member = h.new_user().await;
    h.members
        .add(ws, &member, Role::Member, Some(&owner))
        .await
        .expect("seed member");
    let member_token = h.token(&member, ws, Role::Member, Tier::Enterprise);
    let (mem_status, _) = send(
        &h.app,
        post(
            "/org/invites",
            &member_token,
            &json!({ "email": "y@example.test", "role": "member" }),
        ),
    )
    .await;
    assert_eq!(mem_status, StatusCode::FORBIDDEN);

    // No invite was created by either denied attempt.
    let owner_token = h.token(&owner, ws, Role::Owner, Tier::Enterprise);
    let (_, list) = send(&h.app, get("/org/invites", &owner_token)).await;
    assert!(list["invites"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// DELETE /org/invites/{id}: Owner/Admin revoke a pending invite; the token then
// no longer redeems; a non-manager is refused; already-accepted/unknown -> 404.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn revoke_invite_by_owner_and_admin_then_token_no_longer_redeems() {
    let h = harness().await;
    let owner = h.new_user().await;
    let ws = h.enterprise_ws_owned_by(&owner).await;
    let owner_token = h.token(&owner, ws, Role::Owner, Tier::Enterprise);

    // Owner mints an invite, then revokes it: 204, and the pending list empties.
    let (c, created) = send(
        &h.app,
        post(
            "/org/invites",
            &owner_token,
            &json!({ "email": "gone@example.test", "role": "member" }),
        ),
    )
    .await;
    assert_eq!(c, StatusCode::CREATED);
    let invite_id = created["invite_id"]
        .as_str()
        .expect("invite_id")
        .to_string();
    let token = created["token"].as_str().expect("token").to_string();

    let (rev, _) = send(
        &h.app,
        delete(&format!("/org/invites/{invite_id}"), &owner_token),
    )
    .await;
    assert_eq!(rev, StatusCode::NO_CONTENT);

    let (_, list) = send(&h.app, get("/org/invites", &owner_token)).await;
    assert!(list["invites"].as_array().unwrap().is_empty());

    // The revoked token no longer redeems (invalid/expired/used -> 400).
    let joiner = h.new_user().await;
    let joiner_personal = h.personal_ws(&joiner).await;
    let joiner_token = h.token(&joiner, joiner_personal, Role::Owner, Tier::Community);
    let (acc, _) = send(
        &h.app,
        post(
            "/org/invites/accept",
            &joiner_token,
            &json!({ "token": token }),
        ),
    )
    .await;
    assert_eq!(acc, StatusCode::BAD_REQUEST);
    assert_eq!(
        h.members.role_of(ws, &joiner).await.expect("role_of"),
        None,
        "a revoked invite enrolls nobody"
    );

    // Re-revoking (now unknown) -> 404; an Admin can also revoke their own invite.
    let (again, _) = send(
        &h.app,
        delete(&format!("/org/invites/{invite_id}"), &owner_token),
    )
    .await;
    assert_eq!(again, StatusCode::NOT_FOUND);

    let admin = h.new_user().await;
    h.members
        .add(ws, &admin, Role::Admin, Some(&owner))
        .await
        .expect("seed admin");
    let admin_token = h.token(&admin, ws, Role::Admin, Tier::Enterprise);
    let (ac, admin_created) = send(
        &h.app,
        post(
            "/org/invites",
            &admin_token,
            &json!({ "email": "adm@example.test", "role": "member" }),
        ),
    )
    .await;
    assert_eq!(ac, StatusCode::CREATED);
    let admin_invite = admin_created["invite_id"].as_str().expect("invite_id");
    let (ar, _) = send(
        &h.app,
        delete(&format!("/org/invites/{admin_invite}"), &admin_token),
    )
    .await;
    assert_eq!(ar, StatusCode::NO_CONTENT);
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn revoke_invite_denied_for_non_managers_and_scoped_to_own_workspace() {
    let h = harness().await;
    let owner_a = h.new_user().await;
    let owner_b = h.new_user().await;
    let ws_a = h.enterprise_ws_owned_by(&owner_a).await;
    let ws_b = h.enterprise_ws_owned_by(&owner_b).await;
    let token_a = h.token(&owner_a, ws_a, Role::Owner, Tier::Enterprise);
    let token_b = h.token(&owner_b, ws_b, Role::Owner, Tier::Enterprise);

    // B mints an invite.
    let (c, created_b) = send(
        &h.app,
        post(
            "/org/invites",
            &token_b,
            &json!({ "email": "b@example.test", "role": "member" }),
        ),
    )
    .await;
    assert_eq!(c, StatusCode::CREATED);
    let invite_b = created_b["invite_id"]
        .as_str()
        .expect("invite_id")
        .to_string();

    // A's Owner (a different workspace) cannot revoke B's invite: 404 (indistinct
    // from unknown), and B's invite survives.
    let (cross, _) = send(
        &h.app,
        delete(&format!("/org/invites/{invite_b}"), &token_a),
    )
    .await;
    assert_eq!(cross, StatusCode::NOT_FOUND);
    let (_, b_list) = send(&h.app, get("/org/invites", &token_b)).await;
    assert_eq!(
        b_list["invites"].as_array().unwrap().len(),
        1,
        "B's invite survives"
    );

    // A Member of B may not revoke: 403.
    let member = h.new_user().await;
    h.members
        .add(ws_b, &member, Role::Member, Some(&owner_b))
        .await
        .expect("seed member");
    let member_token = h.token(&member, ws_b, Role::Member, Tier::Enterprise);
    let (forbidden, _) = send(
        &h.app,
        delete(&format!("/org/invites/{invite_b}"), &member_token),
    )
    .await;
    assert_eq!(forbidden, StatusCode::FORBIDDEN);
    // Still pending after both refused attempts.
    let (_, b_list2) = send(&h.app, get("/org/invites", &token_b)).await;
    assert_eq!(b_list2["invites"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Claim-bound cross-tenant isolation: a token for A can never list B's invites,
// and no header can redirect it.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn invites_listing_is_claim_bound_and_header_cannot_redirect_it() {
    let h = harness().await;
    let owner_a = h.new_user().await;
    let owner_b = h.new_user().await;
    let ws_a = h.enterprise_ws_owned_by(&owner_a).await;
    let ws_b = h.enterprise_ws_owned_by(&owner_b).await;

    // B has a pending invite; A has none.
    let token_b = h.token(&owner_b, ws_b, Role::Owner, Tier::Enterprise);
    let email_b = format!("{}@example.test", unique("b"));
    let (c, _) = send(
        &h.app,
        post(
            "/org/invites",
            &token_b,
            &json!({ "email": email_b, "role": "member" }),
        ),
    )
    .await;
    assert_eq!(c, StatusCode::CREATED);

    // A's owner lists invites while attaching a header naming B. A sees an EMPTY
    // list (its own workspace) - the header is inert, and B's invite never leaks.
    let token_a = h.token(&owner_a, ws_a, Role::Owner, Tier::Enterprise);
    let req = Request::builder()
        .method("GET")
        .uri("/org/invites")
        .header(header::AUTHORIZATION, format!("Bearer {token_a}"))
        .header("X-Amicu-Workspace-Id", ws_b.to_string())
        .header("X-Workspace-Id", ws_b.to_string())
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["invites"].as_array().unwrap().is_empty(),
        "A's claim-bound listing is empty; B's invite must never leak"
    );
}
