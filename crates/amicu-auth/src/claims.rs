//! JWT claim set. Minimal by design: a user id plus the active-workspace
//! context (`wsid`/`wsrole`/`tier`) and RBAC roles, no PII or secret material.

use serde::{Deserialize, Serialize};

use crate::tenant::{Role, WorkspaceId};

/// Subscription/access tier carried as an app claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Open-core default.
    #[default]
    Community,
    /// Paid individual/team tier.
    Pro,
    /// Enterprise tier.
    Enterprise,
    /// Amicu-hosted tier.
    Hosted,
}

/// Registered + application claims for an access token.
///
/// The `kid` lives in the JWT header, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Issuer (`iss`).
    pub iss: String,
    /// Audience (`aud`) - a single service identifier.
    pub aud: String,
    /// Subject (`sub`) - the user id.
    pub sub: String,
    /// Expiry, seconds since the Unix epoch.
    pub exp: i64,
    /// Issued-at, seconds since the Unix epoch.
    pub iat: i64,
    /// Not-before, seconds since the Unix epoch.
    pub nbf: i64,
    /// The active workspace this token is scoped to. Resolved server-side from
    /// `workspace_members` at mint time and re-validated on every refresh
    /// rotation - never asserted by the client. There is no default: a token
    /// always names exactly one real workspace the subject is a member of
    /// (andypalmi/amicu-planning#129).
    pub wsid: WorkspaceId,
    /// The subject's role in [`wsid`](Claims::wsid), resolved server-side from
    /// `workspace_members` at mint time (a fresh rotation carries the current
    /// role, so a demotion lands within the access TTL).
    pub wsrole: Role,
    /// The ACTIVE WORKSPACE's tier (per-workspace billing), read from
    /// `workspaces` at mint time - not a user-global value.
    pub tier: Tier,
    /// RBAC roles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
}
