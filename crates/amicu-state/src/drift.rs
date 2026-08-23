//! Pure structural diff between a desired [`Graph`] and a recorded snapshot.

use std::collections::BTreeMap;

use amicu_core::Graph;
use serde::{Deserialize, Serialize};

use crate::snapshot::StateSnapshot;

/// The structured difference between the desired graph and the recorded one.
///
/// Each field lists resource addresses in the canonical `kind.id` form (for
/// example `deployment.support_chat_prod`). The report is a pure computation:
/// no locks, no I/O.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DriftReport {
    /// Addresses desired but absent from the recorded snapshot.
    pub added: Vec<String>,
    /// Addresses recorded but absent from the desired graph.
    pub removed: Vec<String>,
    /// Addresses present in both whose recorded value differs from desired.
    pub changed: Vec<String>,
}

impl DriftReport {
    /// Whether the desired graph and the snapshot agree on every resource.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// Whether any drift was detected.
    #[must_use]
    pub fn has_drift(&self) -> bool {
        !self.is_empty()
    }
}

/// Compute drift between a `desired` graph and the graph recorded in `actual`.
///
/// Pure and total: it compares the two graphs' resources by address and never
/// touches the lock, history, or any backend.
#[must_use]
pub fn detect_drift(desired: &Graph, actual: &StateSnapshot) -> DriftReport {
    let desired = flatten(desired);
    let recorded = flatten(&actual.graph);

    let mut report = DriftReport::default();
    for (addr, value) in &desired {
        match recorded.get(addr) {
            None => report.added.push(addr.clone()),
            Some(recorded_value) if recorded_value != value => report.changed.push(addr.clone()),
            Some(_) => {}
        }
    }
    for addr in recorded.keys() {
        if !desired.contains_key(addr) {
            report.removed.push(addr.clone());
        }
    }
    report
}

/// Flatten a graph into an address-keyed map of canonical resource values.
///
/// The `BTreeMap` keeps every produced address list sorted and deterministic.
fn flatten(graph: &Graph) -> BTreeMap<String, serde_json::Value> {
    use amicu_core::id::{
        BudgetId, CredentialId, DataClassId, DeploymentId, GovernanceRuleId, HardwareProfileId,
        ModelId, RoutingPolicyId, TenantId, UseCaseId,
    };

    macro_rules! index {
        ($map:expr, $items:expr, $id_ty:ty) => {
            for item in &$items {
                let addr = format!("{}.{}", <$id_ty>::KIND, item.id);
                let value = serde_json::to_value(item).unwrap_or(serde_json::Value::Null);
                $map.insert(addr, value);
            }
        };
    }

    let mut map = BTreeMap::new();
    index!(map, graph.models, ModelId);
    index!(map, graph.hardware_profiles, HardwareProfileId);
    index!(map, graph.deployments, DeploymentId);
    index!(map, graph.routing_policies, RoutingPolicyId);
    index!(map, graph.data_classes, DataClassId);
    index!(map, graph.governance_rules, GovernanceRuleId);
    index!(map, graph.budgets, BudgetId);
    index!(map, graph.tenants, TenantId);
    index!(map, graph.use_cases, UseCaseId);
    // Credentials carry only `SecretRef` pointers (name/scope/version
    // metadata), never secret bytes, so indexing them into drift is safe:
    // see amicu_core::resource::Credential.
    index!(map, graph.credentials, CredentialId);

    map
}
