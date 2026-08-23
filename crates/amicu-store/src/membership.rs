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
use sqlx::PgPool;
use uuid::Uuid;

use crate::{WorkspaceId, begin_scoped, begin_scoped_user, map_db, now_unix_seconds};

/// A workspace role, in descending privilege. Stored as lowercase text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Full control, including membership and billing.
    Owner,
    /// Manages membership below Owner.
    Admin,
    /// A regular member.
    Member,
    /// Read-only.
    Viewer,
}

impl Role {
    /// The stored lowercase text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
            Role::Viewer => "viewer",
        }
    }

    /// Parse the stored text form. An unrecognized value is a data-integrity
    /// fault (the store is the only writer), surfaced as [`AuthError::Backend`].
    pub fn parse(text: &str) -> Result<Self, AuthError> {
        match text {
            "owner" => Ok(Role::Owner),
            "admin" => Ok(Role::Admin),
            "member" => Ok(Role::Member),
            "viewer" => Ok(Role::Viewer),
            _ => Err(AuthError::Backend),
        }
    }
}

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

/// Postgres implementation of the membership store.
pub struct PostgresMembershipStore {
    pool: PgPool,
}

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
}

/// Parse a domain user id (token `sub`) into the `user_id` UUID.
fn parse_uid(id: &str) -> Result<Uuid, AuthError> {
    Uuid::parse_str(id).map_err(|_| AuthError::Backend)
}
