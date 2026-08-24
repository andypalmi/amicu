//! Workspace-membership read seam the token issuer trusts.
//!
//! The active-workspace claim (`wsid`/`wsrole`/`tier`) is minted from a
//! SERVER-SIDE membership lookup, never from anything the client asserts. This
//! module defines the seam [`AuthService`](crate::service::AuthService) reads at
//! mint time (fresh sign-in, workspace switch) and re-reads on every refresh
//! rotation (so a removal or role change lands within the short access TTL).
//!
//! Two implementations back it:
//! - [`PersonalMembershipStore`] (here): derives each user's single personal
//!   workspace (where they are Owner, at the user's tier) straight from the
//!   [`UserStore`]. This is the zero-config dev/in-memory path; it knows only
//!   the personal workspace, so a switch to any other workspace is (correctly)
//!   refused.
//! - `amicu-store`'s `PostgresMembershipStore`: the real, multi-workspace
//!   authority backed by `workspace_members` + `workspaces`, with genuine
//!   invite/removal/role-change semantics.

use async_trait::async_trait;

use crate::claims::Tier;
use crate::error::Result;
use crate::tenant::{Role, WorkspaceId};
use crate::user::UserStore;

/// The server-resolved context for issuing a workspace-scoped token: the
/// subject's role in the workspace and the workspace's billing tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MembershipContext {
    /// The subject's role in the workspace (the `wsrole` claim).
    pub role: Role,
    /// The workspace's tier (the `tier` claim - per-workspace billing).
    pub tier: Tier,
}

/// One entry in "which workspaces am I in?": the workspace, its display name,
/// and the caller's role in it. Backs `GET /workspaces`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMembership {
    /// The workspace id.
    pub workspace_id: WorkspaceId,
    /// The workspace's human-facing name.
    pub name: String,
    /// The caller's role in the workspace.
    pub role: Role,
}

/// The membership authority the token issuer consults. Every method resolves
/// state SERVER-SIDE from the store of record; nothing here trusts a
/// client-supplied workspace or role.
#[async_trait]
pub trait MembershipStore: Send + Sync {
    /// The subject's role and the workspace's tier, if (and only if) the subject
    /// is a member of `workspace`. `None` means "not a member" - the caller MUST
    /// treat that as "no token for this workspace" (refuse the switch / fail the
    /// refresh). This is the single membership check every mint path funnels
    /// through.
    ///
    /// # Errors
    /// Propagates store errors.
    async fn context(
        &self,
        workspace: WorkspaceId,
        user_id: &str,
    ) -> Result<Option<MembershipContext>>;

    /// Every workspace the subject belongs to (id, name, role). Cross-workspace,
    /// but scoped to the SUBJECT's own memberships: a caller may always list
    /// their own, and only their own.
    ///
    /// # Errors
    /// Propagates store errors.
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<WorkspaceMembership>>;
}

/// Dev/in-memory membership authority derived from the [`UserStore`].
///
/// A user provisioned in-memory is Owner of exactly one workspace - their
/// [`personal_workspace_id`](crate::user::User::personal_workspace_id), at the
/// user's own tier - and belongs to no other. This adapter reflects exactly
/// that, so the zero-config server and the in-memory service tests get coherent
/// membership without a database: sign-in mints a token for the personal
/// workspace, refresh re-validates it, and a switch to any OTHER workspace is
/// correctly refused (the user is a member of none).
pub struct PersonalMembershipStore {
    users: std::sync::Arc<dyn UserStore>,
}

impl PersonalMembershipStore {
    /// A membership view over `users`: each user's single personal workspace.
    #[must_use]
    pub fn new(users: std::sync::Arc<dyn UserStore>) -> Self {
        Self { users }
    }
}

#[async_trait]
impl MembershipStore for PersonalMembershipStore {
    async fn context(
        &self,
        workspace: WorkspaceId,
        user_id: &str,
    ) -> Result<Option<MembershipContext>> {
        let Some(user) = self.users.get_by_id(user_id).await? else {
            return Ok(None);
        };
        // The only membership the in-memory model knows: Owner of one's own
        // personal workspace. Any other workspace -> not a member.
        if user.personal_workspace_id == workspace {
            Ok(Some(MembershipContext {
                role: Role::Owner,
                tier: user.tier,
            }))
        } else {
            Ok(None)
        }
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<WorkspaceMembership>> {
        let Some(user) = self.users.get_by_id(user_id).await? else {
            return Ok(Vec::new());
        };
        Ok(vec![WorkspaceMembership {
            workspace_id: user.personal_workspace_id,
            name: "Personal workspace".to_string(),
            role: Role::Owner,
        }])
    }
}
