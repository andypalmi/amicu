use serde::{Deserialize, Serialize};

use crate::id::{DeploymentId, RoutingPolicyId};

/// Directs traffic across deployments: aliases, weighted split,
/// health/failover, and latency/cost behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingPolicy {
    pub id: RoutingPolicyId,
    /// Public names that resolve to this policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Weighted traffic split across target deployments.
    pub targets: Vec<RoutingTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover: Option<FailoverPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optimize_for: Option<OptimizeFor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingTarget {
    pub deployment: DeploymentId,
    /// Relative weight in the traffic split.
    pub weight: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailoverPolicy {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_interval_secs: Option<u32>,
    /// Consecutive failures before a target is taken out of rotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_failures: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizeFor {
    Latency,
    Cost,
    Balanced,
}
