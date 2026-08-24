//! Postgres-backed workspace membership store: the authority for WHO may act
//! WHERE, as WHAT.
//!
//! Workspace-scoped. Every operation that reads or writes the members OF a
//! workspace runs inside a [`begin_scoped`](crate::begin_scoped) transaction
//! that binds `app.current_workspace`, so the row-level-security policy admits
//! exactly that workspace's rows - and the explicit `workspace_id` predicate on
//! every statement is the primary defense. The one legitimately cross-workspace
//! read, [`list_workspaces`](PostgresMembershipStore::list_workspaces) ("which
//! workspaces is this user in?"), runs under the USER context
//! ([`begin_scoped_user`](crate::begin_scoped_user)) instead, so a user sees
//! their own memberships across workspaces and nothing else.

use amicu_auth::AuthError;
use amicu_auth::membership::{MembershipContext, MembershipStore, WorkspaceMembership};
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    WorkspaceId, begin_scoped, begin_scoped_user, map_db, meets_enterprise, now_unix_seconds,
    read_workspace_tier, tier_from_str,
};

/// A workspace role, in descending privilege. Stored as lowercase text.
///
/// Defined in `amicu-auth` (`amicu_auth::tenant::Role`) since
/// andypalmi/amicu-planning#129 so the access-token `wsrole` claim can carry it
/// without a store dependency; re-exported here unchanged, so
/// `amicu_store::membership::Role` keeps resolving.
pub use amicu_auth::Role;

/// A membership row: the join of a user into a workspace at a role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The workspace.
    pub workspace_id: WorkspaceId,
    /// The user (global id / token `sub`, UUID text).
    pub user_id: String,
    /// The user's role in the workspace.
    pub role: Role,
    /// Who added them (the founding Owner has `None`).
    pub invited_by: Option<String>,
    /// When they joined, Unix seconds.
    pub joined_at: i64,
}

// ---------------------------------------------------------------------------
// Org-console audited mutations (andypalmi/amicu-planning#130)
//
// The Enterprise organization console runs its privileged membership mutations
// through this store so authorization is re-checked against the ACTOR's and
// TARGET's *current* roles, read under the same transaction (and the same row
// locks) as the mutation - carrying the #117 in-memory TOCTOU fix onto Postgres
// - and so each mutation's audit record is written atomically alongside it.
// ---------------------------------------------------------------------------

/// A privileged org-console action, as recorded in `org_audit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgAuditAction {
    /// A member was added.
    MemberAdded,
    /// A member's role was changed.
    RoleChanged,
    /// A member was removed.
    MemberRemoved,
    /// A pending invite was created (an offer to join at a role).
    InviteCreated,
    /// An invite was accepted, enrolling the redeemer as a member.
    InviteAccepted,
    /// A pending invite was revoked before acceptance.
    InviteRevoked,
}

impl OrgAuditAction {
    /// The stored wire name for this action.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OrgAuditAction::MemberAdded => "member.added",
            OrgAuditAction::RoleChanged => "member.role_changed",
            OrgAuditAction::MemberRemoved => "member.removed",
            OrgAuditAction::InviteCreated => "invite.created",
            OrgAuditAction::InviteAccepted => "invite.accepted",
            OrgAuditAction::InviteRevoked => "invite.revoked",
        }
    }

    /// Parse the stored wire name. An unrecognized value is a data-integrity
    /// fault (the store is the only writer).
    fn parse(text: &str) -> Result<Self, AuthError> {
        match text {
            "member.added" => Ok(OrgAuditAction::MemberAdded),
            "member.role_changed" => Ok(OrgAuditAction::RoleChanged),
            "member.removed" => Ok(OrgAuditAction::MemberRemoved),
            "invite.created" => Ok(OrgAuditAction::InviteCreated),
            "invite.accepted" => Ok(OrgAuditAction::InviteAccepted),
            "invite.revoked" => Ok(OrgAuditAction::InviteRevoked),
            _ => Err(AuthError::Backend),
        }
    }
}

/// One append-only org-console audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgAuditEntry {
    /// Opaque, monotonic id (used only for ordering, most-recent-first).
    pub id: i64,
    /// When the action was recorded, Unix seconds.
    pub recorded_at: i64,
    /// The user id that performed the action (UUID text).
    pub actor_id: String,
    /// What happened.
    pub action: OrgAuditAction,
    /// The member the action targeted (UUID text), if any.
    pub target_id: Option<String>,
    /// A short human-readable detail (e.g. `"member -> admin"`).
    pub detail: Option<String>,
}

/// The failure modes of an audited org-console membership mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrgMutationError {
    /// The target user is not a member of the workspace.
    NotFound,
    /// The actor's role does not permit the action, checked against both the
    /// actor's and the target's *current* roles read under the mutation's own
    /// transaction and row locks.
    Forbidden(String),
    /// The mutation would leave the workspace with no Owner.
    LastOwner,
    /// (add only) the target user is already a member.
    AlreadyMember,
    /// A backend/data-integrity fault.
    Backend,
}

impl From<AuthError> for OrgMutationError {
    fn from(_: AuthError) -> Self {
        OrgMutationError::Backend
    }
}

/// Authorize a membership mutation by the actor's role against the target's
/// current role and (for add/change) the requested new role. Mirrors the
/// in-memory `authorize_membership_mutation` (andypalmi/amicu-planning#117); the
/// caller has read `actor`/`target_current` under the mutation's own transaction
/// and row locks, so both are fresh.
fn authorize_membership_mutation(
    actor: Option<Role>,
    target_current: Option<Role>,
    new_role: Option<Role>,
) -> Result<(), OrgMutationError> {
    let Some(actor) = actor else {
        return Err(OrgMutationError::Forbidden(
            "you are not a member of this organization".to_owned(),
        ));
    };
    if !actor.can_manage_members() {
        return Err(OrgMutationError::Forbidden(
            "your role may not manage organization membership".to_owned(),
        ));
    }
    if actor == Role::Admin {
        if new_role.is_some_and(Role::is_privileged) {
            return Err(OrgMutationError::Forbidden(
                "only an Owner may grant the Owner or Admin role".to_owned(),
            ));
        }
        if target_current.is_some_and(Role::is_privileged) {
            return Err(OrgMutationError::Forbidden(
                "only an Owner may modify or remove an Owner or Admin".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Postgres implementation of the membership store.
pub struct PostgresMembershipStore {
    pool: PgPool,
}

// Several methods below hold both an id in its `&str` form (`actor_id`,
// `target_id`) and its parsed `Uuid` form (`actor_uid`, `target_uid`) at once;
// `clippy::similar_names` (pedantic) reads those as confusable, but they are two
// representations of the SAME id and keeping the shared stem is the clearer
// naming here.
#[allow(clippy::similar_names)]
impl PostgresMembershipStore {
    /// A membership store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Add `user_id` to `workspace` at `role`. `invited_by` is the acting user
    /// (`None` for a founding Owner). Errors if the user is already a member.
    pub async fn add(
        &self,
        workspace: WorkspaceId,
        user_id: &str,
        role: Role,
        invited_by: Option<&str>,
    ) -> Result<(), AuthError> {
        let uid = parse_uid(user_id)?;
        let inviter = invited_by.map(parse_uid).transpose()?;
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        let result = sqlx::query!(
            "INSERT INTO workspace_members \
             (workspace_id, user_id, role, invited_by, joined_at) \
             VALUES ($1, $2, $3, $4, $5)",
            workspace.as_uuid(),
            uid,
            role.as_str(),
            inviter,
            now_unix_seconds(),
        )
        .execute(&mut *tx)
        .await;
        match result {
            Ok(_) => {
                tx.commit().await.map_err(map_db)?;
                Ok(())
            }
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(AuthError::UserExists)
            }
            Err(other) => Err(map_db(other)),
        }
    }

    /// The membership of `user_id` in `workspace`, if any.
    pub async fn get(
        &self,
        workspace: WorkspaceId,
        user_id: &str,
    ) -> Result<Option<Member>, AuthError> {
        let uid = parse_uid(user_id)?;
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        let row = sqlx::query!(
            "SELECT user_id, role, invited_by, joined_at FROM workspace_members \
             WHERE workspace_id = $1 AND user_id = $2",
            workspace.as_uuid(),
            uid,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        row.map(|r| {
            Ok(Member {
                workspace_id: workspace,
                user_id: r.user_id.to_string(),
                role: Role::parse(&r.role)?,
                invited_by: r.invited_by.map(|u| u.to_string()),
                joined_at: r.joined_at,
            })
        })
        .transpose()
    }

    /// The role of `user_id` in `workspace`, if they are a member. This is the
    /// membership check the claim-bound gate performs before trusting a `wsid`.
    pub async fn role_of(
        &self,
        workspace: WorkspaceId,
        user_id: &str,
    ) -> Result<Option<Role>, AuthError> {
        Ok(self.get(workspace, user_id).await?.map(|m| m.role))
    }

    /// Change `user_id`'s role in `workspace`. A no-op if they are not a member.
    pub async fn set_role(
        &self,
        workspace: WorkspaceId,
        user_id: &str,
        role: Role,
    ) -> Result<(), AuthError> {
        let uid = parse_uid(user_id)?;
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        sqlx::query!(
            "UPDATE workspace_members SET role = $3 \
             WHERE workspace_id = $1 AND user_id = $2",
            workspace.as_uuid(),
            uid,
            role.as_str(),
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    /// Remove `user_id` from `workspace`. A no-op if they are not a member.
    pub async fn remove(&self, workspace: WorkspaceId, user_id: &str) -> Result<(), AuthError> {
        let uid = parse_uid(user_id)?;
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        sqlx::query!(
            "DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
            workspace.as_uuid(),
            uid,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    /// Every member of `workspace`, oldest first.
    pub async fn list_members(&self, workspace: WorkspaceId) -> Result<Vec<Member>, AuthError> {
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        let rows = sqlx::query!(
            "SELECT user_id, role, invited_by, joined_at FROM workspace_members \
             WHERE workspace_id = $1 ORDER BY joined_at, user_id",
            workspace.as_uuid(),
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        rows.into_iter()
            .map(|r| {
                Ok(Member {
                    workspace_id: workspace,
                    user_id: r.user_id.to_string(),
                    role: Role::parse(&r.role)?,
                    invited_by: r.invited_by.map(|u| u.to_string()),
                    joined_at: r.joined_at,
                })
            })
            .collect()
    }

    /// Every workspace `user_id` belongs to, with their role in each (oldest
    /// first). Runs under the user context, not a workspace context: this is the
    /// one cross-workspace read, and a user may always see their own
    /// memberships.
    pub async fn list_workspaces(&self, user_id: &str) -> Result<Vec<Member>, AuthError> {
        let uid = parse_uid(user_id)?;
        let mut tx = begin_scoped_user(&self.pool, uid).await?;
        let rows = sqlx::query!(
            "SELECT workspace_id, role, invited_by, joined_at FROM workspace_members \
             WHERE user_id = $1 ORDER BY joined_at, workspace_id",
            uid,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        rows.into_iter()
            .map(|r| {
                Ok(Member {
                    workspace_id: WorkspaceId::from_uuid(r.workspace_id),
                    user_id: user_id.to_string(),
                    role: Role::parse(&r.role)?,
                    invited_by: r.invited_by.map(|u| u.to_string()),
                    joined_at: r.joined_at,
                })
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Org-console audited mutations (andypalmi/amicu-planning#130).
    //
    // Each of the three mutations opens ONE `begin_scoped` transaction, locks the
    // actor's and target's membership rows `FOR UPDATE`, re-reads their *current*
    // roles under that lock, authorizes against those fresh reads, performs the
    // mutation, and appends its `org_audit` record - all before commit. The row
    // locks serialize a concurrent role change on the actor or target, so a
    // mutation is always authorized against who the actor/target are at the
    // instant it is serialized, never a stale snapshot (the #117 TOCTOU
    // closure). A cancelled task either committed both the membership change and
    // its audit record, or neither.
    // -----------------------------------------------------------------------

    /// The current roles of `actor_id` and `target_id` in `workspace`, read and
    /// locked `FOR UPDATE` (in canonical id order to avoid deadlocks) inside
    /// `tx`. A non-member (or a non-UUID id) reads as `None`.
    async fn locked_roles(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace: WorkspaceId,
        actor_id: &str,
        target_id: &str,
    ) -> Result<(Option<Role>, Option<Role>), OrgMutationError> {
        let actor_uid = Uuid::parse_str(actor_id).ok();
        let target_uid = Uuid::parse_str(target_id).ok();
        let mut ids: Vec<Uuid> = actor_uid.into_iter().chain(target_uid).collect();
        ids.sort_unstable();
        ids.dedup();
        let rows = sqlx::query!(
            "SELECT user_id, role FROM workspace_members \
             WHERE workspace_id = $1 AND user_id = ANY($2) \
             ORDER BY user_id FOR UPDATE",
            workspace.as_uuid(),
            &ids[..],
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(|_| OrgMutationError::Backend)?;
        let role_of = |want: Option<Uuid>| -> Result<Option<Role>, OrgMutationError> {
            let Some(want) = want else { return Ok(None) };
            rows.iter()
                .find(|r| r.user_id == want)
                .map(|r| Role::parse(&r.role).map_err(|_| OrgMutationError::Backend))
                .transpose()
        };
        Ok((role_of(actor_uid)?, role_of(target_uid)?))
    }

    /// The number of Owners currently in `workspace`, read inside `tx`.
    async fn owner_count(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace: WorkspaceId,
    ) -> Result<i64, OrgMutationError> {
        let row = sqlx::query!(
            "SELECT COUNT(*) AS \"count!\" FROM workspace_members \
             WHERE workspace_id = $1 AND role = 'owner'",
            workspace.as_uuid(),
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| OrgMutationError::Backend)?;
        Ok(row.count)
    }

    /// Serialize ALL audited membership mutations on `workspace` by taking a
    /// transaction-scoped advisory lock keyed on the workspace id. This MUST be
    /// the first statement in each audited-mutation transaction, before any
    /// role/owner reads, so that concurrent mutations on the same workspace run
    /// their read-modify-write one at a time - mirroring the in-memory store's
    /// single-mutex semantics.
    ///
    /// Why it is needed (andypalmi/amicu-planning#130): the last-Owner guard
    /// reads an UNLOCKED `owner_count`. When actor == target (self-demotion or
    /// self-removal) only that one row is locked `FOR UPDATE`, so two Owners
    /// self-mutating concurrently hold DISJOINT row locks; under READ COMMITTED
    /// both read `owner_count == 2`, both pass the `== 1` guard, and both commit
    /// -> 0 Owners. Acquiring the SAME advisory lock first on every mutation
    /// removes that overlap-free window, so `owner_count` can no longer be read
    /// stale.
    ///
    /// Deadlock-free: the advisory lock is always taken before the row
    /// `FOR UPDATE` locks, and it is the same key for every mutation on the
    /// workspace, so there is no lock-ordering cycle. A hash collision between
    /// two different workspaces only causes rare unnecessary serialization,
    /// never a correctness problem.
    async fn lock_workspace_mutations(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace: WorkspaceId,
    ) -> Result<(), OrgMutationError> {
        sqlx::query!(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))::text AS lock",
            workspace.as_uuid().to_string(),
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| OrgMutationError::Backend)?;
        Ok(())
    }

    /// Re-read `workspace`'s CURRENT tier inside `tx` and reject the mutation if
    /// it no longer meets Enterprise (andypalmi/amicu-planning#132): the
    /// claim-based Enterprise gate keys on the access token's `claims.tier`, so
    /// a workspace DOWNGRADED from Enterprise still passes it until the token
    /// expires (<=5 min). Re-reading here, inside the SAME transaction as the
    /// mutation, closes that staleness window for state-changing operations
    /// without an extra round-trip or waiting on token expiry.
    async fn require_current_enterprise_tier(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace: WorkspaceId,
    ) -> Result<(), OrgMutationError> {
        let tier = read_workspace_tier(tx, workspace).await?;
        if !meets_enterprise(tier) {
            return Err(OrgMutationError::Forbidden(
                "this workspace's plan no longer permits org administration".to_owned(),
            ));
        }
        Ok(())
    }

    /// Append one `org_audit` record inside `tx` (paired with its mutation).
    async fn append_audit(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace: WorkspaceId,
        actor_id: &str,
        action: OrgAuditAction,
        target_id: Option<&str>,
        detail: Option<&str>,
    ) -> Result<(), OrgMutationError> {
        let actor_uid = Uuid::parse_str(actor_id).map_err(|_| OrgMutationError::Backend)?;
        let target_uid = target_id.and_then(|t| Uuid::parse_str(t).ok());
        sqlx::query!(
            "INSERT INTO org_audit \
             (workspace_id, recorded_at, actor_id, action, target_id, detail) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            workspace.as_uuid(),
            now_unix_seconds(),
            actor_uid,
            action.as_str(),
            target_uid,
            detail,
        )
        .execute(&mut **tx)
        .await
        .map_err(|_| OrgMutationError::Backend)?;
        Ok(())
    }

    /// Add `target_id` at `role`, authorized against `actor_id`'s current role
    /// (re-read + locked), atomically with its `member.added` audit record.
    pub async fn add_member_audited(
        &self,
        workspace: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        role: Role,
    ) -> Result<Role, OrgMutationError> {
        // `target_id` originates as client input (the add-member request body),
        // but the API handler validates it as a UUID and returns 400 on a
        // malformed value before ever calling here; reaching this method with a
        // non-UUID would be an internal-contract violation, so map it to Backend.
        let target_uid = Uuid::parse_str(target_id).map_err(|_| OrgMutationError::Backend)?;
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        Self::require_current_enterprise_tier(&mut tx, workspace).await?;
        // Serialize all mutations on this workspace FIRST (before any role/owner
        // reads) so the last-Owner guard cannot race - see
        // `lock_workspace_mutations`.
        Self::lock_workspace_mutations(&mut tx, workspace).await?;
        let (actor_role, target_role) =
            Self::locked_roles(&mut tx, workspace, actor_id, target_id).await?;
        authorize_membership_mutation(actor_role, None, Some(role))?;
        if target_role.is_some() {
            return Err(OrgMutationError::AlreadyMember);
        }
        let inviter = Uuid::parse_str(actor_id).ok();
        let insert = sqlx::query!(
            "INSERT INTO workspace_members \
             (workspace_id, user_id, role, invited_by, joined_at) \
             VALUES ($1, $2, $3, $4, $5)",
            workspace.as_uuid(),
            target_uid,
            role.as_str(),
            inviter,
            now_unix_seconds(),
        )
        .execute(&mut *tx)
        .await;
        match insert {
            Ok(_) => {}
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                return Err(OrgMutationError::AlreadyMember);
            }
            Err(_) => return Err(OrgMutationError::Backend),
        }
        Self::append_audit(
            &mut tx,
            workspace,
            actor_id,
            OrgAuditAction::MemberAdded,
            Some(target_id),
            Some(&format!("added as {}", role.as_str())),
        )
        .await?;
        tx.commit().await.map_err(|_| OrgMutationError::Backend)?;
        Ok(role)
    }

    /// Change `target_id`'s role to `new_role`, authorized against both the
    /// actor's and the target's current (re-read + locked) roles, atomically with
    /// its `member.role_changed` audit record. Returns the previous role.
    pub async fn set_role_audited(
        &self,
        workspace: WorkspaceId,
        actor_id: &str,
        target_id: &str,
        new_role: Role,
    ) -> Result<Role, OrgMutationError> {
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        Self::require_current_enterprise_tier(&mut tx, workspace).await?;
        // Serialize all mutations on this workspace FIRST (before any role/owner
        // reads) so the last-Owner guard cannot race - see
        // `lock_workspace_mutations`.
        Self::lock_workspace_mutations(&mut tx, workspace).await?;
        let (actor_role, target_role) =
            Self::locked_roles(&mut tx, workspace, actor_id, target_id).await?;
        let Some(current) = target_role else {
            return Err(OrgMutationError::NotFound);
        };
        authorize_membership_mutation(actor_role, Some(current), Some(new_role))?;
        if current == Role::Owner
            && new_role != Role::Owner
            && Self::owner_count(&mut tx, workspace).await? == 1
        {
            return Err(OrgMutationError::LastOwner);
        }
        let target_uid = Uuid::parse_str(target_id).map_err(|_| OrgMutationError::Backend)?;
        sqlx::query!(
            "UPDATE workspace_members SET role = $3 \
             WHERE workspace_id = $1 AND user_id = $2",
            workspace.as_uuid(),
            target_uid,
            new_role.as_str(),
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| OrgMutationError::Backend)?;
        Self::append_audit(
            &mut tx,
            workspace,
            actor_id,
            OrgAuditAction::RoleChanged,
            Some(target_id),
            Some(&format!("{} -> {}", current.as_str(), new_role.as_str())),
        )
        .await?;
        tx.commit().await.map_err(|_| OrgMutationError::Backend)?;
        Ok(current)
    }

    /// Remove `target_id`, authorized against both the actor's and the target's
    /// current (re-read + locked) roles, atomically with its `member.removed`
    /// audit record. Returns the removed role.
    pub async fn remove_member_audited(
        &self,
        workspace: WorkspaceId,
        actor_id: &str,
        target_id: &str,
    ) -> Result<Role, OrgMutationError> {
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        Self::require_current_enterprise_tier(&mut tx, workspace).await?;
        // Serialize all mutations on this workspace FIRST (before any role/owner
        // reads) so the last-Owner guard cannot race - see
        // `lock_workspace_mutations`.
        Self::lock_workspace_mutations(&mut tx, workspace).await?;
        let (actor_role, target_role) =
            Self::locked_roles(&mut tx, workspace, actor_id, target_id).await?;
        let Some(current) = target_role else {
            return Err(OrgMutationError::NotFound);
        };
        authorize_membership_mutation(actor_role, Some(current), None)?;
        if current == Role::Owner && Self::owner_count(&mut tx, workspace).await? == 1 {
            return Err(OrgMutationError::LastOwner);
        }
        let target_uid = Uuid::parse_str(target_id).map_err(|_| OrgMutationError::Backend)?;
        sqlx::query!(
            "DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
            workspace.as_uuid(),
            target_uid,
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| OrgMutationError::Backend)?;
        Self::append_audit(
            &mut tx,
            workspace,
            actor_id,
            OrgAuditAction::MemberRemoved,
            Some(target_id),
            Some(&format!("removed ({})", current.as_str())),
        )
        .await?;
        tx.commit().await.map_err(|_| OrgMutationError::Backend)?;
        Ok(current)
    }

    /// The org-console audit log for `workspace`, most-recent first.
    pub async fn org_audit(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<OrgAuditEntry>, OrgMutationError> {
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        let rows = sqlx::query!(
            "SELECT id, recorded_at, actor_id, action, target_id, detail \
             FROM org_audit WHERE workspace_id = $1 ORDER BY id DESC",
            workspace.as_uuid(),
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| OrgMutationError::Backend)?;
        tx.commit().await.map_err(|_| OrgMutationError::Backend)?;
        rows.into_iter()
            .map(|r| {
                Ok(OrgAuditEntry {
                    id: r.id,
                    recorded_at: r.recorded_at,
                    actor_id: r.actor_id.to_string(),
                    action: OrgAuditAction::parse(&r.action)
                        .map_err(|_| OrgMutationError::Backend)?,
                    target_id: r.target_id.map(|u| u.to_string()),
                    detail: r.detail,
                })
            })
            .collect()
    }
}

/// The real, multi-workspace membership authority the token issuer consults
/// (andypalmi/amicu-planning#129): [`context`](MembershipStore::context) resolves
/// the `wsrole` + workspace `tier` for a mint/refresh, and
/// [`list_for_user`](MembershipStore::list_for_user) backs `GET /workspaces`.
/// Both resolve state SERVER-SIDE from `workspace_members` + `workspaces`;
/// neither trusts a client-supplied workspace or role.
#[async_trait]
impl MembershipStore for PostgresMembershipStore {
    async fn context(
        &self,
        workspace: WorkspaceId,
        user_id: &str,
    ) -> Result<Option<MembershipContext>, AuthError> {
        let uid = parse_uid(user_id)?;
        // Workspace context: the workspace_members RLS admits exactly this
        // workspace's rows, and the explicit predicate is the primary filter.
        // `workspaces` has no RLS, so the join reads the tier freely.
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        let row = sqlx::query!(
            "SELECT m.role, w.tier \
             FROM workspace_members m \
             JOIN workspaces w ON w.id = m.workspace_id \
             WHERE m.workspace_id = $1 AND m.user_id = $2",
            workspace.as_uuid(),
            uid,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        row.map(|r| {
            Ok::<MembershipContext, AuthError>(MembershipContext {
                role: Role::parse(&r.role)?,
                tier: tier_from_str(&r.tier)?,
            })
        })
        .transpose()
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<WorkspaceMembership>, AuthError> {
        let uid = parse_uid(user_id)?;
        // User context: the workspace_members RLS admits the caller's OWN rows
        // across workspaces (and nothing else), so a caller lists only their own
        // memberships. `workspaces` has no RLS, so the name join is free.
        let mut tx = begin_scoped_user(&self.pool, uid).await?;
        let rows = sqlx::query!(
            "SELECT m.workspace_id, w.name, m.role \
             FROM workspace_members m \
             JOIN workspaces w ON w.id = m.workspace_id \
             WHERE m.user_id = $1 \
             ORDER BY m.joined_at, m.workspace_id",
            uid,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        rows.into_iter()
            .map(|r| {
                Ok(WorkspaceMembership {
                    workspace_id: WorkspaceId::from_uuid(r.workspace_id),
                    name: r.name,
                    role: Role::parse(&r.role)?,
                })
            })
            .collect()
    }
}

/// Parse a domain user id (token `sub`) into the `user_id` UUID.
fn parse_uid(id: &str) -> Result<Uuid, AuthError> {
    Uuid::parse_str(id).map_err(|_| AuthError::Backend)
}
