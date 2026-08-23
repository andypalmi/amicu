//! JWT claim set. Minimal by design: a user id plus tier/roles, no PII or
//! secret material.

use serde::{Deserialize, Serialize};

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
    /// Access tier.
    pub tier: Tier,
    /// RBAC roles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
}
