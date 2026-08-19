use serde::{Deserialize, Serialize};

use crate::id::{DataClassId, GovernanceRuleId};

/// A deployment/routing restriction expressed as an allow or deny rule
/// scoped to one or more data classes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernanceRule {
    pub id: GovernanceRuleId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub effect: RuleEffect,
    /// Data classes this rule governs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_classes: Vec<DataClassId>,
    /// Providers the rule permits (when `effect` is `allow`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_providers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_regions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleEffect {
    Allow,
    Deny,
}
