use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::id::CredentialId;
use crate::secret::SecretRef;

/// A named bundle of secret-designated fields.
///
/// Every field here holds a [`SecretRef`] - a pointer to a secret in a
/// backend - never secret bytes. The HCL front end (`amicu-hcl`) enforces
/// this structurally: `secret("vault://...")` parses to a `SecretRef`, while
/// a raw string literal in `api_key`, `token`, `password`, or an `env` map
/// value is a hard parse-time rejection (`AMICU-POLICY-004`), not merely a
/// convention. This is the minimal schema surface introduced to give
/// "secret-designated field" something concrete to anchor to; the previous
/// nine resource kinds had no credential-bearing fields at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub id: CredentialId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretRef>,
    /// Named secret-valued entries, e.g. runtime environment variables that
    /// must carry credentials rather than literal values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, SecretRef>,
}
