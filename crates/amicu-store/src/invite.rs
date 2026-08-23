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

use crate::membership::Role;
use crate::{WorkspaceId, begin_scoped, begin_scoped_invite, map_db};

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
}
