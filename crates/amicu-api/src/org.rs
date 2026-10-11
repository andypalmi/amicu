//! HTTP surface for the Enterprise **organization console**: membership,
//! role-based access control (RBAC), and an append-only audit log.
//!
//! # Shape
//! Every endpoint is auth-gated **and** Enterprise-tier-gated (see
//! [`org_auth_gate`]): a caller below Enterprise is turned away with a `403`
//! *before* any org state is read or serialized. Which organization the caller
//! acts in, and the coarse role they hold there, come ENTIRELY from the verified
//! access token (`claims.wsid` / `claims.wsrole`), surfaced as the request's
//! [`WorkspaceContext`] - never from a request header (see "Claim-bound tenancy"
//! below).
//!
//! - [`GET /org/members`](list_members_handler): list members and their roles.
//! - [`POST /org/members`](add_member_handler): invite/add a member with a role.
//! - [`PATCH /org/members/{id}`](change_role_handler): change a member's role.
//! - [`DELETE /org/members/{id}`](remove_member_handler): remove a member.
//! - [`GET /org/roles`](list_roles_handler): the RBAC catalog (roles + perms).
//! - [`GET /org/audit`](audit_handler): the audit log, most-recent first.
//!
//! # Two distinct 403s
//! The tier gate returns `403` when the caller is not on Enterprise. A *second*,
//! independent `403` ([`OrgError::Forbidden`]) is returned by the handlers/store
//! when the caller's **org role** does not permit the action (e.g. a `Member`
//! trying to remove someone). The messages differ so the two are never confused.
//!
//! # Claim-bound tenancy (andypalmi/amicu-planning#130)
//! The workspace a caller operates in is `claims.wsid` - resolved server-side
//! from `workspace_members` when the token was minted, re-validated on every
//! refresh rotation, and never asserted by the client. There is no
//! client-supplied workspace request header any more: holding a valid token for
//! a workspace IS the proof of membership in it, so a token minted for A can only ever
//! read or mutate A. The caller's coarse role for cheap read authorization comes
//! from `claims.wsrole` (the [`WorkspaceContext::role`]); every privileged
//! mutation re-checks the actor's and target's *current* roles in the store's
//! own transaction (see "Atomicity").
//!
//! # Atomicity: mutation + audit, and RBAC, under one transaction/lock
//! Two correctness properties are held by construction rather than by
//! convention, in BOTH store backends:
//! - **A mutation and its audit record are never split.** The `*_audited` store
//!   methods write both the membership change *and* its audit entry before
//!   committing (one lock acquisition in-memory; one transaction on Postgres). A
//!   task cancelled at any point either did nothing or did both.
//! - **RBAC is checked against *both* roles read under the same lock/transaction
//!   as the mutation.** The handlers never pass a pre-resolved caller role into a
//!   mutation. The `*_audited` methods take the actor's *id* and re-read the
//!   actor's *and* the target's current roles inside the same critical section
//!   (in-memory: the store mutex; Postgres: a `begin_scoped` transaction with the
//!   member rows locked `FOR UPDATE`) and authorize against those fresh reads.
//!   This closes the #110/#117 TOCTOUs: a concurrent role change on the target,
//!   or on the actor themselves, mid-flight, is authorized against who they are
//!   *now*, not who they were when the request started.
//!
//! # Two backends
//! [`OrgStore`] has two implementations: the in-memory [`InMemoryOrgStore`]
//! (zero-config dev / tests, state lost on restart) and
//! [`amicu_store::PostgresMembershipStore`] (the durable, multi-workspace
//! authority backed by `workspace_members` + `org_audit`). `amicu-server` picks
//! Postgres when `DATABASE_URL` is set, else the in-memory store.
//!
//! # SSO is out of scope (deferred post-v1)
//! Single sign-on (SAML/OIDC federation, SCIM provisioning) is intentionally not
//! built here - see the `andypalmi/amicu-planning#92` SSO follow-up. Members are
//! keyed by their first-party user id.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_auth::{Permission, Role, Tier};
use amicu_store::WorkspaceId;
use amicu_store::{OrgAuditAction, OrgAuditEntry, OrgMutationError, PostgresMembershipStore};
use async_trait::async_trait;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use crate::engine::{AccessVerifier, WorkspaceContext};

// ---------------------------------------------------------------------------
// RBAC domain: the role catalog (roles + permissions)
//
// The role type is `amicu_auth::Role` (owner/admin/member/viewer) and the
// permission type is `amicu_auth::Permission` - the SAME types the `wsrole`
// claim, the store, and every enforcement call site use, so there is exactly
// one definition of a role, a permission, and the role -> permission matrix
// (`Role::grants`/`Role::permissions`) in the codebase
// (andypalmi/amicu-planning#133). This module surfaces that shared matrix as
// the `GET /org/roles` catalog DTO; it holds no permission logic of its own.
// ---------------------------------------------------------------------------

/// A short human description of `role`, for the RBAC catalog.
fn role_description(role: Role) -> &'static str {
    match role {
        Role::Owner => {
            "Full control, including granting Owner/Admin. At least one Owner must remain."
        }
        Role::Admin => "Manages membership, but cannot grant or modify Owner/Admin members.",
        Role::Member => "Regular member with read-only visibility into membership.",
        Role::Viewer => {
            "Read-only visibility into the role catalog. Cannot view the member roster."
        }
    }
}

/// Every role, privilege-descending - the catalog and store iteration order.
const ALL_ROLES: [Role; 4] = [Role::Owner, Role::Admin, Role::Member, Role::Viewer];

// ---------------------------------------------------------------------------
// Store: members + append-only audit log, partitioned by organization
// ---------------------------------------------------------------------------

/// A member of the organization: a first-party user id and their assigned role.
#[derive(Debug, Clone)]
pub struct Member {
    /// The member's user id (the token `sub`).
    pub user_id: String,
    /// The member's assigned role.
    pub role: Role,
}

/// A structured, privileged-action audit record. Append-only; the store stamps
/// the id and time so the log's ordering and clock are the store's own.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// Monotonic sequence id within this organization's log.
    pub id: u64,
    /// When the action was recorded (Unix seconds).
    pub timestamp: i64,
    /// The user id that performed the action.
    pub actor_id: String,
    /// What happened.
    pub action: AuditAction,
    /// The member the action targeted, if any.
    pub target_id: Option<String>,
    /// A human-readable detail (e.g. `"member -> admin"`).
    pub detail: Option<String>,
}

/// The set of audited privileged actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    /// A member was added.
    MemberAdded,
    /// A member's role was changed.
    RoleChanged,
    /// A member was removed.
    MemberRemoved,
    /// A pending invite was created.
    InviteCreated,
    /// An invite was accepted, enrolling the redeemer as a member.
    InviteAccepted,
    /// A pending invite was revoked before acceptance.
    InviteRevoked,
}

impl AuditAction {
    /// The wire name for this action.
    fn as_str(self) -> &'static str {
        match self {
            AuditAction::MemberAdded => "member.added",
            AuditAction::RoleChanged => "member.role_changed",
            AuditAction::MemberRemoved => "member.removed",
            AuditAction::InviteCreated => "invite.created",
            AuditAction::InviteAccepted => "invite.accepted",
            AuditAction::InviteRevoked => "invite.revoked",
        }
    }
}

impl From<OrgAuditAction> for AuditAction {
    fn from(a: OrgAuditAction) -> Self {
        match a {
            OrgAuditAction::MemberAdded => AuditAction::MemberAdded,
            OrgAuditAction::RoleChanged => AuditAction::RoleChanged,
            OrgAuditAction::MemberRemoved => AuditAction::MemberRemoved,
            OrgAuditAction::InviteCreated => AuditAction::InviteCreated,
            OrgAuditAction::InviteAccepted => AuditAction::InviteAccepted,
            OrgAuditAction::InviteRevoked => AuditAction::InviteRevoked,
        }
    }
}

impl From<OrgAuditEntry> for AuditEntry {
    fn from(e: OrgAuditEntry) -> Self {
        Self {
            id: u64::try_from(e.id).unwrap_or(0),
            timestamp: e.recorded_at,
            actor_id: e.actor_id,
            action: e.action.into(),
            target_id: e.target_id,
            detail: e.detail,
        }
    }
}

/// A new audit record to append (the store assigns its id and timestamp).
#[derive(Debug, Clone)]
struct NewAuditEntry {
    /// The user id that performed the action.
    actor_id: String,
    /// What happened.
    action: AuditAction,
    /// The member the action targeted, if any.
    target_id: Option<String>,
    /// A human-readable detail.
    detail: Option<String>,
}

/// A store invariant a mutation would violate, or an RBAC check performed
/// atomically with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrgStoreError {
    /// No member with that user id exists in this organization.
    NotFound,
    /// The actor's role does not permit this action, checked against the
    /// target's role under the same lock/transaction as the mutation.
    Forbidden(String),
    /// The mutation would leave the organization with no Owner.
    LastOwner,
    /// (add only) the user is already a member.
    AlreadyMember,
    /// A backend/data-integrity fault (Postgres backend only).
    Backend,
}

impl From<OrgMutationError> for OrgStoreError {
    fn from(e: OrgMutationError) -> Self {
        match e {
            OrgMutationError::NotFound => OrgStoreError::NotFound,
            OrgMutationError::Forbidden(m) => OrgStoreError::Forbidden(m),
            OrgMutationError::LastOwner => OrgStoreError::LastOwner,
            OrgMutationError::AlreadyMember => OrgStoreError::AlreadyMember,
            OrgMutationError::Backend => OrgStoreError::Backend,
        }
    }
}

/// Organization persistence seam - the surface the console handlers need,
/// scoped to a [`WorkspaceId`] per call.
///
/// The privileged mutations (`*_audited`) re-check RBAC against a freshly-read
/// actor and target role and write their audit record atomically with the
/// membership change; see the module docs' "Atomicity" section. Both the
/// in-memory [`InMemoryOrgStore`] and the durable
/// [`amicu_store::PostgresMembershipStore`] implement it.
#[async_trait]
pub trait OrgStore: Send + Sync {
    /// All members of `org`, in a stable order.
    async fn members(&self, org: WorkspaceId) -> Vec<Member>;
    /// Add `target_id` with `role`, RBAC-authorized against `actor_id`'s
    /// *current* role (re-read under the mutation's own lock/transaction),
    /// atomically with its `member.added` audit record.
    async fn add_member_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        role: Role,
    ) -> Result<Role, OrgStoreError>;
    /// Change `target_id`'s role to `new_role`, RBAC-authorized against both the
    /// actor's and the target's *current* roles (re-read under the mutation's own
    /// lock/transaction), atomically with its `member.role_changed` audit record.
    /// Returns the previous role.
    async fn set_role_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        new_role: Role,
    ) -> Result<Role, OrgStoreError>;
    /// Remove `target_id`, RBAC-authorized against both the actor's and the
    /// target's *current* roles (re-read under the mutation's own
    /// lock/transaction), atomically with its `member.removed` audit record.
    /// Returns the removed role.
    async fn remove_member_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
    ) -> Result<Role, OrgStoreError>;
    /// The audit log for `org`, most-recent first.
    async fn audit(&self, org: WorkspaceId) -> Vec<AuditEntry>;
}

/// Mutable state behind the in-memory store's mutex, for one organization.
#[derive(Default)]
struct OrgState {
    /// user_id -> role. `BTreeMap` for a deterministic member order.
    members: BTreeMap<String, Role>,
    /// Append-only log (oldest first internally; reversed on read).
    audit: Vec<AuditEntry>,
    /// Next audit id to assign (1-based), scoped to this organization.
    next_id: u64,
}

impl OrgState {
    /// How many Owners are currently in the org.
    fn owner_count(&self) -> usize {
        self.members.values().filter(|r| **r == Role::Owner).count()
    }
}

/// Append `entry` to `org_state`'s log under the caller's already-held lock.
/// Never called on its own - always paired with the membership mutation that
/// motivated it, inside the same critical section (see the module docs'
/// "Atomicity" section).
fn append_audit_locked(org_state: &mut OrgState, entry: NewAuditEntry) -> AuditEntry {
    org_state.next_id += 1;
    let stored = AuditEntry {
        id: org_state.next_id,
        timestamp: now_unix(),
        actor_id: entry.actor_id,
        action: entry.action,
        target_id: entry.target_id,
        detail: entry.detail,
    };
    org_state.audit.push(stored.clone());
    stored
}

/// In-memory [`OrgStore`] for tests and single-node dev. All state is lost on
/// restart. Organizations are partitioned by [`WorkspaceId`] behind a single
/// mutex; the Postgres backend instead scopes each transaction to the workspace.
#[derive(Default)]
pub struct InMemoryOrgStore {
    inner: Mutex<BTreeMap<WorkspaceId, OrgState>>,
}

impl InMemoryOrgStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a member directly, bypassing RBAC and without an audit record. Used
    /// to seed org state in tests (and as the out-of-band primitive that
    /// populates a workspace's first member). Returns `true` if inserted,
    /// `false` if `user_id` was already a member.
    pub fn add_member_seed(&self, org: WorkspaceId, user_id: &str, role: Role) -> bool {
        let mut state = self.inner.lock().expect("org store poisoned");
        let org_state = state.entry(org).or_default();
        if org_state.members.contains_key(user_id) {
            return false;
        }
        org_state.members.insert(user_id.to_owned(), role);
        true
    }

    /// The role of `user_id` within `org`, or `None` if they are not a member.
    #[must_use]
    pub fn role_of(&self, org: WorkspaceId, user_id: &str) -> Option<Role> {
        self.inner
            .lock()
            .expect("org store poisoned")
            .get(&org)
            .and_then(|s| s.members.get(user_id))
            .copied()
    }

    /// The current number of members of `org`.
    #[must_use]
    pub fn member_count(&self, org: WorkspaceId) -> usize {
        self.inner
            .lock()
            .expect("org store poisoned")
            .get(&org)
            .map_or(0, |s| s.members.len())
    }
}

#[async_trait]
impl OrgStore for InMemoryOrgStore {
    async fn members(&self, org: WorkspaceId) -> Vec<Member> {
        let state = self.inner.lock().expect("org store poisoned");
        state.get(&org).map_or_else(Vec::new, |s| {
            s.members
                .iter()
                .map(|(id, role)| Member {
                    user_id: id.clone(),
                    role: *role,
                })
                .collect()
        })
    }

    async fn add_member_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        role: Role,
    ) -> Result<Role, OrgStoreError> {
        let mut state = self.inner.lock().expect("org store poisoned");
        let org_state = state.entry(org).or_default();
        let actor = org_state.members.get(actor_id).copied();
        authorize_membership_mutation(actor, Permission::InviteMember, None, Some(role))?;
        if org_state.members.contains_key(target_id) {
            return Err(OrgStoreError::AlreadyMember);
        }
        org_state.members.insert(target_id.to_owned(), role);
        append_audit_locked(
            org_state,
            NewAuditEntry {
                actor_id: actor_id.to_owned(),
                action: AuditAction::MemberAdded,
                target_id: Some(target_id.to_owned()),
                detail: Some(format!("added as {}", role.as_str())),
            },
        );
        Ok(role)
    }

    async fn set_role_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        new_role: Role,
    ) -> Result<Role, OrgStoreError> {
        let mut state = self.inner.lock().expect("org store poisoned");
        let org_state = state.entry(org).or_default();
        let Some(current) = org_state.members.get(target_id).copied() else {
            return Err(OrgStoreError::NotFound);
        };
        let actor = org_state.members.get(actor_id).copied();
        authorize_membership_mutation(
            actor,
            Permission::ChangeRole,
            Some(current),
            Some(new_role),
        )?;
        // Refuse to demote the last remaining Owner.
        if current == Role::Owner && new_role != Role::Owner && org_state.owner_count() == 1 {
            return Err(OrgStoreError::LastOwner);
        }
        org_state.members.insert(target_id.to_owned(), new_role);
        append_audit_locked(
            org_state,
            NewAuditEntry {
                actor_id: actor_id.to_owned(),
                action: AuditAction::RoleChanged,
                target_id: Some(target_id.to_owned()),
                detail: Some(format!("{} -> {}", current.as_str(), new_role.as_str())),
            },
        );
        Ok(current)
    }

    async fn remove_member_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
    ) -> Result<Role, OrgStoreError> {
        let mut state = self.inner.lock().expect("org store poisoned");
        let org_state = state.entry(org).or_default();
        let Some(current) = org_state.members.get(target_id).copied() else {
            return Err(OrgStoreError::NotFound);
        };
        let actor = org_state.members.get(actor_id).copied();
        authorize_membership_mutation(actor, Permission::RemoveMember, Some(current), None)?;
        // Refuse to remove the last remaining Owner.
        if current == Role::Owner && org_state.owner_count() == 1 {
            return Err(OrgStoreError::LastOwner);
        }
        org_state.members.remove(target_id);
        append_audit_locked(
            org_state,
            NewAuditEntry {
                actor_id: actor_id.to_owned(),
                action: AuditAction::MemberRemoved,
                target_id: Some(target_id.to_owned()),
                detail: Some(format!("removed ({})", current.as_str())),
            },
        );
        Ok(current)
    }

    async fn audit(&self, org: WorkspaceId) -> Vec<AuditEntry> {
        let state = self.inner.lock().expect("org store poisoned");
        let mut entries = state.get(&org).map_or_else(Vec::new, |s| s.audit.clone());
        entries.reverse(); // most-recent first
        entries
    }
}

/// The durable org-console backend: `workspace_members` for membership +
/// `org_audit` for the log, with every privileged mutation re-checking RBAC and
/// writing its audit record inside one workspace-scoped transaction (member rows
/// locked `FOR UPDATE`), carrying the #117 TOCTOU/atomicity guarantees onto
/// Postgres.
#[async_trait]
impl OrgStore for PostgresMembershipStore {
    async fn members(&self, org: WorkspaceId) -> Vec<Member> {
        self.list_members(org).await.map_or_else(
            |_| Vec::new(),
            |members| {
                members
                    .into_iter()
                    .map(|m| Member {
                        user_id: m.user_id,
                        role: m.role,
                    })
                    .collect()
            },
        )
    }

    async fn add_member_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        role: Role,
    ) -> Result<Role, OrgStoreError> {
        PostgresMembershipStore::add_member_audited(self, org, actor_id, target_id, role)
            .await
            .map_err(OrgStoreError::from)
    }

    async fn set_role_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        new_role: Role,
    ) -> Result<Role, OrgStoreError> {
        PostgresMembershipStore::set_role_audited(self, org, actor_id, target_id, new_role)
            .await
            .map_err(OrgStoreError::from)
    }

    async fn remove_member_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
    ) -> Result<Role, OrgStoreError> {
        PostgresMembershipStore::remove_member_audited(self, org, actor_id, target_id)
            .await
            .map_err(OrgStoreError::from)
    }

    async fn audit(&self, org: WorkspaceId) -> Vec<AuditEntry> {
        self.org_audit(org).await.map_or_else(
            |_| Vec::new(),
            |es| es.into_iter().map(AuditEntry::from).collect(),
        )
    }
}

/// Current wall-clock time in seconds since the Unix epoch.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------
// State + router + tier gate
// ---------------------------------------------------------------------------

/// The minimum tier required to reach the org console.
const ORG_MIN_TIER: Tier = Tier::Enterprise;

/// Shared state for the org endpoints: the access-token verifier that backs the
/// Enterprise gate, and the org store.
#[derive(Clone)]
pub struct OrgApiState {
    verifier: Arc<dyn AccessVerifier>,
    store: Arc<dyn OrgStore>,
    /// The durable provisioning/invite authority backing `POST /org/invites` and
    /// `GET /org/invites`. `None` on the in-memory dev path (those routes then
    /// return `503`); see [`crate::tenancy`].
    tenancy: Option<Arc<dyn crate::tenancy::TenancyStore>>,
}

impl OrgApiState {
    /// Assemble org state from a verifier and a store.
    #[must_use]
    pub fn new(verifier: Arc<dyn AccessVerifier>, store: Arc<dyn OrgStore>) -> Self {
        Self {
            verifier,
            store,
            tenancy: None,
        }
    }

    /// Install the durable tenancy store, enabling `POST /org/invites` and
    /// `GET /org/invites` (the Postgres deployment path).
    #[must_use]
    pub fn with_tenancy(mut self, tenancy: Arc<dyn crate::tenancy::TenancyStore>) -> Self {
        self.tenancy = Some(tenancy);
        self
    }

    /// The durable tenancy store, if wired.
    pub(crate) fn tenancy(&self) -> Option<&Arc<dyn crate::tenancy::TenancyStore>> {
        self.tenancy.as_ref()
    }
}

/// Ordinal ranking of tiers for the Enterprise gate. Kept local (the engine's is
/// private) so this vertical is self-contained.
fn tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::Community => 0,
        Tier::Pro => 1,
        Tier::Enterprise => 2,
        Tier::Hosted => 3,
    }
}

/// Extract a `Bearer` token from the `Authorization` header.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Auth + Enterprise-tier gate for the org endpoints.
///
/// Rejects requests without a valid token (`401`) and authenticated requests
/// whose ACTIVE-WORKSPACE tier (`claims.tier`) is below Enterprise (`403`). On
/// success the verified [`WorkspaceContext`] (workspace + role from the claim,
/// never a header) is stashed in the request extensions; the per-role RBAC
/// checks happen *inside* the handlers/store (a second, distinct `403`).
async fn org_auth_gate(
    State(state): State<OrgApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_token(request.headers()) else {
        return unauthorized("missing or malformed bearer token");
    };
    let claims = match state.verifier.verify(token).await {
        Ok(claims) if tier_rank(claims.tier) >= tier_rank(ORG_MIN_TIER) => claims,
        Ok(_) => return forbidden_tier(),
        Err(_) => return unauthorized("invalid or expired access token"),
    };
    request
        .extensions_mut()
        .insert(WorkspaceContext::from_claims(&claims));
    next.run(request).await
}

fn unauthorized(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
}

fn forbidden_tier() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "the organization console requires the Enterprise tier" })),
    )
        .into_response()
}

/// Maximum request body on the `/org/*` routes.
pub const MAX_ORG_BODY_BYTES: usize = 16 * 1024;

/// Build the org sub-router: every route behind the Enterprise gate.
pub fn org_router(state: OrgApiState) -> Router {
    Router::new()
        .route(
            "/org/members",
            get(list_members_handler).post(add_member_handler),
        )
        .route(
            "/org/members/{id}",
            patch(change_role_handler).delete(remove_member_handler),
        )
        .route(
            "/org/invites",
            get(crate::tenancy::list_invites_handler).post(crate::tenancy::create_invite_handler),
        )
        .route(
            "/org/invites/{id}",
            axum::routing::delete(crate::tenancy::revoke_invite_handler),
        )
        .route("/org/roles", get(list_roles_handler))
        .route("/org/audit", get(audit_handler))
        // Small JSON bodies only (a user id and a role / an invite).
        .layer(axum::extract::DefaultBodyLimit::max(MAX_ORG_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            org_auth_gate,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

/// Errors surfaced by the org handlers, each mapped to a clean HTTP status with a
/// body that leaks no server internals. (`401`/tier-`403` come from the gate.)
pub(crate) enum OrgError {
    /// Malformed input, e.g. an empty user id or an unknown role name (`400`).
    BadRequest(String),
    /// The caller's org role does not permit the action, or they are not a member
    /// (`403` - distinct from the tier gate's `403`).
    Forbidden(String),
    /// The targeted member does not exist (`404`).
    NotFound(String),
    /// The action conflicts with the org's state: a duplicate member, or the
    /// last-Owner invariant (`409`).
    Conflict(String),
    /// An unexpected store failure (`500`).
    Internal,
}

impl IntoResponse for OrgError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            OrgError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            OrgError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            OrgError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            OrgError::Conflict(m) => (StatusCode::CONFLICT, m),
            OrgError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_owned(),
            ),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// Map a store-level error onto the handler's [`OrgError`]. `target_id` fills
/// in the `NotFound`/`AlreadyMember` messages; `last_owner_message` is the
/// operation-specific wording for [`OrgStoreError::LastOwner`] ("demoted" vs.
/// "removed").
fn map_store_error(err: OrgStoreError, target_id: &str, last_owner_message: &str) -> OrgError {
    match err {
        OrgStoreError::NotFound => OrgError::NotFound(format!("no such member: {target_id}")),
        OrgStoreError::Forbidden(m) => OrgError::Forbidden(m),
        OrgStoreError::LastOwner => OrgError::Conflict(last_owner_message.to_owned()),
        OrgStoreError::AlreadyMember => {
            OrgError::Conflict(format!("user {target_id} is already a member"))
        }
        OrgStoreError::Backend => OrgError::Internal,
    }
}

// ---------------------------------------------------------------------------
// Wire DTOs
// ---------------------------------------------------------------------------

/// One member as returned on the wire.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub(crate) struct MemberView {
    /// The member's user id.
    pub user_id: String,
    /// The member's role (lowercase, e.g. `"admin"`).
    #[schema(example = "admin")]
    pub role: String,
}

/// Response for `GET /org/members`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub(crate) struct MembersResponse {
    /// The calling user's own role - so a client can render exactly the controls
    /// their role allows (the API still enforces every action).
    #[schema(example = "owner")]
    pub caller_role: String,
    /// All members, in a stable order.
    pub members: Vec<MemberView>,
}

/// Request body for `POST /org/members`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub(crate) struct AddMemberRequest {
    /// The user id to add as a member.
    #[schema(example = "usr_123")]
    pub user_id: String,
    /// The role to assign (`owner` | `admin` | `member` | `viewer`).
    #[schema(example = "member")]
    pub role: String,
}

/// Request body for `PATCH /org/members/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub(crate) struct ChangeRoleRequest {
    /// The new role to assign.
    #[schema(example = "admin")]
    pub role: String,
}

/// One role and the permissions it grants (the RBAC catalog).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub(crate) struct RoleView {
    /// The role name (lowercase).
    #[schema(example = "admin")]
    pub role: String,
    /// A short human description of the role.
    pub description: String,
    /// The permission names this role holds.
    pub permissions: Vec<String>,
}

/// Response for `GET /org/roles`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub(crate) struct RolesResponse {
    /// Every role, privilege-descending.
    pub roles: Vec<RoleView>,
}

/// One audit record as returned on the wire.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub(crate) struct AuditEntryView {
    /// Monotonic sequence id.
    pub id: u64,
    /// When the action was recorded (Unix seconds).
    pub timestamp: i64,
    /// The user id that performed the action.
    pub actor_id: String,
    /// The action name (e.g. `"member.added"`).
    #[schema(example = "member.added")]
    pub action: String,
    /// The member the action targeted, if any.
    pub target_id: Option<String>,
    /// A human-readable detail.
    pub detail: Option<String>,
}

/// Response for `GET /org/audit`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub(crate) struct AuditResponse {
    /// The audit log, most-recent first.
    pub entries: Vec<AuditEntryView>,
}

impl From<Member> for MemberView {
    fn from(m: Member) -> Self {
        Self {
            user_id: m.user_id,
            role: m.role.as_str().to_owned(),
        }
    }
}

impl From<AuditEntry> for AuditEntryView {
    fn from(e: AuditEntry) -> Self {
        Self {
            id: e.id,
            timestamp: e.timestamp,
            actor_id: e.actor_id,
            action: e.action.as_str().to_owned(),
            target_id: e.target_id,
            detail: e.detail,
        }
    }
}

// ---------------------------------------------------------------------------
// RBAC authorization helper
// ---------------------------------------------------------------------------

/// Authorize a membership mutation by the actor's role against the target's
/// current role and (for add/change) the requested new role, against
/// `required` (the base capability - [`Permission::InviteMember`] /
/// [`Permission::ChangeRole`] / [`Permission::RemoveMember`]).
///
/// Delegates to the SHARED [`amicu_auth::authorize_membership_mutation`]
/// (andypalmi/amicu-planning#133) - the SAME function
/// `amicu_store::PostgresMembershipStore` calls for the Postgres backend - so
/// the two backends can never drift.
///
/// Called from inside the in-memory store's mutation methods, under the same
/// lock as the mutation itself, so both `actor` and `target_current` are
/// freshly read.
fn authorize_membership_mutation(
    actor: Option<Role>,
    required: Permission,
    target_current: Option<Role>,
    new_role: Option<Role>,
) -> Result<(), OrgStoreError> {
    amicu_auth::authorize_membership_mutation(actor, required, target_current, new_role)
        .map_err(|m| OrgStoreError::Forbidden(m.to_owned()))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// List the organization's members and their roles.
///
/// Requires the `view_members` permission - Owner/Admin/Member hold it; a
/// **Viewer does not** and is refused with `403` (andypalmi/amicu-planning#133:
/// the enforced distinction that makes Viewer a real, more-restricted role).
#[utoipa::path(
    get,
    path = "/org/members",
    tag = "org",
    responses(
        (status = 200, description = "The members and the caller's own role.", body = MembersResponse),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below the Enterprise tier, or your role may not view the member roster."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn list_members_handler(
    State(state): State<OrgApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
) -> Result<Json<MembersResponse>, OrgError> {
    if !ctx.role.grants(Permission::ViewMembers) {
        return Err(OrgError::Forbidden(
            "your role may not view the member roster".to_owned(),
        ));
    }
    let members = state
        .store
        .members(ctx.workspace)
        .await
        .into_iter()
        .map(MemberView::from)
        .collect();
    Ok(Json(MembersResponse {
        caller_role: ctx.role.as_str().to_owned(),
        members,
    }))
}

/// Invite/add a member with a role.
#[utoipa::path(
    post,
    path = "/org/members",
    tag = "org",
    request_body = AddMemberRequest,
    responses(
        (status = 201, description = "The member was added.", body = MemberView),
        (status = 400, description = "Empty user id or unknown role."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, or your role may not add this member."),
        (status = 409, description = "That user is already a member."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn add_member_handler(
    State(state): State<OrgApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
    Json(body): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<MemberView>), OrgError> {
    let user_id = body.user_id.trim();
    if user_id.is_empty() {
        return Err(OrgError::BadRequest(
            "`user_id` must not be empty".to_owned(),
        ));
    }
    // `user_id` is client input (the request body). A user id is a UUID (the
    // token `sub`), so a non-UUID value is a malformed request: reject it here
    // with 400 rather than letting the store surface it as a 500.
    if uuid::Uuid::parse_str(user_id).is_err() {
        return Err(OrgError::BadRequest(
            "`user_id` must be a valid UUID".to_owned(),
        ));
    }
    let role = Role::parse_wire(&body.role)
        .ok_or_else(|| OrgError::BadRequest(format!("unknown role: {}", body.role)))?;

    // The mutation re-checks the actor's role fresh under the store's own
    // lock/transaction (see the module docs' "Atomicity" section) rather than
    // trusting the caller's claim role.
    let assigned = state
        .store
        .add_member_audited(ctx.workspace, &ctx.user_id, user_id, role)
        .await
        .map_err(|e| map_store_error(e, user_id, "cannot demote the last remaining Owner"))?;
    amicu_auth::security::SecurityEvent::new("member_added", amicu_auth::security::Severity::Info)
        .outcome("ok")
        .workspace(&ctx.workspace.to_string())
        .user(&ctx.user_id)
        .target_user(user_id)
        .role(assigned.as_str())
        .emit();

    Ok((
        StatusCode::CREATED,
        Json(MemberView {
            user_id: user_id.to_owned(),
            role: assigned.as_str().to_owned(),
        }),
    ))
}

/// Change a member's role.
#[utoipa::path(
    patch,
    path = "/org/members/{id}",
    tag = "org",
    params(("id" = String, Path, description = "The member's user id.")),
    request_body = ChangeRoleRequest,
    responses(
        (status = 200, description = "The member's new role.", body = MemberView),
        (status = 400, description = "Unknown role."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, or your role may not make this change."),
        (status = 404, description = "No such member."),
        (status = 409, description = "The change would demote the last remaining Owner."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn change_role_handler(
    State(state): State<OrgApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
    Path(target_id): Path<String>,
    Json(body): Json<ChangeRoleRequest>,
) -> Result<Json<MemberView>, OrgError> {
    let new_role = Role::parse_wire(&body.role)
        .ok_or_else(|| OrgError::BadRequest(format!("unknown role: {}", body.role)))?;

    state
        .store
        .set_role_audited(ctx.workspace, &ctx.user_id, &target_id, new_role)
        .await
        .map_err(|e| map_store_error(e, &target_id, "cannot demote the last remaining Owner"))?;
    amicu_auth::security::SecurityEvent::new("role_changed", amicu_auth::security::Severity::Info)
        .outcome("ok")
        .workspace(&ctx.workspace.to_string())
        .user(&ctx.user_id)
        .target_user(&target_id)
        .role(new_role.as_str())
        .emit();

    Ok(Json(MemberView {
        user_id: target_id,
        role: new_role.as_str().to_owned(),
    }))
}

/// Remove a member.
#[utoipa::path(
    delete,
    path = "/org/members/{id}",
    tag = "org",
    params(("id" = String, Path, description = "The member's user id.")),
    responses(
        (status = 200, description = "The removed member.", body = MemberView),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, or your role may not remove this member."),
        (status = 404, description = "No such member."),
        (status = 409, description = "Removing this member would leave no Owner."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn remove_member_handler(
    State(state): State<OrgApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
    Path(target_id): Path<String>,
) -> Result<Json<MemberView>, OrgError> {
    let removed = state
        .store
        .remove_member_audited(ctx.workspace, &ctx.user_id, &target_id)
        .await
        .map_err(|e| map_store_error(e, &target_id, "cannot remove the last remaining Owner"))?;
    amicu_auth::security::SecurityEvent::new(
        "member_removed",
        amicu_auth::security::Severity::Info,
    )
    .outcome("ok")
    .workspace(&ctx.workspace.to_string())
    .user(&ctx.user_id)
    .target_user(&target_id)
    .role(removed.as_str())
    .emit();

    Ok(Json(MemberView {
        user_id: target_id,
        role: removed.as_str().to_owned(),
    }))
}

/// The RBAC catalog: every role and the permissions it grants.
#[utoipa::path(
    get,
    path = "/org/roles",
    tag = "org",
    responses(
        (status = 200, description = "The role -> permission catalog.", body = RolesResponse),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below the Enterprise tier."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn list_roles_handler(
    State(_state): State<OrgApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
) -> Result<Json<RolesResponse>, OrgError> {
    // Every role holds `view_roles` (see the matrix on `Role::permissions`),
    // so this is enforced identically to every other endpoint even though it
    // never currently refuses anyone.
    if !ctx.role.grants(Permission::ViewRoles) {
        return Err(OrgError::Forbidden(
            "your role may not view the role catalog".to_owned(),
        ));
    }
    let roles = ALL_ROLES
        .iter()
        .map(|r| RoleView {
            role: r.as_str().to_owned(),
            description: role_description(*r).to_owned(),
            // The SAME matrix (`Role::permissions`) enforcement consults - the
            // catalog can never drift from what is actually enforced.
            permissions: r
                .permissions()
                .iter()
                .map(|p| p.as_str().to_owned())
                .collect(),
        })
        .collect();
    Ok(Json(RolesResponse { roles }))
}

/// The audit log, most-recent first. Requires the `view_audit` permission
/// (Owner/Admin).
#[utoipa::path(
    get,
    path = "/org/audit",
    tag = "org",
    responses(
        (status = 200, description = "The audit log, most-recent first.", body = AuditResponse),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, or your role may not view the audit log."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn audit_handler(
    State(state): State<OrgApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
) -> Result<Json<AuditResponse>, OrgError> {
    if !ctx.role.grants(Permission::ViewAudit) {
        return Err(OrgError::Forbidden(
            "your role may not view the audit log".to_owned(),
        ));
    }
    let entries = state
        .store
        .audit(ctx.workspace)
        .await
        .into_iter()
        .map(AuditEntryView::from)
        .collect();
    Ok(Json(AuditResponse { entries }))
}
