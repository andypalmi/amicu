//! HTTP surface for the Enterprise **organization console**: membership,
//! role-based access control (RBAC), and an append-only audit log.
//!
//! # Shape
//! Every endpoint is auth-gated **and** Enterprise-tier-gated (see
//! [`org_auth_gate`]): a caller below Enterprise is turned away with a `403`
//! *before* any org state is read or serialized. The caller's organization role
//! is then derived from their identity (`sub`) against the [`OrgStore`],
//! **scoped to the caller's organization** (see "Multi-tenant scoping" below).
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
//! independent `403` ([`OrgError::Forbidden`]) is returned by the handlers when
//! an authenticated Enterprise caller's **org role** does not permit the action
//! (e.g. a `Member` trying to remove someone). The messages differ so the two
//! are never confused.
//!
//! # In-memory, single-node (dev) - same caveat as auth/billing
//! The store is a `Mutex`-guarded in-memory map ([`InMemoryOrgStore`]); all state
//! is lost on restart and cannot be shared across replicas. It is shaped behind
//! the [`OrgStore`] trait so a real (Postgres) implementation can follow. Data
//! invariants that a real backend would enforce in a transaction (member
//! uniqueness, "at least one Owner must remain", *and* that a mutation is never
//! persisted without its audit record) are enforced atomically inside single
//! store methods (one lock acquisition each) - see "Atomicity" below.
//!
//! # Atomicity: mutation + audit, and RBAC, under one lock
//! Two correctness properties are held by construction rather than by
//! convention:
//! - **A mutation and its audit record are never split.** `add_member_audited`,
//!   `set_role_audited`, `remove_member_audited`, and `bootstrap_owner` each
//!   take the store's mutex exactly once and write both the membership change
//!   *and* the audit entry before releasing it. A task cancelled at any point
//!   either did nothing or did both; it can never leave a membership change
//!   with no audit trail.
//! - **RBAC authorization is checked against *both* roles read under the same
//!   lock as the mutation.** The handlers only resolve *whether* the caller is
//!   a member at all up front (via [`resolve_caller_role`], which also does
//!   the first-Owner bootstrap); they never pass a pre-resolved `Role` into a
//!   mutation. Instead the `*_audited` store methods take the actor's *id* and
//!   re-read the actor's *and* the target's current roles from the map inside
//!   the same critical section that performs the mutation, and authorize
//!   against those freshly-read roles. This closes two TOCTOUs: a concurrent
//!   role change on the *target* (e.g. a racing Owner promoting them) can no
//!   longer slip between an earlier authorization check and the mutation that
//!   trusted it, and neither can a concurrent change to the *actor's own*
//!   role or membership (e.g. an Owner revoking a compromised Admin at the
//!   same moment that Admin's own already-in-flight mutation is landing) - if
//!   the actor has been removed or demoted by the time their mutation is
//!   actually serialized, it is authorized against who they are *now*, not
//!   who they were when the request started.
//!
//! # Multi-tenant scoping (organization id)
//! Every store method takes an [`amicu_store::WorkspaceId`] - the same type
//! that unifies "workspace" and "Enterprise org" elsewhere in the codebase -
//! and all membership/audit state is partitioned by it. A caller selects their
//! organization with the `X-Amicu-Workspace-Id` request header (a UUID); when
//! the header is absent, [`amicu_store::WorkspaceId::DEFAULT`] is used, which
//! is exactly today's single-tenant behavior (and is what every caller that
//! predates this header gets, unchanged).
//!
//! **What this does and does not fix.** Distinct workspace ids now get fully
//! separate member lists and audit logs - a request scoped to workspace A can
//! never read or mutate workspace B's state, because every store lookup is
//! keyed by the workspace id and RBAC still requires the caller to be a member
//! of *that* workspace. What remains open, and is **not** solved here, is
//! *authenticating* which workspace a caller may act as: the header is
//! client-supplied and not derived from the caller's identity, because
//! [`amicu_auth::Claims`] carries no workspace/org claim and there is no
//! login-routing or org-provisioning flow anywhere in the codebase yet (the
//! same gap `amicu-store`'s own `WorkspaceId` docs call out for the Postgres
//! auth/config stores: "until login-routing exists, the self-host default is
//! used everywhere"). Concretely:
//! - For the default workspace (no header, today's only real deployment
//!   shape) nothing changes: same single-tenant behavior as before.
//! - For any other workspace id, the first-Owner bootstrap (see below) is
//!   disabled entirely - not merely "gated off reads" but off *every*
//!   endpoint - so a caller who merely guesses or is handed a workspace id
//!   can never silently seize it as Owner. Populating a non-default
//!   workspace's very first member currently requires calling
//!   [`OrgStore::add_member`] directly (e.g. a future admin/provisioning path
//!   or a DB migration) rather than any HTTP endpoint. This is the piece that
//!   genuinely needs the not-yet-built persistence/tenancy plumbing (an
//!   authenticated org claim, or an org-provisioning API) to close; until then
//!   this vertical deliberately leaves non-default workspaces reachable only
//!   by that out-of-band seeding, rather than pretending an unauthenticated
//!   header is a security boundary for *creating* organizations.
//!
//! # First-Owner bootstrap
//! For a single-node dev deployment nobody is a member until someone is added.
//! The first Enterprise caller to reach the console when the org has **no
//! members** is enrolled as its [`Role::Owner`] (an audited `org.bootstrapped`
//! action); everyone after that must be invited. This makes the console usable
//! out of the box without faking data. As described above, this bootstrap only
//! fires for the default (single-tenant) workspace; see "Multi-tenant scoping".
//!
//! # SSO is out of scope (deferred post-v1)
//! Single sign-on (SAML/OIDC federation, SCIM provisioning) is intentionally not
//! built here - see the `andypalmi/amicu-planning#92` SSO follow-up. Members are
//! keyed by their first-party user id.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_auth::{Claims, Tier};
use amicu_store::WorkspaceId;
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

use crate::engine::AccessVerifier;

// ---------------------------------------------------------------------------
// RBAC domain: roles + permissions
// ---------------------------------------------------------------------------

/// A fixed organization role. The permission each role grants is defined by
/// [`Role::permissions`]; the ordering here is privilege-descending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Full control, including granting/revoking Owner and Admin. At least one
    /// Owner must always remain (enforced by the store).
    Owner,
    /// Manages membership (invite/remove/change role) but may not grant or touch
    /// Owner/Admin members, and may not create new Owners/Admins.
    Admin,
    /// A regular member: may view membership and the role catalog only.
    Member,
    /// Read-only: may view membership and the role catalog only.
    Viewer,
}

/// A discrete capability a [`Role`] may hold. The catalog endpoint surfaces the
/// role → permission mapping so a client can render exactly the controls a role
/// is allowed to use (the API remains the enforcement point regardless).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// See the member list and each member's role.
    ViewMembers,
    /// See the role/permission catalog.
    ViewRoles,
    /// See the audit log.
    ViewAudit,
    /// Invite/add a member.
    InviteMember,
    /// Change a member's role.
    ChangeRole,
    /// Remove a member.
    RemoveMember,
    /// Grant or modify a privileged (Owner/Admin) member - Owner only.
    AssignPrivilegedRole,
}

impl Role {
    /// The lowercase wire name for this role.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
            Role::Viewer => "viewer",
        }
    }

    /// Parse a case-insensitive wire role name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "owner" => Some(Role::Owner),
            "admin" => Some(Role::Admin),
            "member" => Some(Role::Member),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }

    /// A short human description of the role.
    fn description(self) -> &'static str {
        match self {
            Role::Owner => {
                "Full control, including granting Owner/Admin. At least one Owner must remain."
            }
            Role::Admin => "Manages membership, but cannot grant or modify Owner/Admin members.",
            Role::Member => "Regular member with read-only visibility into membership.",
            Role::Viewer => "Read-only visibility into membership.",
        }
    }

    /// Whether this role is privileged (Owner or Admin). Privileged members may
    /// only be created, modified, or removed by an Owner.
    fn is_privileged(self) -> bool {
        matches!(self, Role::Owner | Role::Admin)
    }

    /// The full permission set this role holds.
    #[must_use]
    pub fn permissions(self) -> Vec<Permission> {
        match self {
            Role::Owner => vec![
                Permission::ViewMembers,
                Permission::ViewRoles,
                Permission::ViewAudit,
                Permission::InviteMember,
                Permission::ChangeRole,
                Permission::RemoveMember,
                Permission::AssignPrivilegedRole,
            ],
            Role::Admin => vec![
                Permission::ViewMembers,
                Permission::ViewRoles,
                Permission::ViewAudit,
                Permission::InviteMember,
                Permission::ChangeRole,
                Permission::RemoveMember,
            ],
            Role::Member | Role::Viewer => {
                vec![Permission::ViewMembers, Permission::ViewRoles]
            }
        }
    }

    /// May this role manage membership at all (invite/remove/change)?
    fn can_manage_members(self) -> bool {
        matches!(self, Role::Owner | Role::Admin)
    }

    /// May this role read the audit log?
    fn can_view_audit(self) -> bool {
        matches!(self, Role::Owner | Role::Admin)
    }
}

impl Permission {
    /// The wire name for this permission.
    fn as_str(self) -> &'static str {
        match self {
            Permission::ViewMembers => "view_members",
            Permission::ViewRoles => "view_roles",
            Permission::ViewAudit => "view_audit",
            Permission::InviteMember => "invite_member",
            Permission::ChangeRole => "change_role",
            Permission::RemoveMember => "remove_member",
            Permission::AssignPrivilegedRole => "assign_privileged_role",
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
    /// Monotonic sequence id (1-based) within this organization's log.
    pub id: u64,
    /// When the action was recorded (Unix seconds).
    pub timestamp: i64,
    /// The user id that performed the action.
    pub actor_id: String,
    /// What happened.
    pub action: AuditAction,
    /// The member the action targeted, if any.
    pub target_id: Option<String>,
    /// A human-readable detail (e.g. `"member → admin"`).
    pub detail: Option<String>,
}

/// The set of audited privileged actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    /// The organization was bootstrapped by its first Owner.
    OrgBootstrapped,
    /// A member was added.
    MemberAdded,
    /// A member's role was changed.
    RoleChanged,
    /// A member was removed.
    MemberRemoved,
}

impl AuditAction {
    /// The wire name for this action.
    fn as_str(self) -> &'static str {
        match self {
            AuditAction::OrgBootstrapped => "org.bootstrapped",
            AuditAction::MemberAdded => "member.added",
            AuditAction::RoleChanged => "member.role_changed",
            AuditAction::MemberRemoved => "member.removed",
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
    /// target's role under the same lock as the mutation (see the module
    /// docs' "Atomicity" section).
    Forbidden(String),
    /// The mutation would leave the organization with no Owner.
    LastOwner,
    /// (`add_member_audited` only) the user is already a member.
    AlreadyMember,
}

/// Organization persistence seam (multi-tenant: every method is scoped to a
/// [`WorkspaceId`]).
///
/// Shaped so a real multi-tenant, Postgres-backed implementation can follow: the
/// data invariants (`add_member_audited` is a no-op on a duplicate;
/// `set_role_audited`/`remove_member_audited` refuse to drop the last Owner,
/// and check RBAC against a freshly-read target role) are enforced atomically
/// per call - the mutation and its audit record are written under one lock
/// acquisition, exactly what a transactional backend would do.
#[async_trait]
pub trait OrgStore: Send + Sync {
    /// All members of `org`, in a stable (user-id) order.
    async fn members(&self, org: WorkspaceId) -> Vec<Member>;
    /// The role of `user_id` within `org`, or `None` if they are not a member.
    async fn role_of(&self, org: WorkspaceId, user_id: &str) -> Option<Role>;
    /// The current number of members of `org`.
    async fn member_count(&self, org: WorkspaceId) -> usize;
    /// Add a member directly, bypassing RBAC and without an audit record.
    /// Used to seed org state (tests; a future admin/provisioning path can use
    /// the same primitive to populate a non-default workspace's first member).
    /// Returns `true` if inserted, `false` if `user_id` was already a member.
    async fn add_member(&self, org: WorkspaceId, user_id: &str, role: Role) -> bool;
    /// Enroll `user_id` as [`Role::Owner`] **iff** `org` currently has no
    /// members, atomically with the `org.bootstrapped` audit record. Returns
    /// `true` if it did the bootstrap.
    async fn bootstrap_owner(&self, org: WorkspaceId, user_id: &str) -> bool;
    /// Add `target_id` with `role`, RBAC-authorized against `actor_id`'s
    /// *current* role - re-read from the map, and `actor_id` is also the audit
    /// actor - under the same lock as the mutation, atomically with its
    /// `member.added` audit record. Errors with [`OrgStoreError::Forbidden`]
    /// if `actor_id` is not (or is no longer) a member, or may not grant
    /// `role`, or [`OrgStoreError::AlreadyMember`] if `target_id` already
    /// belongs to `org`.
    async fn add_member_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        role: Role,
    ) -> Result<Role, OrgStoreError>;
    /// Change `target_id`'s role to `new_role`, RBAC-authorized against both
    /// `actor_id`'s and `target_id`'s *current* roles - both re-read from the
    /// map under the same lock as the mutation - atomically with its
    /// `member.role_changed` audit record. Returns the previous role. Errors
    /// with [`OrgStoreError::NotFound`] if `target_id` is not a member,
    /// [`OrgStoreError::Forbidden`] if `actor_id` is not (or is no longer) a
    /// member, or may not make this change, or [`OrgStoreError::LastOwner`] if
    /// it would demote the only remaining Owner.
    async fn set_role_audited(
        &self,
        org: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        new_role: Role,
    ) -> Result<Role, OrgStoreError>;
    /// Remove `target_id`, RBAC-authorized against both `actor_id`'s and
    /// `target_id`'s *current* roles - both re-read from the map under the
    /// same lock as the mutation - atomically with its `member.removed` audit
    /// record. Returns the removed role. Errors with
    /// [`OrgStoreError::NotFound`] if `target_id` is not a member,
    /// [`OrgStoreError::Forbidden`] if `actor_id` is not (or is no longer) a
    /// member, or may not remove them, or [`OrgStoreError::LastOwner`] if it
    /// would remove the only remaining Owner.
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
    /// user_id → role. `BTreeMap` for a deterministic member order.
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
/// mutex; a real backend would instead scope each transaction to the
/// workspace (see `amicu-store`'s workspace-scoped stores).
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

    async fn role_of(&self, org: WorkspaceId, user_id: &str) -> Option<Role> {
        self.inner
            .lock()
            .expect("org store poisoned")
            .get(&org)
            .and_then(|s| s.members.get(user_id))
            .copied()
    }

    async fn member_count(&self, org: WorkspaceId) -> usize {
        self.inner
            .lock()
            .expect("org store poisoned")
            .get(&org)
            .map_or(0, |s| s.members.len())
    }

    async fn add_member(&self, org: WorkspaceId, user_id: &str, role: Role) -> bool {
        let mut state = self.inner.lock().expect("org store poisoned");
        let org_state = state.entry(org).or_default();
        if org_state.members.contains_key(user_id) {
            return false;
        }
        org_state.members.insert(user_id.to_owned(), role);
        true
    }

    async fn bootstrap_owner(&self, org: WorkspaceId, user_id: &str) -> bool {
        let mut state = self.inner.lock().expect("org store poisoned");
        let org_state = state.entry(org).or_default();
        if !org_state.members.is_empty() {
            return false;
        }
        org_state.members.insert(user_id.to_owned(), Role::Owner);
        append_audit_locked(
            org_state,
            NewAuditEntry {
                actor_id: user_id.to_owned(),
                action: AuditAction::OrgBootstrapped,
                target_id: Some(user_id.to_owned()),
                detail: Some("bootstrapped as first owner".to_owned()),
            },
        );
        true
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
        authorize_membership_mutation(actor, None, Some(role))?;
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
        authorize_membership_mutation(actor, Some(current), Some(new_role))?;
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
                detail: Some(format!("{} → {}", current.as_str(), new_role.as_str())),
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
        authorize_membership_mutation(actor, Some(current), None)?;
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

/// Current wall-clock time in seconds since the Unix epoch. Mirrors the billing
/// vertical's clock; there is no injected clock in this bootstrap.
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

/// The request header carrying the caller's organization id (a UUID, parsed
/// into a [`WorkspaceId`]). Absent means [`WorkspaceId::DEFAULT`] - see the
/// module docs' "Multi-tenant scoping" section for what this does and does
/// not secure today.
const ORG_WORKSPACE_HEADER: &str = "x-amicu-workspace-id";

/// Shared state for the org endpoints: the access-token verifier that backs the
/// Enterprise gate, and the org store.
#[derive(Clone)]
pub struct OrgApiState {
    verifier: Arc<dyn AccessVerifier>,
    store: Arc<dyn OrgStore>,
}

impl OrgApiState {
    /// Assemble org state from a verifier and a store.
    #[must_use]
    pub fn new(verifier: Arc<dyn AccessVerifier>, store: Arc<dyn OrgStore>) -> Self {
        Self { verifier, store }
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

/// Extract the caller's organization id from the [`ORG_WORKSPACE_HEADER`], or
/// [`WorkspaceId::DEFAULT`] if it is absent. Rejects a malformed header
/// (non-ASCII, or not a UUID) with [`OrgError::BadRequest`].
fn workspace_id_from_headers(headers: &HeaderMap) -> Result<WorkspaceId, OrgError> {
    let Some(value) = headers.get(ORG_WORKSPACE_HEADER) else {
        return Ok(WorkspaceId::DEFAULT);
    };
    let text = value.to_str().map_err(|_| {
        OrgError::BadRequest(format!("the {ORG_WORKSPACE_HEADER} header must be ASCII"))
    })?;
    text.trim().parse::<WorkspaceId>().map_err(|_| {
        OrgError::BadRequest(format!("the {ORG_WORKSPACE_HEADER} header must be a UUID"))
    })
}

/// Auth + Enterprise-tier gate for the org endpoints.
///
/// Rejects requests without a valid token (`401`) and authenticated requests
/// below Enterprise (`403`, tier), and a malformed organization header
/// (`400`). Cryptographic token verification always runs *before* the
/// workspace-header format check, so an invalid/expired token is reported as
/// `401` even when the header is also malformed - request *shape* is never
/// validated ahead of *identity*. On success the verified [`Claims`] and the
/// resolved [`WorkspaceId`] are stashed in the request extensions. The
/// per-role RBAC checks happen *inside* the handlers (a second, distinct
/// `403`).
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
    let workspace = match workspace_id_from_headers(request.headers()) {
        Ok(workspace) => workspace,
        Err(err) => return err.into_response(),
    };
    request.extensions_mut().insert(claims);
    request.extensions_mut().insert(workspace);
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
        .route("/org/roles", get(list_roles_handler))
        .route("/org/audit", get(audit_handler))
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
    /// Malformed input, e.g. an empty user id, an unknown role name, or a
    /// malformed organization header (`400`).
    BadRequest(String),
    /// The caller's org role does not permit the action, or they are not a member
    /// (`403` - distinct from the tier gate's `403`).
    Forbidden(String),
    /// The targeted member does not exist (`404`).
    NotFound(String),
    /// The action conflicts with the org's state: a duplicate member, or the
    /// last-Owner invariant (`409`).
    Conflict(String),
}

impl IntoResponse for OrgError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            OrgError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            OrgError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            OrgError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            OrgError::Conflict(m) => (StatusCode::CONFLICT, m),
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
// RBAC authorization helpers
// ---------------------------------------------------------------------------

/// Authorize a membership mutation by the actor's role against the target's
/// current role and (for add/change) the requested new role.
///
/// - `actor` must currently be a member (`None` - not, or no longer, a
///   member - is always refused).
/// - Only Owner/Admin may manage membership at all.
/// - Only an **Owner** may grant a privileged role (Owner/Admin) or modify/remove
///   an existing privileged member. (An Admin manages Members/Viewers only.)
///
/// Called from inside the store's mutation methods, under the same lock
/// acquisition as the mutation itself, so both `actor` and `target_current`
/// are always freshly read (see the module docs' "Atomicity" section) - a
/// concurrent removal or demotion of the actor themselves, mid-flight, is
/// authorized against who they are *now*, not who they were when the
/// request started.
fn authorize_membership_mutation(
    actor: Option<Role>,
    target_current: Option<Role>,
    new_role: Option<Role>,
) -> Result<(), OrgStoreError> {
    let Some(actor) = actor else {
        return Err(OrgStoreError::Forbidden(
            "you are not a member of this organization".to_owned(),
        ));
    };
    if !actor.can_manage_members() {
        return Err(OrgStoreError::Forbidden(
            "your role may not manage organization membership".to_owned(),
        ));
    }
    if actor == Role::Admin {
        if new_role.is_some_and(Role::is_privileged) {
            return Err(OrgStoreError::Forbidden(
                "only an Owner may grant the Owner or Admin role".to_owned(),
            ));
        }
        if target_current.is_some_and(Role::is_privileged) {
            return Err(OrgStoreError::Forbidden(
                "only an Owner may modify or remove an Owner or Admin".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Resolve the caller's org role, bootstrapping them as the first Owner when the
/// org is empty **and** eligible for bootstrap (see [`bootstrap_eligible`]).
/// Returns `403` when the caller is (an Enterprise user but) not a member of
/// the established organization.
async fn resolve_caller_role(
    state: &OrgApiState,
    org: WorkspaceId,
    caller: &str,
) -> Result<Role, OrgError> {
    if let Some(role) = state.store.role_of(org, caller).await {
        return Ok(role);
    }
    // Nobody is a member yet: the first Enterprise caller claims the org as
    // Owner - but only for the default (single-tenant) workspace. A caller
    // that names any other workspace can never silently seize it this way;
    // see the module docs' "Multi-tenant scoping" section.
    if bootstrap_eligible(org) && state.store.bootstrap_owner(org, caller).await {
        return Ok(Role::Owner);
    }
    // Lost the bootstrap race, or the org exists (or is bootstrap-ineligible)
    // and the caller is not in it.
    match state.store.role_of(org, caller).await {
        Some(role) => Ok(role),
        None => Err(OrgError::Forbidden(
            "you are not a member of this organization".to_owned(),
        )),
    }
}

/// Whether `org` may be silently bootstrapped by its first caller. Only the
/// default (single-tenant, self-host) workspace is eligible; any explicitly
/// named workspace must be populated out-of-band (see the module docs'
/// "Multi-tenant scoping" section for why).
fn bootstrap_eligible(org: WorkspaceId) -> bool {
    org == WorkspaceId::DEFAULT
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// List the organization's members and their roles.
#[utoipa::path(
    get,
    path = "/org/members",
    tag = "org",
    params(
        ("x-amicu-workspace-id" = Option<String>, Header, description = "The caller's organization id (a UUID). Defaults to the single-tenant workspace when absent."),
    ),
    responses(
        (status = 200, description = "The members and the caller's own role.", body = MembersResponse),
        (status = 400, description = "Malformed organization header."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below the Enterprise tier, or not a member of the organization."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn list_members_handler(
    State(state): State<OrgApiState>,
    Extension(claims): Extension<Claims>,
    Extension(workspace): Extension<WorkspaceId>,
) -> Result<Json<MembersResponse>, OrgError> {
    let role = resolve_caller_role(&state, workspace, &claims.sub).await?;
    let members = state
        .store
        .members(workspace)
        .await
        .into_iter()
        .map(MemberView::from)
        .collect();
    Ok(Json(MembersResponse {
        caller_role: role.as_str().to_owned(),
        members,
    }))
}

/// Invite/add a member with a role.
#[utoipa::path(
    post,
    path = "/org/members",
    tag = "org",
    params(
        ("x-amicu-workspace-id" = Option<String>, Header, description = "The caller's organization id (a UUID). Defaults to the single-tenant workspace when absent."),
    ),
    request_body = AddMemberRequest,
    responses(
        (status = 201, description = "The member was added.", body = MemberView),
        (status = 400, description = "Empty user id, unknown role, or malformed organization header."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, not a member, or your role may not add this member."),
        (status = 409, description = "That user is already a member."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn add_member_handler(
    State(state): State<OrgApiState>,
    Extension(claims): Extension<Claims>,
    Extension(workspace): Extension<WorkspaceId>,
    Json(body): Json<AddMemberRequest>,
) -> Result<(StatusCode, Json<MemberView>), OrgError> {
    // Establishes membership (bootstrapping the first Owner if the org is
    // empty and eligible) and turns away a non-member fast. The mutation
    // itself re-checks the actor's role fresh under the store's lock (see the
    // module docs' "Atomicity" section) rather than trusting this snapshot.
    resolve_caller_role(&state, workspace, &claims.sub).await?;

    let user_id = body.user_id.trim();
    if user_id.is_empty() {
        return Err(OrgError::BadRequest(
            "`user_id` must not be empty".to_owned(),
        ));
    }
    let role = Role::parse(&body.role)
        .ok_or_else(|| OrgError::BadRequest(format!("unknown role: {}", body.role)))?;

    let assigned = state
        .store
        .add_member_audited(workspace, &claims.sub, user_id, role)
        .await
        .map_err(|e| map_store_error(e, user_id, "cannot demote the last remaining Owner"))?;

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
    params(
        ("id" = String, Path, description = "The member's user id."),
        ("x-amicu-workspace-id" = Option<String>, Header, description = "The caller's organization id (a UUID). Defaults to the single-tenant workspace when absent."),
    ),
    request_body = ChangeRoleRequest,
    responses(
        (status = 200, description = "The member's new role.", body = MemberView),
        (status = 400, description = "Unknown role, or malformed organization header."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, not a member, or your role may not make this change."),
        (status = 404, description = "No such member."),
        (status = 409, description = "The change would demote the last remaining Owner."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn change_role_handler(
    State(state): State<OrgApiState>,
    Extension(claims): Extension<Claims>,
    Extension(workspace): Extension<WorkspaceId>,
    Path(target_id): Path<String>,
    Json(body): Json<ChangeRoleRequest>,
) -> Result<Json<MemberView>, OrgError> {
    // See add_member_handler: the resolved role is not trusted for
    // authorization - only membership/bootstrap is established here.
    resolve_caller_role(&state, workspace, &claims.sub).await?;
    let new_role = Role::parse(&body.role)
        .ok_or_else(|| OrgError::BadRequest(format!("unknown role: {}", body.role)))?;

    state
        .store
        .set_role_audited(workspace, &claims.sub, &target_id, new_role)
        .await
        .map_err(|e| map_store_error(e, &target_id, "cannot demote the last remaining Owner"))?;

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
    params(
        ("id" = String, Path, description = "The member's user id."),
        ("x-amicu-workspace-id" = Option<String>, Header, description = "The caller's organization id (a UUID). Defaults to the single-tenant workspace when absent."),
    ),
    responses(
        (status = 200, description = "The removed member.", body = MemberView),
        (status = 400, description = "Malformed organization header."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, not a member, or your role may not remove this member."),
        (status = 404, description = "No such member."),
        (status = 409, description = "Removing this member would leave no Owner."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn remove_member_handler(
    State(state): State<OrgApiState>,
    Extension(claims): Extension<Claims>,
    Extension(workspace): Extension<WorkspaceId>,
    Path(target_id): Path<String>,
) -> Result<Json<MemberView>, OrgError> {
    // See add_member_handler: the resolved role is not trusted for
    // authorization - only membership/bootstrap is established here.
    resolve_caller_role(&state, workspace, &claims.sub).await?;

    let removed = state
        .store
        .remove_member_audited(workspace, &claims.sub, &target_id)
        .await
        .map_err(|e| map_store_error(e, &target_id, "cannot remove the last remaining Owner"))?;

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
    params(
        ("x-amicu-workspace-id" = Option<String>, Header, description = "The caller's organization id (a UUID). Defaults to the single-tenant workspace when absent."),
    ),
    responses(
        (status = 200, description = "The role → permission catalog.", body = RolesResponse),
        (status = 400, description = "Malformed organization header."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below the Enterprise tier, or not a member of the organization."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn list_roles_handler(
    State(state): State<OrgApiState>,
    Extension(claims): Extension<Claims>,
    Extension(workspace): Extension<WorkspaceId>,
) -> Result<Json<RolesResponse>, OrgError> {
    // Membership (any role) is required to read the catalog.
    resolve_caller_role(&state, workspace, &claims.sub).await?;
    let roles = ALL_ROLES
        .iter()
        .map(|r| RoleView {
            role: r.as_str().to_owned(),
            description: r.description().to_owned(),
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
    params(
        ("x-amicu-workspace-id" = Option<String>, Header, description = "The caller's organization id (a UUID). Defaults to the single-tenant workspace when absent."),
    ),
    responses(
        (status = 200, description = "The audit log, most-recent first.", body = AuditResponse),
        (status = 400, description = "Malformed organization header."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, not a member, or your role may not view the audit log."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn audit_handler(
    State(state): State<OrgApiState>,
    Extension(claims): Extension<Claims>,
    Extension(workspace): Extension<WorkspaceId>,
) -> Result<Json<AuditResponse>, OrgError> {
    let role = resolve_caller_role(&state, workspace, &claims.sub).await?;
    if !role.can_view_audit() {
        return Err(OrgError::Forbidden(
            "your role may not view the audit log".to_owned(),
        ));
    }
    let entries = state
        .store
        .audit(workspace)
        .await
        .into_iter()
        .map(AuditEntryView::from)
        .collect();
    Ok(Json(AuditResponse { entries }))
}
