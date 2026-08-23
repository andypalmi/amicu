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
    AuthApiState, BillingApiState, EngineApiState, InMemoryOrgStore, OrgApiState, RateLimiter,
    api_router,
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
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

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

    Harness {
        app: api_router(auth_state, engine_state, billing_state, org_state),
        store,
        tokens,
    }
}

use amicu_api::org::{OrgStore, Role};

/// Seed a member directly into the store (bypassing RBAC), for arranging a test.
async fn seed(store: &InMemoryOrgStore, user_id: &str, role: Role) {
    assert!(store.add_member(user_id, role).await, "seed {user_id}");
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
    assert_eq!(h.store.role_of(MEMBER).await, Some(Role::Member));

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
    assert_eq!(h.store.role_of(MEMBER).await, Some(Role::Admin));
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
    assert_eq!(h.store.role_of(MEMBER).await, None);
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
    assert_eq!(h.store.role_of(VIEWER).await, Some(Role::Viewer));
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
    assert_eq!(h.store.role_of(MEMBER).await, None);

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
    assert_eq!(h.store.role_of(OWNER).await, Some(Role::Owner));
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
    assert_eq!(h.store.role_of(OWNER).await, Some(Role::Owner));
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
    assert_eq!(h.store.role_of(OWNER).await, Some(Role::Owner));
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
    assert_eq!(h.store.role_of("usr_owner2").await, Some(Role::Member));
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
