//! Tenancy primitives shared across the auth surface and the persistence layer:
//! the [`WorkspaceId`] isolation boundary and the membership [`Role`].
//!
//! These live here (not in `amicu-store`) so the always-compiled [`Claims`]
//! (`crate::claims`) can carry `wsid`/`wsrole` without `amicu-auth` depending on
//! `amicu-store` - the dependency runs the other way. `amicu-store` re-exports
//! both types, so existing `amicu_store::WorkspaceId` /
//! `amicu_store::membership::Role` paths keep resolving.
//!
//! [`Claims`]: crate::claims::Claims

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AuthError;

/// The control-plane isolation boundary: a workspace (unified with the
/// Enterprise org). Every persisted resource is scoped to a workspace, so a
/// solo self-host deployment is simply a workspace-of-one.
///
/// Serializes transparently as the underlying UUID string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(Uuid);

impl WorkspaceId {
    /// The well-known workspace for single-tenant self-host: the nil UUID.
    ///
    /// A self-host deployment that has not provisioned distinct workspaces
    /// operates entirely within this one, so it is the sensible default.
    ///
    /// NB (andypalmi/amicu-planning#129): this is NOT a valid active-workspace
    /// claim. No token is ever minted with `wsid = DEFAULT`; it survives only
    /// for the not-yet-reworked self-host config/state stores (#116).
    pub const DEFAULT: Self = Self(Uuid::nil());

    /// Creates a fresh, random workspace id (UUID v4).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wraps an existing UUID as a workspace id.
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// The well-known single-tenant workspace (the nil UUID). Equivalent to
    /// [`WorkspaceId::DEFAULT`].
    #[must_use]
    pub const fn nil() -> Self {
        Self::DEFAULT
    }

    /// Returns the underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for WorkspaceId {
    /// The single-tenant self-host workspace (the nil UUID).
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for WorkspaceId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::from_str(s).map(Self)
    }
}

impl From<Uuid> for WorkspaceId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

impl From<WorkspaceId> for Uuid {
    fn from(id: WorkspaceId) -> Self {
        id.0
    }
}

/// A workspace role, in descending privilege. The authority for WHO may act
/// WHERE, as WHAT: the `workspace_members` join stores it, and it rides the
/// access token as the `wsrole` claim (resolved server-side at mint time, never
/// asserted by the client).
///
/// Serializes as its lowercase text form (`owner`/`admin`/`member`/`viewer`),
/// matching the stored representation so a role round-trips through the claim
/// and the database identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    ///
    /// # Errors
    /// [`AuthError::Backend`] on an unrecognized role string.
    pub fn parse(text: &str) -> Result<Self, AuthError> {
        match text {
            "owner" => Ok(Role::Owner),
            "admin" => Ok(Role::Admin),
            "member" => Ok(Role::Member),
            "viewer" => Ok(Role::Viewer),
            _ => Err(AuthError::Backend),
        }
    }

    /// Parse a case-insensitive, surrounding-whitespace-tolerant wire role name
    /// (as accepted on the org-console membership API), returning `None` for an
    /// unrecognized value. Unlike [`Role::parse`] this never errors and admits
    /// mixed case (`"Owner"`, `" admin "`).
    #[must_use]
    pub fn parse_wire(text: &str) -> Option<Self> {
        Self::parse(text.trim().to_ascii_lowercase().as_str()).ok()
    }

    /// Whether this role is privileged (Owner or Admin). Privileged members may
    /// only be created, modified, or removed by an Owner. The authority for the
    /// org console's RBAC (both the in-memory store and the Postgres membership
    /// store re-check against this, so the rule lives in exactly one place).
    #[must_use]
    pub fn is_privileged(self) -> bool {
        matches!(self, Role::Owner | Role::Admin)
    }

    /// Whether this role may manage membership at all (invite / remove / change
    /// role). Owner and Admin may; Member and Viewer may not.
    #[must_use]
    pub fn can_manage_members(self) -> bool {
        matches!(self, Role::Owner | Role::Admin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_default_is_nil() {
        assert_eq!(WorkspaceId::default(), WorkspaceId::DEFAULT);
        assert_eq!(WorkspaceId::nil(), WorkspaceId::DEFAULT);
        assert_eq!(WorkspaceId::DEFAULT.as_uuid(), Uuid::nil());
    }

    #[test]
    fn workspace_id_text_round_trips() {
        let id = WorkspaceId::new();
        let text = id.to_string();
        let parsed: WorkspaceId = text.parse().expect("valid uuid text round-trips");
        assert_eq!(id, parsed);
    }

    #[test]
    fn workspace_id_serializes_transparently() {
        let id = WorkspaceId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{id}\""));
        let back: WorkspaceId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn role_text_round_trips() {
        for role in [Role::Owner, Role::Admin, Role::Member, Role::Viewer] {
            assert_eq!(Role::parse(role.as_str()).unwrap(), role);
        }
        assert!(Role::parse("root").is_err());
    }

    #[test]
    fn role_serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&Role::Owner).unwrap(), "\"owner\"");
        let back: Role = serde_json::from_str("\"viewer\"").unwrap();
        assert_eq!(back, Role::Viewer);
    }
}
