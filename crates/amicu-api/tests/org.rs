//! Integration tests for the Enterprise organization console (`/org/*`):
//! membership, RBAC, and the audit log, under CLAIM-BOUND tenancy
//! (andypalmi/amicu-planning#130).
//!
//! The workspace a caller acts in, and the coarse role they hold there, come
//! entirely from the verified access token (`claims.wsid` / `claims.wsrole`) -
//! there is no client-supplied workspace request header. Every token in the harness is
//! minted for the shared test workspace `ORG` at the role that matches the actor
//! seeded into the store, so the claim (read authorization) and the store
//! (mutation authorization, re-checked in the store's own critical section)
//! agree - except where a test deliberately makes them disagree to prove the
//! store is authoritative for mutations.
//!
//! These exercise the full contract over the assembled `api_router`: the auth
//! gate (401), the Enterprise tier gate (403), the happy-path membership CRUD,
//! the per-role RBAC denials (a second, distinct 403), the last-Owner invariant
//! (409), duplicate/unknown handling, that every mutation lands a structured
//! audit record, and that a token minted for one workspace can only ever see
//! that workspace.

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use amicu_api::org::{AuditAction, OrgStore, OrgStoreError};
use amicu_api::{
    AuthApiState, BillingApiState, ConfigApiState, EngineApiState, InMemoryOrgStore, OrgApiState,
    RateLimiter, SessionApiState, api_router,
};
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verifier::StaticJwksProvider;
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_auth::{
    AuthService, IssuerConfig, Keyring, Role, SigningKey, Tier, TokenIssuer, Verifier,
    VerifierConfig,
};
use amicu_store::WorkspaceId;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The shared test workspace every token in the harness is scoped to, and where
/// the harness seeds its member cast. Stable within a process run (which is all
/// the harness needs: it mints the tokens and seeds the store with the same id).
static ORG: LazyLock<WorkspaceId> = LazyLock::new(WorkspaceId::new);
/// A DISTINCT workspace, for the cross-tenant isolation tests: a token minted
/// for `OTHER` can only ever see `OTHER`, never `ORG`.
static OTHER: LazyLock<WorkspaceId> = LazyLock::new(WorkspaceId::new);

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";

// User ids are UUIDs in production (the token `sub`, per `new_user_id`), and the
// add-member handler now rejects a non-UUID `user_id` with 400, so the cast uses
// real UUIDs. The in-memory store treats ids as opaque strings, so the values
// are otherwise arbitrary.
const OWNER: &str = "00000000-0000-0000-0000-0000000000a1";
const ADMIN: &str = "00000000-0000-0000-0000-0000000000a2";
const MEMBER: &str = "00000000-0000-0000-0000-0000000000a3";
const VIEWER: &str = "00000000-0000-0000-0000-0000000000a4";
const OTHER_OWNER: &str = "00000000-0000-0000-0000-0000000000a5";
/// A well-formed (UUID) user id that is not seeded anywhere: the target of an
/// add that either succeeds or is refused by RBAC, never by input validation.
const NEW_MEMBER: &str = "00000000-0000-0000-0000-0000000000b1";

/// Tokens for the fixed test cast, all signed by the harness's one key. Each is
/// minted for `ORG` at the actor's role (so the claim matches the seeded store
/// role), except `pro` (a Pro token that must fail the tier gate) and
/// `other_owner` (an Enterprise Owner token scoped to `OTHER`).
struct Tokens {
    owner: String,
    admin: String,
    member: String,
    viewer: String,
    pro: String,
    other_owner: String,
}

/// The assembled app plus the shared org store (so a test can seed members and
/// assert persisted state) and the token set.
struct Harness {
    app: Router,
    store: Arc<InMemoryOrgStore>,
    tokens: Tokens,
}

/// Build the app with a fresh, empty org store. Tests seed the store through
/// `store` before issuing requests.
fn harness() -> Harness {
    let key = SigningKey::generate().expect("generate key");
    let keyring = Keyring::new(key);
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));
    let jwks = keyring.jwks();

    // Mint every token before the keyring moves into the auth service. The
    // workspace + role in each token are what the claim-bound gate trusts.
    let mint = |sub: &str, ws: WorkspaceId, role: Role, tier: Tier| {
        issuer
            .issue_access(keyring.active(), sub, ws, role, tier, Vec::new())
            .expect("mint token")
    };
    let tokens = Tokens {
        owner: mint(OWNER, *ORG, Role::Owner, Tier::Enterprise),
        admin: mint(ADMIN, *ORG, Role::Admin, Tier::Enterprise),
        member: mint(MEMBER, *ORG, Role::Member, Tier::Enterprise),
        viewer: mint(VIEWER, *ORG, Role::Viewer, Tier::Enterprise),
        pro: mint("usr_pro", *ORG, Role::Owner, Tier::Pro),
        other_owner: mint(OTHER_OWNER, *OTHER, Role::Owner, Tier::Enterprise),
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
        service.clone(),
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
    let org_state = OrgApiState::new(verifier.clone(), store.clone());
    let config_state = ConfigApiState::new(Arc::new(amicu_store::InMemoryConfigStore::new()));

    Harness {
        app: api_router(
            auth_state,
            engine_state,
            billing_state,
            org_state,
            config_state,
            SessionApiState::new(service, verifier),
        ),
        store,
        tokens,
    }
}

/// Seed a member directly into a workspace's store (bypassing RBAC), for
/// arranging a test.
fn seed(store: &InMemoryOrgStore, ws: WorkspaceId, user_id: &str, role: Role) {
    assert!(store.add_member_seed(ws, user_id, role), "seed {user_id}");
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
async fn invalid_token_is_401() {
    let h = harness();
    let (status, _) = send(&h.app, get("/org/members", Some("not-a-real-token"))).await;
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
// Happy-path list (the caller's claim role is surfaced as caller_role)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn owner_lists_members_with_own_claim_role() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    let (status, body) = send(&h.app, get("/org/members", Some(&h.tokens.owner))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["caller_role"], "owner");
    let members = body["members"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["user_id"], OWNER);
    assert_eq!(members[0]["role"], "owner");
}

#[tokio::test]
async fn member_lists_members() {
    // A Member holds `view_members`, so `GET /org/members` succeeds - the
    // permission-gated distinction (andypalmi/amicu-planning#133) that a
    // Viewer specifically does NOT get (see `viewer_cannot_list_members_403`).
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, MEMBER, Role::Member);
    let (status, body) = send(&h.app, get("/org/members", Some(&h.tokens.member))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["caller_role"], "member");
    assert_eq!(body["members"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn viewer_cannot_list_members_403() {
    // The enforced Viewer-vs-Member distinction this slice introduces: a
    // Viewer does NOT hold `view_members`, unlike a Member, so `GET
    // /org/members` is refused with a permission 403 (distinct from the tier
    // gate's 403). This assertion would FAIL under the old coarse role match
    // (which treated Member and Viewer identically for read access).
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, VIEWER, Role::Viewer);
    let (status, body) = send(&h.app, get("/org/members", Some(&h.tokens.viewer))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("member roster"));
}

#[tokio::test]
async fn viewer_can_view_roles_catalog() {
    // A Viewer holds `view_roles` (every role does), so `GET /org/roles`
    // succeeds even though `GET /org/members` (above) does not - the same
    // caller, two different outcomes, driven by two different permissions.
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, VIEWER, Role::Viewer);
    let (status, _) = send(&h.app, get("/org/roles", Some(&h.tokens.viewer))).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// The store is authoritative for mutations, even when a claim disagrees
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mutation_is_refused_when_actor_absent_from_store() {
    // The owner token claims role=Owner in ORG, but the store has no such
    // member: the mutation re-reads the actor fresh under its own lock and
    // refuses (the claim never authorizes a mutation on its own).
    let h = harness();
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
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("not a member"));
    assert_eq!(h.store.role_of(*ORG, MEMBER), None);
}

// ---------------------------------------------------------------------------
// Membership CRUD (happy paths) + audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn owner_adds_member_then_lists_and_audits() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);

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
    assert_eq!(h.store.role_of(*ORG, MEMBER), Some(Role::Member));

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
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, MEMBER, Role::Member);

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
    assert_eq!(h.store.role_of(*ORG, MEMBER), Some(Role::Admin));
}

#[tokio::test]
async fn owner_removes_member() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, MEMBER, Role::Member);

    let (status, body) = send(
        &h.app,
        delete(&format!("/org/members/{MEMBER}"), &h.tokens.owner),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user_id"], MEMBER);
    assert_eq!(h.store.role_of(*ORG, MEMBER), None);
}

// ---------------------------------------------------------------------------
// RBAC denials (a second, distinct 403), re-checked against the store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn member_cannot_remove_anyone_403() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, MEMBER, Role::Member);
    seed(&h.store, *ORG, VIEWER, Role::Viewer);

    // A Member has no membership-management permission (re-checked in the store).
    let (status, body) = send(
        &h.app,
        delete(&format!("/org/members/{VIEWER}"), &h.tokens.member),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("may not manage"));
    assert_eq!(h.store.role_of(*ORG, VIEWER), Some(Role::Viewer));
}

#[tokio::test]
async fn admin_cannot_grant_privileged_role_403() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, ADMIN, Role::Admin);

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
    assert_eq!(h.store.role_of(*ORG, MEMBER), None);

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
    seed(&h.store, *ORG, OWNER, Role::Owner);
    // A second owner so this is not blocked by the last-Owner invariant - it must
    // be blocked by RBAC (Admin may not touch a privileged member).
    seed(&h.store, *ORG, "usr_owner2", Role::Owner);
    seed(&h.store, *ORG, ADMIN, Role::Admin);

    let (status, _) = send(
        &h.app,
        delete(&format!("/org/members/{OWNER}"), &h.tokens.admin),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(h.store.role_of(*ORG, OWNER), Some(Role::Owner));
}

#[tokio::test]
async fn member_cannot_view_audit_403() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, MEMBER, Role::Member);

    // The audit gate uses the caller's claim role (Member) directly.
    let (status, body) = send(&h.app, get("/org/audit", Some(&h.tokens.member))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("audit log"));
}

#[tokio::test]
async fn viewer_cannot_mutate_membership_403() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, VIEWER, Role::Viewer);
    seed(&h.store, *ORG, MEMBER, Role::Member);

    // A Viewer has read-only access: every mutating action is a 403, and none
    // of them change the store.
    let (add_status, _) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.viewer,
            &json!({ "user_id": NEW_MEMBER, "role": "member" }),
        ),
    )
    .await;
    assert_eq!(add_status, StatusCode::FORBIDDEN);
    assert_eq!(h.store.role_of(*ORG, NEW_MEMBER), None);

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
    assert_eq!(h.store.role_of(*ORG, MEMBER), Some(Role::Member));

    let (remove_status, _) = send(
        &h.app,
        delete(&format!("/org/members/{MEMBER}"), &h.tokens.viewer),
    )
    .await;
    assert_eq!(remove_status, StatusCode::FORBIDDEN);
    assert_eq!(h.store.role_of(*ORG, MEMBER), Some(Role::Member));
}

#[tokio::test]
async fn viewer_cannot_view_audit_403() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, VIEWER, Role::Viewer);

    let (status, body) = send(&h.app, get("/org/audit", Some(&h.tokens.viewer))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("audit log"));
}

#[tokio::test]
async fn admin_cannot_self_escalate_to_owner_403() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, ADMIN, Role::Admin);

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
    assert_eq!(h.store.role_of(*ORG, ADMIN), Some(Role::Admin));
}

// ---------------------------------------------------------------------------
// Invariants + edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn last_owner_cannot_be_removed_409() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);

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
    assert_eq!(h.store.role_of(*ORG, OWNER), Some(Role::Owner));
}

#[tokio::test]
async fn last_owner_cannot_be_demoted_409() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);

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
    assert_eq!(h.store.role_of(*ORG, OWNER), Some(Role::Owner));
}

#[tokio::test]
async fn two_owners_allow_demoting_one() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, "usr_owner2", Role::Owner);

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
    assert_eq!(h.store.role_of(*ORG, "usr_owner2"), Some(Role::Member));
}

#[tokio::test]
async fn add_duplicate_member_is_409() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, MEMBER, Role::Member);

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
    seed(&h.store, *ORG, OWNER, Role::Owner);

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

/// A malformed (non-UUID) `user_id` is client input, so it is a 400 - it must
/// NOT reach the store and surface as a 500. A well-formed-but-unknown UUID
/// still behaves as before (here: the Owner adds it and it is created).
#[tokio::test]
async fn malformed_user_id_is_400() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);

    let (status, body) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.owner,
            &json!({ "user_id": "not-a-uuid", "role": "member" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("UUID"));

    // A well-formed but unknown user id is accepted (201), unchanged behavior.
    let (ok_status, ok_body) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.owner,
            &json!({ "user_id": NEW_MEMBER, "role": "member" }),
        ),
    )
    .await;
    assert_eq!(ok_status, StatusCode::CREATED);
    assert_eq!(ok_body["user_id"], NEW_MEMBER);
    assert_eq!(h.store.role_of(*ORG, NEW_MEMBER), Some(Role::Member));
}

#[tokio::test]
async fn change_role_unknown_member_is_404() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);

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
    seed(&h.store, *ORG, OWNER, Role::Owner);

    let (status, _) = send(&h.app, delete("/org/members/usr_ghost", &h.tokens.owner)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Roles catalog
// ---------------------------------------------------------------------------

#[tokio::test]
async fn roles_catalog_lists_permissions() {
    let h = harness();

    // Any Enterprise token (even a Viewer's) may read the RBAC catalog.
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
    // A Viewer must not carry management permissions, NOR `view_members` - the
    // enforced distinction from Member (andypalmi/amicu-planning#133).
    let viewer = roles.iter().find(|r| r["role"] == "viewer").unwrap();
    let vperms: Vec<&str> = viewer["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(!vperms.contains(&"remove_member"));
    assert!(!vperms.contains(&"view_members"));
    assert!(vperms.contains(&"view_roles"));
    // A Member DOES carry `view_members` - the catalog reflects the same
    // matrix `GET /org/members` enforces.
    let member = roles.iter().find(|r| r["role"] == "member").unwrap();
    let mperms: Vec<&str> = member["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert!(mperms.contains(&"view_members"));
}

// ---------------------------------------------------------------------------
// Claim-bound cross-tenant isolation (no header: a token names its own ws)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_token_only_ever_sees_its_own_workspace() {
    let h = harness();
    // ORG has two members; OTHER has one, seeded independently.
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, MEMBER, Role::Member);
    seed(&h.store, *OTHER, OTHER_OWNER, Role::Owner);

    // The ORG-scoped owner token sees exactly ORG's two members.
    let (status, body) = send(&h.app, get("/org/members", Some(&h.tokens.owner))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["members"].as_array().unwrap().len(), 2);

    // The OTHER-scoped owner token sees exactly OTHER's one member - never ORG's.
    let (status_b, body_b) = send(&h.app, get("/org/members", Some(&h.tokens.other_owner))).await;
    assert_eq!(status_b, StatusCode::OK);
    let other_members = body_b["members"].as_array().unwrap();
    assert_eq!(other_members.len(), 1);
    assert_eq!(other_members[0]["user_id"], OTHER_OWNER);
}

/// andypalmi/amicu-planning#131 exit criterion: the workspace is claim-derived
/// EVERYWHERE and no request header can influence it. A caller attaches a bogus
/// `X-Amicu-Workspace-Id` (and a stray `X-Workspace-Id`) naming OTHER while
/// holding an ORG-scoped token; the request still acts on ORG (the claim), and
/// OTHER's data is never reached. There is no header path by construction - the
/// gate reads only the verified `Authorization` bearer - so the bogus headers
/// are inert.
#[tokio::test]
async fn a_bogus_workspace_header_cannot_redirect_the_claim() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *ORG, MEMBER, Role::Member);
    // OTHER has a single, distinct member; if a header could redirect scope, the
    // ORG token would "see" OTHER's one member instead of ORG's two.
    seed(&h.store, *OTHER, OTHER_OWNER, Role::Owner);

    let req = Request::builder()
        .method("GET")
        .uri("/org/members")
        .header(header::AUTHORIZATION, format!("Bearer {}", h.tokens.owner))
        .header("X-Amicu-Workspace-Id", OTHER.to_string())
        .header("X-Workspace-Id", OTHER.to_string())
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::OK);
    // Still ORG (two members), never OTHER (one): the header was ignored.
    let members = body["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "claim workspace ORG wins over any header");
    let ids: Vec<&str> = members
        .iter()
        .map(|m| m["user_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&OWNER) && ids.contains(&MEMBER));
    assert!(
        !ids.contains(&OTHER_OWNER),
        "OTHER's member must never appear"
    );
}

/// Companion exit criterion: a mutation cannot be redirected by a header either.
/// The ORG owner adds a member while attaching a header naming OTHER; the member
/// lands in ORG (the claim), and OTHER is untouched.
#[tokio::test]
async fn a_bogus_workspace_header_cannot_redirect_a_mutation() {
    let h = harness();
    seed(&h.store, *ORG, OWNER, Role::Owner);
    seed(&h.store, *OTHER, OTHER_OWNER, Role::Owner);

    let req = Request::builder()
        .method("POST")
        .uri("/org/members")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", h.tokens.owner))
        .header("X-Amicu-Workspace-Id", OTHER.to_string())
        .body(Body::from(
            json!({ "user_id": NEW_MEMBER, "role": "member" }).to_string(),
        ))
        .unwrap();
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::CREATED);
    // Landed in ORG (the claim), not OTHER (the header).
    assert_eq!(h.store.role_of(*ORG, NEW_MEMBER), Some(Role::Member));
    assert_eq!(h.store.role_of(*OTHER, NEW_MEMBER), None);
}

#[tokio::test]
async fn a_mutation_lands_only_in_the_tokens_workspace() {
    let h = harness();
    seed(&h.store, *OTHER, OTHER_OWNER, Role::Owner);

    // The OTHER-scoped owner adds a member; it can only ever land in OTHER.
    let (status, _) = send(
        &h.app,
        body_req(
            "POST",
            "/org/members",
            &h.tokens.other_owner,
            &json!({ "user_id": NEW_MEMBER, "role": "member" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(h.store.role_of(*OTHER, NEW_MEMBER), Some(Role::Member));
    // ORG is untouched (the token could not name it).
    assert_eq!(h.store.role_of(*ORG, NEW_MEMBER), None);
    assert_eq!(h.store.member_count(*ORG), 0);
}

// ---------------------------------------------------------------------------
// Store-level tenant isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn distinct_workspaces_are_isolated_in_the_store() {
    let store = Arc::new(InMemoryOrgStore::new());
    let ws_a = WorkspaceId::new();
    let ws_b = WorkspaceId::new();

    assert!(store.add_member_seed(ws_a, "usr_a_owner", Role::Owner));
    assert!(store.add_member_seed(ws_b, "usr_b_owner", Role::Owner));

    store
        .add_member_audited(ws_a, "usr_a_owner", "usr_a_member", Role::Member)
        .await
        .expect("add into workspace a");

    // Workspace B never sees workspace A's members or audit trail.
    assert_eq!(store.member_count(ws_a), 2);
    assert_eq!(store.member_count(ws_b), 1);
    assert_eq!(store.role_of(ws_b, "usr_a_member"), None);
    assert!(store.audit(ws_b).await.is_empty());
    assert_eq!(store.audit(ws_a).await.len(), 1);

    // A caller who is a member of A is not automatically a member of B, even
    // with the same user id.
    assert_eq!(store.role_of(ws_b, "usr_a_owner"), None);
}

// ---------------------------------------------------------------------------
// Concurrency: atomic mutation+audit, and RBAC checked under the same lock
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_removes_of_same_member_are_atomic_with_audit() {
    let ws = WorkspaceId::new();
    let store = Arc::new(InMemoryOrgStore::new());
    seed(&store, ws, OWNER, Role::Owner);
    seed(&store, ws, MEMBER, Role::Member);

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let store = store.clone();
        tasks.spawn(async move { store.remove_member_audited(ws, OWNER, MEMBER).await });
    }
    let mut oks = 0usize;
    let mut not_found = 0usize;
    while let Some(res) = tasks.join_next().await {
        match res.expect("task panicked") {
            Ok(role) => {
                assert_eq!(role, Role::Member);
                oks += 1;
            }
            Err(OrgStoreError::NotFound) => not_found += 1,
            Err(other) => panic!("unexpected store error: {other:?}"),
        }
    }
    // Cancellation-safety of the atomic mutate-then-audit critical section:
    // exactly one of the 32 concurrent removes can observe the member and
    // remove it; every other one must see it already gone, never a partial
    // state.
    assert_eq!(oks, 1, "exactly one concurrent remove should succeed");
    assert_eq!(not_found, 31);
    assert_eq!(store.role_of(ws, MEMBER), None);

    // No mutation without an audit record, and no audit record without the
    // matching mutation: exactly one, never zero, never doubled.
    let audit = store.audit(ws).await;
    let removals = audit
        .iter()
        .filter(|e| {
            e.action == AuditAction::MemberRemoved && e.target_id.as_deref() == Some(MEMBER)
        })
        .count();
    assert_eq!(removals, 1, "exactly one audit record for the one removal");
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
        let ws = WorkspaceId::new();
        let store = Arc::new(InMemoryOrgStore::new());
        seed(&store, ws, OWNER, Role::Owner);
        seed(&store, ws, ADMIN, Role::Admin);
        seed(&store, ws, MEMBER, Role::Member);

        let remove_store = store.clone();
        let remove =
            tokio::spawn(
                async move { remove_store.remove_member_audited(ws, ADMIN, MEMBER).await },
            );
        let promote_store = store.clone();
        let promote = tokio::spawn(async move {
            promote_store
                .set_role_audited(ws, OWNER, MEMBER, Role::Admin)
                .await
        });

        let (remove_res, promote_res) = tokio::join!(remove, promote);
        let remove_res = remove_res.expect("remove task panicked");
        let promote_res = promote_res.expect("promote task panicked");

        match (remove_res, promote_res) {
            // Remove was serialized first: it correctly removed the
            // still-unprivileged Member, and the promotion correctly finds no
            // such member afterward.
            (Ok(removed_role), Err(OrgStoreError::NotFound)) => {
                assert_eq!(removed_role, Role::Member);
            }
            // Promote was serialized first: it landed (Member -> Admin), and
            // the remove correctly observed the now-privileged target and
            // refused it - this is the exact case the old two-phase
            // lock+lock code could get wrong.
            (Err(OrgStoreError::Forbidden(_)), Ok(prev_role)) => {
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
        let ws = WorkspaceId::new();
        let store = Arc::new(InMemoryOrgStore::new());
        seed(&store, ws, OWNER, Role::Owner);
        seed(&store, ws, ADMIN, Role::Admin);
        let new_member = format!("usr_new_{i}");

        let remove_store = store.clone();
        let remove =
            tokio::spawn(async move { remove_store.remove_member_audited(ws, OWNER, ADMIN).await });
        let add_store = store.clone();
        let new_member_for_add = new_member.clone();
        let add = tokio::spawn(async move {
            add_store
                .add_member_audited(ws, ADMIN, &new_member_for_add, Role::Member)
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
            // authorized against the stale, pre-removal Admin role.
            Err(OrgStoreError::Forbidden(msg)) => {
                assert!(msg.contains("not a member"));
            }
            Err(other) => panic!("unexpected add outcome: {other:?}"),
        }

        // Whichever ordering occurred, the new member exists iff the add's
        // own result said so - never orphaned, never phantom.
        let is_member = store.role_of(ws, &new_member).is_some();
        assert_eq!(is_member, add_succeeded);
    }
}
