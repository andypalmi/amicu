//! Postgres-backed workspace invite store.
//!
//! An invite is a pending, tokenized offer to join a workspace at a role. The
//! server stores only the SHA-256 hash of the opaque token (never the token
//! itself); the acceptance flow that turns an invite into a membership lands in
//! andypalmi/amicu-planning#130 - this is the data layer it builds on.
//!
//! Workspace-scoped for create/accept/expire (bound via
//! [`begin_scoped`](crate::begin_scoped)); redemption
//! ([`find_by_token_hash`](PostgresInviteStore::find_by_token_hash)) runs under
//! the invite-token context ([`begin_scoped_invite`](crate::begin_scoped_invite))
//! because the redeemer does not yet know the workspace - the unguessable token
//! is the capability that reveals exactly its one invite.

use amicu_auth::Result;
use sqlx::PgPool;
use uuid::Uuid;

use crate::membership::{OrgAuditAction, Role};
use crate::{WorkspaceId, begin_scoped, begin_scoped_invite, map_db, now_unix_seconds};

/// The failure modes of the authorized invite-lifecycle operations
/// ([`create_authorized`](PostgresInviteStore::create_authorized) /
/// [`accept_and_enroll`](PostgresInviteStore::accept_and_enroll)).
///
/// Distinct from the coarse [`AuthError`](amicu_auth::AuthError) the raw CRUD
/// methods return, because these operations layer RBAC and redemption semantics
/// on top of the store: the API maps each variant to a precise HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteError {
    /// The actor's CURRENT role (re-read in the mutation's own transaction) does
    /// not permit creating this invite - e.g. they are not a member, may not
    /// manage membership, or an Admin tried to invite a privileged role.
    Forbidden(String),
    /// The presented token maps to no live invite: unknown, expired, or already
    /// accepted (kept deliberately indistinct so a redeemer learns nothing about
    /// which tokens exist).
    Invalid,
    /// (accept only) the redeeming user is already a member of the invite's
    /// workspace.
    AlreadyMember,
    /// (revoke only) no PENDING invite with that id exists in the actor's
    /// workspace - unknown, already accepted, or belonging to another workspace
    /// (kept deliberately indistinct so nothing about other tenants leaks).
    NotFound,
    /// A backend/data-integrity fault.
    Backend,
}

/// The workspace + role a redeemed invite enrolled the user into
/// (returned by [`accept_and_enroll`](PostgresInviteStore::accept_and_enroll)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedInvite {
    /// The workspace the membership was bound to - the INVITE's workspace, never
    /// anything the redeemer asserted.
    pub workspace_id: WorkspaceId,
    /// The role granted - the INVITE's role, never anything the redeemer
    /// asserted.
    pub role: Role,
}

/// Authorize creating an invite that GRANTS `granted` by an actor whose CURRENT
/// role is `actor` (re-read under the mutation's own transaction and row lock).
///
/// Mirrors the membership-mutation rule (see
/// `membership::authorize_membership_mutation`): only Owner/Admin may invite at
/// all, and only an Owner may invite a privileged (Owner/Admin) role. Consults
/// the shared [`Role::can_manage_members`] / [`Role::is_privileged`], so there is
/// one definition of the rule.
fn authorize_invite_create(
    actor: Option<Role>,
    granted: Role,
) -> std::result::Result<(), InviteError> {
    let Some(actor) = actor else {
        return Err(InviteError::Forbidden(
            "you are not a member of this workspace".to_owned(),
        ));
    };
    if !actor.can_manage_members() {
        return Err(InviteError::Forbidden(
            "your role may not invite members".to_owned(),
        ));
    }
    if actor == Role::Admin && granted.is_privileged() {
        return Err(InviteError::Forbidden(
            "only an Owner may invite an Owner or Admin".to_owned(),
        ));
    }
    Ok(())
}

/// Authorize revoking a pending invite by an actor whose CURRENT role is `actor`
/// (re-read under the mutation's own transaction and row lock). Owner/Admin only
/// via the same coarse membership-management gate that guards invite CREATE
/// ([`Role::can_manage_members`]), so a concurrently-demoted or -removed actor
/// cannot revoke on a stale role.
fn authorize_invite_revoke(actor: Option<Role>) -> std::result::Result<(), InviteError> {
    let Some(actor) = actor else {
        return Err(InviteError::Forbidden(
            "you are not a member of this workspace".to_owned(),
        ));
    };
    if !actor.can_manage_members() {
        return Err(InviteError::Forbidden(
            "your role may not revoke invites".to_owned(),
        ));
    }
    Ok(())
}

/// Append one `org_audit` row inside `tx`, paired with the invite-lifecycle
/// mutation that motivated it (create / accept / revoke), so the audit record and
/// the mutation land or roll back together. Mirrors the `org_audit` write shape
/// used by the membership store's audited mutations.
async fn append_invite_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    actor_uid: Uuid,
    action: OrgAuditAction,
    target_uid: Option<Uuid>,
    detail: &str,
) -> std::result::Result<(), InviteError> {
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
    .map_err(|_| InviteError::Backend)?;
    Ok(())
}

/// A pending or accepted invite (never carries the token itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// Server-generated invite id.
    pub id: Uuid,
    /// The inviting workspace.
    pub workspace_id: WorkspaceId,
    /// The invited email.
    pub email: String,
    /// The role the invite grants on acceptance.
    pub role: Role,
    /// Expiry, Unix seconds.
    pub expires_at: i64,
    /// When accepted, Unix seconds (`None` while pending).
    pub accepted_at: Option<i64>,
}

/// Postgres implementation of the invite store.
pub struct PostgresInviteStore {
    pool: PgPool,
}

impl PostgresInviteStore {
    /// An invite store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a pending invite to `workspace` for `email` at `role`, storing
    /// only `token_hash`. Returns the server-generated invite id.
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        email: &str,
        role: Role,
        token_hash: &[u8; 32],
        expires_at: i64,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        sqlx::query!(
            "INSERT INTO workspace_invites \
             (id, workspace_id, email, role, token_hash, expires_at, accepted_at) \
             VALUES ($1, $2, $3, $4, $5, $6, NULL)",
            id,
            workspace.as_uuid(),
            email,
            role.as_str(),
            token_hash.as_slice(),
            expires_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(id)
    }

    /// Find the single invite a presented token maps to, across workspaces. The
    /// token IS the capability (see the module docs); the caller then reads the
    /// invite's `workspace_id` to scope acceptance.
    pub async fn find_by_token_hash(&self, token_hash: &[u8; 32]) -> Result<Option<Invite>> {
        let mut tx = begin_scoped_invite(&self.pool, token_hash).await?;
        let row = sqlx::query!(
            "SELECT id, workspace_id, email, role, expires_at, accepted_at \
             FROM workspace_invites WHERE token_hash = $1",
            token_hash.as_slice(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        row.map(|r| {
            Ok(Invite {
                id: r.id,
                workspace_id: WorkspaceId::from_uuid(r.workspace_id),
                email: r.email,
                role: Role::parse(&r.role)?,
                expires_at: r.expires_at,
                accepted_at: r.accepted_at,
            })
        })
        .transpose()
    }

    /// Mark an invite accepted at `now` (idempotent: keeps the first instant).
    /// Scoped to `workspace` - the caller passes the workspace it read from the
    /// redeemed invite. A no-op if the invite id is unknown in that workspace.
    pub async fn accept(&self, workspace: WorkspaceId, invite_id: Uuid, now: i64) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        sqlx::query!(
            "UPDATE workspace_invites SET accepted_at = COALESCE(accepted_at, $3) \
             WHERE workspace_id = $1 AND id = $2",
            workspace.as_uuid(),
            invite_id,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    /// Revoke a pending invite by deleting it. A no-op if unknown in `workspace`.
    pub async fn expire(&self, workspace: WorkspaceId, invite_id: Uuid) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        sqlx::query!(
            "DELETE FROM workspace_invites WHERE workspace_id = $1 AND id = $2",
            workspace.as_uuid(),
            invite_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Authorized invite lifecycle (andypalmi/amicu-planning#131): create with an
    // in-transaction actor RBAC re-check, list the workspace's pending invites,
    // and redeem-and-enroll atomically.
    // -----------------------------------------------------------------------

    /// Create a pending invite to `workspace`, authorized against `actor_id`'s
    /// CURRENT role - re-read and locked `FOR UPDATE` inside the SAME transaction
    /// as the insert (the #130 actor re-check applied to invites): a
    /// concurrently-demoted or -removed actor cannot land an invite on a stale
    /// role. Stores only `token_hash`. Returns the server-generated invite id.
    // `actor_id`/`actor_uid` are the `&str` and parsed-`Uuid` forms of the SAME
    // id (see `membership.rs`); the shared stem is the clearer naming.
    #[allow(clippy::similar_names)]
    pub async fn create_authorized(
        &self,
        workspace: WorkspaceId,
        actor_id: &str,
        email: &str,
        role: Role,
        token_hash: &[u8; 32],
        expires_at: i64,
    ) -> std::result::Result<Uuid, InviteError> {
        let actor_uid = Uuid::parse_str(actor_id).map_err(|_| InviteError::Backend)?;
        let id = Uuid::new_v4();
        let mut tx = begin_scoped(&self.pool, workspace)
            .await
            .map_err(|_| InviteError::Backend)?;
        // Re-read the actor's CURRENT role under a row lock, in this transaction,
        // rather than trusting the caller's claim role.
        let actor_row = sqlx::query!(
            "SELECT role FROM workspace_members \
             WHERE workspace_id = $1 AND user_id = $2 FOR UPDATE",
            workspace.as_uuid(),
            actor_uid,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| InviteError::Backend)?;
        let actor_role = actor_row
            .map(|r| Role::parse(&r.role).map_err(|_| InviteError::Backend))
            .transpose()?;
        authorize_invite_create(actor_role, role)?;

        sqlx::query!(
            "INSERT INTO workspace_invites \
             (id, workspace_id, email, role, token_hash, expires_at, accepted_at) \
             VALUES ($1, $2, $3, $4, $5, $6, NULL)",
            id,
            workspace.as_uuid(),
            email,
            role.as_str(),
            token_hash.as_slice(),
            expires_at,
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| InviteError::Backend)?;
        // Audit the invite creation atomically with the invite row (same tx): a
        // rollback drops both, so an invite can never land without its record.
        // The invite has no member target yet, so `target_id` is NULL and the
        // invite id + granted role live in `detail`.
        append_invite_audit(
            &mut tx,
            workspace,
            actor_uid,
            OrgAuditAction::InviteCreated,
            None,
            &format!("invited {email} as {} (invite {id})", role.as_str()),
        )
        .await?;
        tx.commit().await.map_err(|_| InviteError::Backend)?;
        Ok(id)
    }

    /// Every PENDING (not-yet-accepted) invite for `workspace`, soonest-expiring
    /// first. Never returns token material (only the hash is stored, and it is not
    /// selected).
    pub async fn list_pending(&self, workspace: WorkspaceId) -> Result<Vec<Invite>> {
        let mut tx = begin_scoped(&self.pool, workspace).await?;
        let rows = sqlx::query!(
            "SELECT id, workspace_id, email, role, expires_at, accepted_at \
             FROM workspace_invites \
             WHERE workspace_id = $1 AND accepted_at IS NULL \
             ORDER BY expires_at, id",
            workspace.as_uuid(),
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        rows.into_iter()
            .map(|r| {
                Ok(Invite {
                    id: r.id,
                    workspace_id: WorkspaceId::from_uuid(r.workspace_id),
                    email: r.email,
                    role: Role::parse(&r.role)?,
                    expires_at: r.expires_at,
                    accepted_at: r.accepted_at,
                })
            })
            .collect()
    }

    /// Redeem `token_hash` and enroll `user_id` into the INVITE's workspace at the
    /// INVITE's role - the two bindings a redeemer can never influence.
    ///
    /// The redemption READ (which workspace does this token map to?) runs under
    /// the invite-token context. The enrolling MUTATION then runs in ONE
    /// workspace-scoped transaction that atomically (a) claims the invite - an
    /// `UPDATE ... WHERE accepted_at IS NULL AND expires_at > now` that a
    /// concurrent redeemer or an expiry can only lose - and (b) inserts the
    /// `workspace_members` row. Either both land or neither does; a lost claim or
    /// an already-accepted/expired invite is [`InviteError::Invalid`], and an
    /// already-enrolled user is [`InviteError::AlreadyMember`] (the whole
    /// transaction rolls back, leaving the invite pending).
    // `user_id`/`user_uid` are the `&str` and parsed-`Uuid` forms of the SAME id
    // (see `membership.rs`); the shared stem is the clearer naming.
    #[allow(clippy::similar_names)]
    pub async fn accept_and_enroll(
        &self,
        user_id: &str,
        token_hash: &[u8; 32],
        now: i64,
    ) -> std::result::Result<AcceptedInvite, InviteError> {
        let user_uid = Uuid::parse_str(user_id).map_err(|_| InviteError::Backend)?;
        // Phase 1 - redeem the token to learn its (single) workspace. The token IS
        // the capability; the redeemer does not assert the workspace or role.
        let invite = self
            .find_by_token_hash(token_hash)
            .await
            .map_err(|_| InviteError::Backend)?
            .ok_or(InviteError::Invalid)?;
        // Coarse pre-check (the authoritative check is the atomic claim below).
        if invite.accepted_at.is_some() || invite.expires_at <= now {
            return Err(InviteError::Invalid);
        }

        // Phase 2 - one workspace-scoped transaction: atomically claim the invite
        // and enroll the membership, both bound to the INVITE's workspace.
        let mut tx = begin_scoped(&self.pool, invite.workspace_id)
            .await
            .map_err(|_| InviteError::Backend)?;
        let claimed = sqlx::query!(
            "UPDATE workspace_invites SET accepted_at = $3 \
             WHERE id = $1 AND workspace_id = $2 \
               AND accepted_at IS NULL AND expires_at > $3 \
             RETURNING id",
            invite.id,
            invite.workspace_id.as_uuid(),
            now,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| InviteError::Backend)?;
        if claimed.is_none() {
            // Lost the race, or it expired between phase 1 and here.
            return Err(InviteError::Invalid);
        }

        let insert = sqlx::query!(
            "INSERT INTO workspace_members \
             (workspace_id, user_id, role, invited_by, joined_at) \
             VALUES ($1, $2, $3, NULL, $4)",
            invite.workspace_id.as_uuid(),
            user_uid,
            invite.role.as_str(),
            now,
        )
        .execute(&mut *tx)
        .await;
        match insert {
            Ok(_) => {}
            // Already a member: roll the whole transaction back (so the invite
            // stays pending, not falsely marked accepted).
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                return Err(InviteError::AlreadyMember);
            }
            Err(_) => return Err(InviteError::Backend),
        }
        // Audit the enrollment atomically with the membership insert (same tx,
        // bound to the INVITE's workspace so RLS admits it). The actor and the
        // target are the joining user; the granted role + invite id are recorded.
        append_invite_audit(
            &mut tx,
            invite.workspace_id,
            user_uid,
            OrgAuditAction::InviteAccepted,
            Some(user_uid),
            &format!(
                "joined as {} via invite {}",
                invite.role.as_str(),
                invite.id
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| InviteError::Backend)?;
        Ok(AcceptedInvite {
            workspace_id: invite.workspace_id,
            role: invite.role,
        })
    }

    /// Revoke a PENDING invite `invite_id` in `workspace`, authorized against
    /// `actor_id`'s CURRENT role - re-read and locked `FOR UPDATE` in the SAME
    /// transaction as the delete (the #130 actor re-check, applied to
    /// revocation): a concurrently-demoted or -removed actor cannot revoke on a
    /// stale role. Deletes the invite and appends its `invite.revoked` audit row
    /// atomically.
    ///
    /// Only a pending invite that BELONGS to `workspace` is revocable. An unknown
    /// id, an already-accepted invite, or one belonging to another workspace
    /// deletes nothing and yields [`InviteError::NotFound`] (kept indistinct so
    /// nothing about other tenants leaks); the RLS policy on `workspace_invites`
    /// is the backstop behind the always-on `workspace_id` predicate.
    // `actor_id`/`actor_uid` are the `&str` and parsed-`Uuid` forms of the SAME
    // id (see `membership.rs`); the shared stem is the clearer naming.
    #[allow(clippy::similar_names)]
    pub async fn revoke_authorized(
        &self,
        workspace: WorkspaceId,
        actor_id: &str,
        invite_id: Uuid,
    ) -> std::result::Result<(), InviteError> {
        let actor_uid = Uuid::parse_str(actor_id).map_err(|_| InviteError::Backend)?;
        let mut tx = begin_scoped(&self.pool, workspace)
            .await
            .map_err(|_| InviteError::Backend)?;
        // Re-read the actor's CURRENT role under a row lock, in this transaction,
        // rather than trusting the caller's claim role.
        let actor_row = sqlx::query!(
            "SELECT role FROM workspace_members \
             WHERE workspace_id = $1 AND user_id = $2 FOR UPDATE",
            workspace.as_uuid(),
            actor_uid,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| InviteError::Backend)?;
        let actor_role = actor_row
            .map(|r| Role::parse(&r.role).map_err(|_| InviteError::Backend))
            .transpose()?;
        authorize_invite_revoke(actor_role)?;

        // Delete only a PENDING invite in THIS workspace; RETURNING confirms it
        // existed (and yields its granted role for the audit detail). A
        // cross-workspace/unknown/already-accepted id deletes nothing.
        let deleted = sqlx::query!(
            "DELETE FROM workspace_invites \
             WHERE workspace_id = $1 AND id = $2 AND accepted_at IS NULL \
             RETURNING role",
            workspace.as_uuid(),
            invite_id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| InviteError::Backend)?;
        let Some(row) = deleted else {
            return Err(InviteError::NotFound);
        };
        let role = Role::parse(&row.role).map_err(|_| InviteError::Backend)?;

        append_invite_audit(
            &mut tx,
            workspace,
            actor_uid,
            OrgAuditAction::InviteRevoked,
            None,
            &format!("revoked invite {invite_id} (granted {})", role.as_str()),
        )
        .await?;
        tx.commit().await.map_err(|_| InviteError::Backend)?;
        Ok(())
    }
}
