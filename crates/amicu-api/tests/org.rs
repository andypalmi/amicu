//! Integration tests for the Enterprise organization console (`/org/*`):
//! membership, RBAC, and the audit log.
//!
//! These exercise the full contract over the assembled `api_router`: the auth
//! gate (401), the Enterprise tier gate (403), the first-Owner bootstrap, the
//! happy-path membership CRUD, the per-role RBAC denials (a second, distinct
//! 403), the last-Owner invariant (409), duplicate/unknown handling, and that
//! every mutation lands a structured audit record.

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

use std::sync::Arc;
use std::time::Duration;

use amicu_api::{
    AuthApiState, BillingApiState, ConfigApiState, EngineApiState, InMemoryOrgStore, OrgApiState,
    RateLimiter, api_router,
};
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verifier::StaticJwksProvider;
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_auth::{
    AuthService, IssuerConfig, Keyring, SigningKey, Tier, TokenIssuer, Verifier, VerifierConfig,
};
use amicu_store::WorkspaceId;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The single-tenant workspace every test in this file operates in unless it
/// explicitly exercises multi-tenant scoping (in which case it sends the
/// `X-Amicu-Workspace-Id` header and constructs its own distinct ids).
const ORG: WorkspaceId = WorkspaceId::DEFAULT;

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";

const OWNER: &str = "usr_owner";
const ADMIN: &str = "usr_admin";
const MEMBER: &str = "usr_member";
const VIEWER: &str = "usr_viewer";
const OUTSIDER: &str = "usr_outsider";

/// Tokens for the fixed test cast, all signed by the harness's one key. The
/// `*_tok` fields are Enterprise tokens (they pass the tier gate); `pro_tok` is a
/// Pro token used to prove the tier gate turns a non-Enterprise caller away.
struct Tokens {
    owner: String,
    admin: String,
    member: String,
    viewer: String,
    outsider: String,
    pro: String,
}

/// The assembled app plus the shared org store (so a test can seed members and
/// assert persisted state) and the token set.
struct Harness {
    app: Router,
    store: Arc<InMemoryOrgStore>,
    tokens: Tokens,
}

/// Build the app with a fresh, empty org store. Tests seed the store through
/// `store` before issuing requests; an empty store triggers the first-Owner
/// bootstrap on first access.
fn harness() -> Harness {
    let key = SigningKey::generate().expect("generate key");
    let keyring = Keyring::new(key);
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));
    let jwks = keyring.jwks();

    // Mint every token before the keyring moves into the auth service.
    let mint = |sub: &str, tier: Tier| {
        issuer
            .issue_access(keyring.active(), sub, tier, Vec::new())
            .expect("mint token")
    };
    let tokens = Tokens {
        owner: mint(OWNER, Tier::Enterprise),
        admin: mint(ADMIN, Tier::Enterprise),
        member: mint(MEMBER, Tier::Enterprise),
        viewer: mint(VIEWER, Tier::Enterprise),
        outsider: mint(OUTSIDER, Tier::Enterprise),
        pro: mint("usr_pro", Tier::Pro),
    };

    let service = Arc::new(AuthService::new(
        keyring,
        issuer,
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        ISS,
    ));
    let auth_state = AuthApiState::new(
        service,
        Arc::new(RateLimiter::new(1000, Duration::from_secs(60))),
    );

    let verifier = Arc::new(Verifier::new(
        StaticJwksProvider::new(jwks),
        VerifierConfig::new(ISS, AUD),
    ));
    let engine_state = EngineApiState::new(verifier.clone());
    let billing_state =
        BillingApiState::unconfigured(verifier.clone(), Arc::new(InMemoryUserStore::default()));

    let store = Arc::new(InMemoryOrgStore::new());
    let org_state = OrgApiState::new(verifier, store.clone());
    let config_state = ConfigApiState::new(Arc::new(amicu_store::InMemoryConfigStore::new()));

    Harness {
        app: api_router(
            auth_state,
            engine_state,
            billing_state,
            org_state,
            config_state,
        ),
        store,
        tokens,
    }
}

use amicu_api::org::{OrgStore, Role};

/// Seed a member directly into the default-workspace store (bypassing RBAC),
/// for arranging a test.
async fn seed(store: &InMemoryOrgStore, user_id: &str, role: Role) {
    assert!(store.add_member(ORG, user_id, role).await, "seed {user_id}");
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.expect("request");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, json)
}

fn get(path: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(path);
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

fn body_req(method: &str, path: &str, token: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(method)
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

/// A `GET` request naming a specific organization via the workspace header.
fn get_ws(path: &str, token: &str, ws: WorkspaceId) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("x-amicu-workspace-id", ws.to_string())
        .body(Body::empty())
        .unwrap()
}

/// A request with a raw (possibly malformed) workspace header value.
fn get_ws_raw(path: &str, token: &str, raw: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header("x-amicu-workspace-id", raw)
        .body(Body::empty())
        .unwrap()
}

// ---------------------------------------------------------------------------
// Auth + tier gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn members_unauthenticated_is_401() {
    let h = harness();
    let (status, _) = send(&h.app, get("/org/members", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn members_below_enterprise_is_403_tier() {
    let h = harness();
    let (status, body) = send(&h.app, get("/org/members", Some(&h.tokens.pro))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // The tier 403 is distinct from an RBAC 403: it names the Enterprise tier.
    assert!(body["error"].as_str().unwrap().contains("Enterprise"));
}

// ---------------------------------------------------------------------------
// First-Owner bootstrap + happy-path list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_enterprise_caller_bootstraps_as_owner() {
    let h = harness(); // empty store
    let (status, body) = send(&h.app, get("/org/members", Some(&h.tokens.owner))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["caller_role"], "owner");
    let members = body["members"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["user_id"], OWNER);
    assert_eq!(members[0]["role"], "owner");
    // The bootstrap was audited.
    let (_, audit) = send(&h.app, get("/org/audit", Some(&h.tokens.owner))).await;
    let entries = audit["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["action"] == "org.bootstrapped"));
}

#[tokio::test]
async fn enterprise_non_member_is_403_rbac() {
    let h = harness();
    // Org already has an Owner, so the outsider is NOT bootstrapped and is not a
    // member: an RBAC 403 (distinct message from the tier 403).
    seed(&h.store, OWNER, Role::Owner).await;
    let (status, body) = send(&h.app, get("/org/members", Some(&h.tokens.outsider))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("not a member"));
}

// ---------------------------------------------------------------------------
// Membership CRUD (happy paths) + audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn owner_adds_member_then_lists_and_audits() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;

    let (status, body) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.owner,
            &json!({ "user_id": MEMBER, "role": "member" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["user_id"], MEMBER);
    assert_eq!(body["role"], "member");

    // The list now has both, and the store agrees.
    let (_, list) = send(&h.app, get("/org/members", Some(&h.tokens.owner))).await;
    assert_eq!(list["members"].as_array().unwrap().len(), 2);
    assert_eq!(h.store.role_of(ORG, MEMBER).await, Some(Role::Member));

    // The add was audited (most-recent first).
    let (_, audit) = send(&h.app, get("/org/audit", Some(&h.tokens.owner))).await;
    let first = &audit["entries"][0];
    assert_eq!(first["action"], "member.added");
    assert_eq!(first["actor_id"], OWNER);
    assert_eq!(first["target_id"], MEMBER);
}

#[tokio::test]
async fn owner_changes_role() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, MEMBER, Role::Member).await;

    let (status, body) = send(
        &h.app,
        body_req(
            "PATCH",
            &format!("/org/members/{MEMBER}"),
            &h.tokens.owner,
            &json!({ "role": "admin" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["role"], "admin");
    assert_eq!(h.store.role_of(ORG, MEMBER).await, Some(Role::Admin));
}

#[tokio::test]
async fn owner_removes_member() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, MEMBER, Role::Member).await;

    let (status, body) = send(
        &h.app,
        delete(&format!("/org/members/{MEMBER}"), &h.tokens.owner),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user_id"], MEMBER);
    assert_eq!(h.store.role_of(ORG, MEMBER).await, None);
}

// ---------------------------------------------------------------------------
// RBAC denials (a second, distinct 403)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn member_cannot_remove_anyone_403() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, MEMBER, Role::Member).await;
    seed(&h.store, VIEWER, Role::Viewer).await;

    // A Member has no membership-management permission.
    let (status, body) = send(
        &h.app,
        delete(&format!("/org/members/{VIEWER}"), &h.tokens.member),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("may not manage"));
    // Nothing was removed.
    assert_eq!(h.store.role_of(ORG, VIEWER).await, Some(Role::Viewer));
}

#[tokio::test]
async fn admin_cannot_grant_privileged_role_403() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, ADMIN, Role::Admin).await;

    // An Admin may manage Members/Viewers but may not create an Admin/Owner.
    let (status, body) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.admin,
            &json!({ "user_id": MEMBER, "role": "admin" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("only an Owner"));
    assert_eq!(h.store.role_of(ORG, MEMBER).await, None);

    // But adding a plain Member is allowed.
    let (ok, _) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.admin,
            &json!({ "user_id": MEMBER, "role": "member" }),
        ),
    )
    .await;
    assert_eq!(ok, StatusCode::CREATED);
}

#[tokio::test]
async fn admin_cannot_remove_an_owner_403() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    // A second owner so this is not blocked by the last-Owner invariant - it must
    // be blocked by RBAC (Admin may not touch a privileged member).
    seed(&h.store, "usr_owner2", Role::Owner).await;
    seed(&h.store, ADMIN, Role::Admin).await;

    let (status, _) = send(
        &h.app,
        delete(&format!("/org/members/{OWNER}"), &h.tokens.admin),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(h.store.role_of(ORG, OWNER).await, Some(Role::Owner));
}

#[tokio::test]
async fn member_cannot_view_audit_403() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, MEMBER, Role::Member).await;

    let (status, body) = send(&h.app, get("/org/audit", Some(&h.tokens.member))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("audit log"));
}

#[tokio::test]
async fn viewer_cannot_mutate_membership_403() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, VIEWER, Role::Viewer).await;
    seed(&h.store, MEMBER, Role::Member).await;

    // A Viewer has read-only access: every mutating action is a 403, and none
    // of them change the store.
    let (add_status, _) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.viewer,
            &json!({ "user_id": "usr_new", "role": "member" }),
        ),
    )
    .await;
    assert_eq!(add_status, StatusCode::FORBIDDEN);
    assert_eq!(h.store.role_of(ORG, "usr_new").await, None);

    let (change_status, _) = send(
        &h.app,
        body_req(
            "PATCH",
            &format!("/org/members/{MEMBER}"),
            &h.tokens.viewer,
            &json!({ "role": "admin" }),
        ),
    )
    .await;
    assert_eq!(change_status, StatusCode::FORBIDDEN);
    assert_eq!(h.store.role_of(ORG, MEMBER).await, Some(Role::Member));

    let (remove_status, _) = send(
        &h.app,
        delete(&format!("/org/members/{MEMBER}"), &h.tokens.viewer),
    )
    .await;
    assert_eq!(remove_status, StatusCode::FORBIDDEN);
    assert_eq!(h.store.role_of(ORG, MEMBER).await, Some(Role::Member));
}

#[tokio::test]
async fn viewer_cannot_view_audit_403() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, VIEWER, Role::Viewer).await;

    let (status, body) = send(&h.app, get("/org/audit", Some(&h.tokens.viewer))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("audit log"));
}

#[tokio::test]
async fn admin_cannot_self_escalate_to_owner_403() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, ADMIN, Role::Admin).await;

    // An Admin granting themselves Owner is exactly the "grant a privileged
    // role" case an Admin may never perform, regardless of the target being
    // themselves.
    let (status, body) = send(
        &h.app,
        body_req(
            "PATCH",
            &format!("/org/members/{ADMIN}"),
            &h.tokens.admin,
            &json!({ "role": "owner" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("only an Owner"));
    assert_eq!(h.store.role_of(ORG, ADMIN).await, Some(Role::Admin));
}

// ---------------------------------------------------------------------------
// Invariants + edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn last_owner_cannot_be_removed_409() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;

    let (status, body) = send(
        &h.app,
        delete(&format!("/org/members/{OWNER}"), &h.tokens.owner),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("last remaining Owner")
    );
    assert_eq!(h.store.role_of(ORG, OWNER).await, Some(Role::Owner));
}

#[tokio::test]
async fn last_owner_cannot_be_demoted_409() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;

    let (status, _) = send(
        &h.app,
        body_req(
            "PATCH",
            &format!("/org/members/{OWNER}"),
            &h.tokens.owner,
            &json!({ "role": "admin" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(h.store.role_of(ORG, OWNER).await, Some(Role::Owner));
}

#[tokio::test]
async fn two_owners_allow_demoting_one() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, "usr_owner2", Role::Owner).await;

    let (status, _) = send(
        &h.app,
        body_req(
            "PATCH",
            "/org/members/usr_owner2",
            &h.tokens.owner,
            &json!({ "role": "member" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.store.role_of(ORG, "usr_owner2").await, Some(Role::Member));
}

#[tokio::test]
async fn add_duplicate_member_is_409() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;
    seed(&h.store, MEMBER, Role::Member).await;

    let (status, body) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.owner,
            &json!({ "user_id": MEMBER, "role": "viewer" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("already a member"));
}

#[tokio::test]
async fn unknown_role_is_400() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;

    let (status, body) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.owner,
            &json!({ "user_id": MEMBER, "role": "wizard" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("unknown role"));
}

#[tokio::test]
async fn change_role_unknown_member_is_404() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;

    let (status, _) = send(
        &h.app,
        body_req(
            "PATCH",
            "/org/members/usr_ghost",
            &h.tokens.owner,
            &json!({ "role": "admin" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn remove_unknown_member_is_404() {
    let h = harness();
    seed(&h.store, OWNER, Role::Owner).await;

    let (status, _) = send(&h.app, delete("/org/members/usr_ghost", &h.tokens.owner)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Roles catalog
// ---------------------------------------------------------------------------

#[tokio::test]
async fn roles_catalog_lists_permissions() {
    let h = harness();
    seed(&h.store, VIEWER, Role::Viewer).await;

    // Any member (even a Viewer) may read the RBAC catalog.
    let (status, body) = send(&h.app, get("/org/roles", Some(&h.tokens.viewer))).await;
    assert_eq!(status, StatusCode::OK);
    let roles = body["roles"].as_array().unwrap();
    assert_eq!(roles.len(), 4);
    let owner = roles.iter().find(|r| r["role"] == "owner").unwrap();
    let perms: Vec<&str> = owner["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(perms.contains(&"assign_privileged_role"));
    // A Viewer must not carry management permissions.
    let viewer = roles.iter().find(|r| r["role"] == "viewer").unwrap();
    let vperms: Vec<&str> = viewer["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(!vperms.contains(&"remove_member"));
}

// ---------------------------------------------------------------------------
// Multi-tenant scoping (organization id)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn distinct_workspaces_are_isolated_in_the_store() {
    let store = Arc::new(InMemoryOrgStore::new());
    let ws_a = WorkspaceId::new();
    let ws_b = WorkspaceId::new();

    assert!(store.add_member(ws_a, "usr_a_owner", Role::Owner).await);
    assert!(store.add_member(ws_b, "usr_b_owner", Role::Owner).await);

    store
        .add_member_audited(ws_a, "usr_a_owner", "usr_a_member", Role::Member)
        .await
        .expect("add into workspace a");

    // Workspace B never sees workspace A's members or audit trail.
    assert_eq!(store.member_count(ws_a).await, 2);
    assert_eq!(store.member_count(ws_b).await, 1);
    assert_eq!(store.role_of(ws_b, "usr_a_member").await, None);
    assert!(store.audit(ws_b).await.is_empty());
    assert_eq!(store.audit(ws_a).await.len(), 1);

    // A caller who is a member of A is not automatically a member of B, even
    // with the same user id.
    assert_eq!(store.role_of(ws_b, "usr_a_owner").await, None);
}

#[tokio::test]
async fn non_default_workspace_header_scopes_the_api_and_disables_bootstrap() {
    let h = harness();
    let ws_a = WorkspaceId::new();
    let ws_b = WorkspaceId::new();

    // Seed workspace A only, out-of-band (as documented: a non-default
    // workspace's first member cannot be created via HTTP bootstrap).
    assert!(h.store.add_member(ws_a, OWNER, Role::Owner).await);

    // The Enterprise caller can see workspace A's membership through the
    // header.
    let (status, body) = send(&h.app, get_ws("/org/members", &h.tokens.owner, ws_a)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["members"].as_array().unwrap().len(), 1);

    // The *same* caller, naming empty workspace B, is not silently
    // bootstrapped into Owner there - the read-endpoint 403 is the same "not
    // a member" RBAC denial as any other non-member, not a tier error.
    let (status_b, body_b) = send(&h.app, get_ws("/org/members", &h.tokens.owner, ws_b)).await;
    assert_eq!(status_b, StatusCode::FORBIDDEN);
    assert!(body_b["error"].as_str().unwrap().contains("not a member"));
    assert_eq!(h.store.member_count(ws_b).await, 0);
}

#[tokio::test]
async fn malformed_workspace_header_is_400() {
    let h = harness();
    let (status, body) = send(
        &h.app,
        get_ws_raw("/org/members", &h.tokens.owner, "not-a-uuid"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("workspace"));
}

#[tokio::test]
async fn invalid_token_with_malformed_workspace_header_is_401_not_400() {
    let h = harness();
    // Cryptographic token verification runs before the workspace-header
    // format check (auth-before-request-shape): an invalid/expired token
    // must win with 401 even when the header is also malformed, never a 400
    // that leaks "your header was wrong" ahead of "you aren't authenticated".
    let (status, _) = send(
        &h.app,
        get_ws_raw("/org/members", "not-a-real-token", "not-a-uuid"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Concurrency: atomic mutation+audit, and RBAC checked under the same lock
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_removes_of_same_member_are_atomic_with_audit() {
    let store = Arc::new(InMemoryOrgStore::new());
    seed(&store, OWNER, Role::Owner).await;
    seed(&store, MEMBER, Role::Member).await;

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let store = store.clone();
        tasks.spawn(async move { store.remove_member_audited(ORG, OWNER, MEMBER).await });
    }
    let mut oks = 0usize;
    let mut not_found = 0usize;
    while let Some(res) = tasks.join_next().await {
        match res.expect("task panicked") {
            Ok(role) => {
                assert_eq!(role, Role::Member);
                oks += 1;
            }
            Err(amicu_api::org::OrgStoreError::NotFound) => not_found += 1,
            Err(other) => panic!("unexpected store error: {other:?}"),
        }
    }
    // Cancellation-safety of the atomic mutate-then-audit critical section:
    // exactly one of the 32 concurrent removes can observe the member and
    // remove it; every other one must see it already gone, never a partial
    // state.
    assert_eq!(oks, 1, "exactly one concurrent remove should succeed");
    assert_eq!(not_found, 31);
    assert_eq!(store.role_of(ORG, MEMBER).await, None);

    // No mutation without an audit record, and no audit record without the
    // matching mutation: exactly one, never zero, never doubled.
    let audit = store.audit(ORG).await;
    let removals = audit
        .iter()
        .filter(|e| {
            e.action == amicu_api::org::AuditAction::MemberRemoved
                && e.target_id.as_deref() == Some(MEMBER)
        })
        .count();
    assert_eq!(removals, 1, "exactly one audit record for the one removal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_bootstrap_is_atomic_with_audit() {
    let store = Arc::new(InMemoryOrgStore::new());

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let store = store.clone();
        tasks.spawn(async move { store.bootstrap_owner(ORG, OWNER).await });
    }
    let mut wins = 0usize;
    while let Some(res) = tasks.join_next().await {
        if res.expect("task panicked") {
            wins += 1;
        }
    }
    assert_eq!(wins, 1, "exactly one concurrent bootstrap should win");
    assert_eq!(store.member_count(ORG).await, 1);

    let audit = store.audit(ORG).await;
    let bootstraps = audit
        .iter()
        .filter(|e| e.action == amicu_api::org::AuditAction::OrgBootstrapped)
        .count();
    assert_eq!(
        bootstraps, 1,
        "exactly one audit record for the one bootstrap"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_admin_remove_races_owner_promotion_closes_the_toctou() {
    // Regression for the RBAC TOCTOU: an Admin removing a Member races an
    // Owner promoting that same Member to Admin. Authorizing the remove
    // against the target's role read under the *same* lock as the mutation
    // (rather than an earlier, separately-locked read) means the store's
    // single mutex fully serializes the two operations into one of exactly
    // two admissible outcomes - it can never observe the pre-promotion role
    // to authorize a removal that actually executes post-promotion.
    for _ in 0..200 {
        let store = Arc::new(InMemoryOrgStore::new());
        seed(&store, OWNER, Role::Owner).await;
        seed(&store, ADMIN, Role::Admin).await;
        seed(&store, MEMBER, Role::Member).await;

        let remove_store = store.clone();
        let remove =
            tokio::spawn(
                async move { remove_store.remove_member_audited(ORG, ADMIN, MEMBER).await },
            );
        let promote_store = store.clone();
        let promote = tokio::spawn(async move {
            promote_store
                .set_role_audited(ORG, OWNER, MEMBER, Role::Admin)
                .await
        });

        let (remove_res, promote_res) = tokio::join!(remove, promote);
        let remove_res = remove_res.expect("remove task panicked");
        let promote_res = promote_res.expect("promote task panicked");

        match (remove_res, promote_res) {
            // Remove was serialized first: it correctly removed the
            // still-unprivileged Member, and the promotion correctly finds no
            // such member afterward.
            (Ok(removed_role), Err(amicu_api::org::OrgStoreError::NotFound)) => {
                assert_eq!(removed_role, Role::Member);
            }
            // Promote was serialized first: it landed (Member → Admin), and
            // the remove correctly observed the now-privileged target and
            // refused it - this is the exact case the old two-phase
            // lock+lock code could get wrong.
            (Err(amicu_api::org::OrgStoreError::Forbidden(_)), Ok(prev_role)) => {
                assert_eq!(prev_role, Role::Member);
            }
            other => panic!(
                "non-atomic interleaving: an Admin must never remove a member \
                 it observed as privileged, and the two operations must never \
                 both succeed or both fail: {other:?}"
            ),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_owner_removal_races_actors_own_privileged_mutation_closes_the_actor_toctou() {
    // Regression for the actor-role TOCTOU: an Owner revokes a (compromised)
    // Admin at the same moment that Admin's own already-in-flight privileged
    // mutation (adding a new Member) is landing. Because the store re-reads
    // the *actor's* current role fresh under the same lock as the mutation
    // (rather than trusting a `Role` resolved before the request ever reached
    // the store), the add can only ever succeed by observing the Admin role
    // that was still valid at the instant it was serialized - it can never
    // execute *after* the removal has landed while still carrying the
    // Admin's stale, pre-removal authorization.
    for i in 0..200 {
        let store = Arc::new(InMemoryOrgStore::new());
        seed(&store, OWNER, Role::Owner).await;
        seed(&store, ADMIN, Role::Admin).await;
        let new_member = format!("usr_new_{i}");

        let remove_store = store.clone();
        let remove =
            tokio::spawn(
                async move { remove_store.remove_member_audited(ORG, OWNER, ADMIN).await },
            );
        let add_store = store.clone();
        let new_member_for_add = new_member.clone();
        let add = tokio::spawn(async move {
            add_store
                .add_member_audited(ORG, ADMIN, &new_member_for_add, Role::Member)
                .await
        });

        let (remove_res, add_res) = tokio::join!(remove, add);
        let remove_res = remove_res.expect("remove task panicked");
        let add_res = add_res.expect("add task panicked");

        // The Owner's removal of the Admin is authorized regardless of
        // ordering (the Owner stays a member throughout), so it always
        // succeeds.
        assert_eq!(
            remove_res,
            Ok(Role::Admin),
            "an Owner removing an Admin must always succeed"
        );

        let add_succeeded = add_res.is_ok();
        match &add_res {
            // Serialized before the removal landed: the Admin was still a
            // real member at that instant, so the add is legitimately
            // authorized.
            Ok(role) => assert_eq!(*role, Role::Member),
            // Serialized after the removal landed: the actor is no longer a
            // member at all, so the add must be refused - never silently
            // authorized against the stale, pre-removal Admin role. This is
            // exactly the case the old "pass a pre-resolved `Role` into the
            // store" code could get wrong.
            Err(amicu_api::org::OrgStoreError::Forbidden(msg)) => {
                assert!(msg.contains("not a member"));
            }
            Err(other) => panic!("unexpected add outcome: {other:?}"),
        }

        // Whichever ordering occurred, the new member exists iff the add's
        // own result said so - never orphaned, never phantom.
        let is_member = store.role_of(ORG, &new_member).await.is_some();
        assert_eq!(is_member, add_succeeded);
    }
}
