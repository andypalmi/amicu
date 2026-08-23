//! The canonical, versioned resource graph and its serialization.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::id::{MissingReference, ResourceKind};
use crate::resource::{
    Budget, Credential, DataClass, Deployment, GovernanceRule, HardwareProfile, Model,
    RoutingPolicy, Tenant, UseCase,
};

/// Schema version of the canonical graph, following semantic versioning.
/// See `GRAPH_VERSIONING.md` for the compatibility policy.
pub const GRAPH_SCHEMA_VERSION: &str = "1.0.0";

/// The canonical graph: every resource, grouped by kind, plus the schema
/// version it was written against. This is the source of truth that
/// serializes to canonical JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    #[serde(default = "default_schema_version")]
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<Model>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hardware_profiles: Vec<HardwareProfile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deployments: Vec<Deployment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routing_policies: Vec<RoutingPolicy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_classes: Vec<DataClass>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub governance_rules: Vec<GovernanceRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub budgets: Vec<Budget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tenants: Vec<Tenant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub use_cases: Vec<UseCase>,
    /// Named credential bundles (amicu-planning#119); see [`Credential`] for
    /// why this holds only [`crate::secret::SecretRef`] pointers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<Credential>,
}

fn default_schema_version() -> String {
    GRAPH_SCHEMA_VERSION.to_string()
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    /// An empty graph stamped with the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: GRAPH_SCHEMA_VERSION.to_string(),
            models: Vec::new(),
            hardware_profiles: Vec::new(),
            deployments: Vec::new(),
            routing_policies: Vec::new(),
            data_classes: Vec::new(),
            governance_rules: Vec::new(),
            budgets: Vec::new(),
            tenants: Vec::new(),
            use_cases: Vec::new(),
            credentials: Vec::new(),
        }
    }

    /// Returns every reference edge whose target id does not exist in the
    /// graph. This is existence checking only, not semantic validation.
    #[must_use]
    pub fn missing_references(&self) -> Vec<MissingReference> {
        let model_ids = id_set(self.models.iter().map(|m| m.id.as_str()));
        let hardware_ids = id_set(self.hardware_profiles.iter().map(|h| h.id.as_str()));
        let deployment_ids = id_set(self.deployments.iter().map(|d| d.id.as_str()));
        let data_class_ids = id_set(self.data_classes.iter().map(|d| d.id.as_str()));
        let tenant_ids = id_set(self.tenants.iter().map(|t| t.id.as_str()));

        let mut missing = Vec::new();

        for d in &self.deployments {
            check(
                &mut missing,
                &model_ids,
                ResourceKind::Deployment,
                d.id.as_str(),
                ResourceKind::Model,
                d.model.as_str(),
            );
            check(
                &mut missing,
                &hardware_ids,
                ResourceKind::Deployment,
                d.id.as_str(),
                ResourceKind::HardwareProfile,
                d.hardware_profile.as_str(),
            );
        }

        for p in &self.routing_policies {
            for target in &p.targets {
                check(
                    &mut missing,
                    &deployment_ids,
                    ResourceKind::RoutingPolicy,
                    p.id.as_str(),
                    ResourceKind::Deployment,
                    target.deployment.as_str(),
                );
            }
        }

        for r in &self.governance_rules {
            for dc in &r.data_classes {
                check(
                    &mut missing,
                    &data_class_ids,
                    ResourceKind::GovernanceRule,
                    r.id.as_str(),
                    ResourceKind::DataClass,
                    dc.as_str(),
                );
            }
        }

        for b in &self.budgets {
            if let Some(tenant) = &b.tenant {
                check(
                    &mut missing,
                    &tenant_ids,
                    ResourceKind::Budget,
                    b.id.as_str(),
                    ResourceKind::Tenant,
                    tenant.as_str(),
                );
            }
        }

        for u in &self.use_cases {
            if let Some(dc) = &u.required_data_class {
                check(
                    &mut missing,
                    &data_class_ids,
                    ResourceKind::UseCase,
                    u.id.as_str(),
                    ResourceKind::DataClass,
                    dc.as_str(),
                );
            }
        }

        missing
    }
}

fn id_set<'a>(ids: impl Iterator<Item = &'a str>) -> HashSet<&'a str> {
    ids.collect()
}

fn check(
    missing: &mut Vec<MissingReference>,
    known: &HashSet<&str>,
    from_kind: ResourceKind,
    from_id: &str,
    to_kind: ResourceKind,
    to_id: &str,
) {
    if !known.contains(to_id) {
        missing.push(MissingReference {
            from_kind,
            from_id: from_id.to_string(),
            to_kind,
            to_id: to_id.to_string(),
        });
    }
}
