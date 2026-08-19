//! Assemble a `DiagnosticReport` from a parsed graph.

use std::collections::HashSet;

use amicu_core::diagnostic::{
    Category, Diagnostic, DiagnosticCode, DiagnosticReport, Severity, catalog,
};
use amicu_core::graph::Graph;
use amicu_core::resource::{RuleEffect, Runtime};
use amicu_llamacpp::{LlamaCppAdapter, host_supervisor};
use amicu_runtime::RuntimeAdapter;

use crate::catalog::build_catalog;

/// `AMICU-SCHEMA-002` — the same id is declared in more than one config file.
const DUPLICATE_ID: DiagnosticCode = DiagnosticCode::new(Category::Schema, 2);

/// Run reference and per-deployment runtime checks over `graph`.
pub(crate) fn report_for(graph: &Graph) -> DiagnosticReport {
    let mut diagnostics = duplicate_ids(graph);
    diagnostics.extend(residency_violations(graph));

    for missing in graph.missing_references() {
        let mut d = Diagnostic::new(
            catalog::REF_UNKNOWN_RESOURCE,
            Severity::Error,
            missing.to_string(),
        );
        d.resource = Some(format!("{}.{}", missing.from_kind, missing.from_id));
        diagnostics.push(d);
    }

    let adapter = LlamaCppAdapter::new(Box::new(build_catalog(graph)), host_supervisor());
    for deployment in &graph.deployments {
        if !matches!(deployment.runtime, Runtime::LlamaCpp(_)) {
            continue;
        }
        for mut d in adapter.validate(deployment) {
            if d.resource.is_none() {
                d.resource = Some(format!("deployment.{}", deployment.id));
            }
            diagnostics.push(d);
        }
    }

    DiagnosticReport::new(diagnostics)
}

/// Reject deployments whose hardware region/provider violate the configured
/// residency/region allowlist or match a deny rule.
///
/// v1 limitation: scope is graph-wide (a union over every `data_class` and
/// `governance_rule`), not per-deployment, because no `deployment -> data_class`
/// DSL edge exists yet to scope residency precisely per data class.
fn residency_violations(graph: &Graph) -> Vec<Diagnostic> {
    let region_allow: HashSet<&str> = graph
        .data_classes
        .iter()
        .flat_map(|d| d.residency.iter())
        .chain(
            graph
                .governance_rules
                .iter()
                .filter(|r| r.effect == RuleEffect::Allow)
                .flat_map(|r| r.allowed_regions.iter()),
        )
        .map(String::as_str)
        .collect();
    let provider_allow: HashSet<&str> = graph
        .governance_rules
        .iter()
        .filter(|r| r.effect == RuleEffect::Allow)
        .flat_map(|r| r.allowed_providers.iter())
        .map(String::as_str)
        .collect();
    let region_deny: HashSet<&str> = deny_set(graph, |r| &r.allowed_regions);
    let provider_deny: HashSet<&str> = deny_set(graph, |r| &r.allowed_providers);

    let mut out = Vec::new();
    for deployment in &graph.deployments {
        let Some(hw) = graph
            .hardware_profiles
            .iter()
            .find(|h| h.id.as_str() == deployment.hardware_profile.as_str())
        else {
            continue; // missing hardware ref is reported elsewhere
        };

        if let Some(region) = hw.region.as_deref() {
            if !region_allow.is_empty() && !region_allow.contains(region) {
                out.push(policy(
                    catalog::POLICY_REGION_RESIDENCY,
                    deployment.id.as_str(),
                    format!("region '{region}' violates the residency allowlist"),
                ));
            } else if region_deny.contains(region) {
                out.push(policy(
                    catalog::POLICY_REGION_RESIDENCY,
                    deployment.id.as_str(),
                    format!("region '{region}' is denied by a governance rule"),
                ));
            }
        }

        let provider = hw.provider.as_str();
        if !provider_allow.is_empty() && !provider_allow.contains(provider) {
            out.push(policy(
                catalog::POLICY_PROVIDER_NOT_ALLOWED,
                deployment.id.as_str(),
                format!("provider '{provider}' is not in the allowed-providers list"),
            ));
        } else if provider_deny.contains(provider) {
            out.push(policy(
                catalog::POLICY_PROVIDER_NOT_ALLOWED,
                deployment.id.as_str(),
                format!("provider '{provider}' is denied by a governance rule"),
            ));
        }
    }
    out
}

/// Union a string field across all `Deny` governance rules.
fn deny_set<'a>(
    graph: &'a Graph,
    field: impl Fn(&'a amicu_core::resource::GovernanceRule) -> &'a Vec<String>,
) -> HashSet<&'a str> {
    graph
        .governance_rules
        .iter()
        .filter(|r| r.effect == RuleEffect::Deny)
        .flat_map(|r| field(r).iter())
        .map(String::as_str)
        .collect()
}

/// Build an `Error`-severity policy diagnostic addressed to a deployment.
fn policy(code: DiagnosticCode, deployment_id: &str, message: String) -> Diagnostic {
    let mut d = Diagnostic::new(code, Severity::Error, message);
    d.resource = Some(deployment_id.to_string());
    d
}

/// Flag any id declared more than once across the merged config files.
fn duplicate_ids(graph: &Graph) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    collect_dups(
        &mut out,
        "model",
        graph.models.iter().map(|m| m.id.as_str()),
    );
    collect_dups(
        &mut out,
        "hardware_profile",
        graph.hardware_profiles.iter().map(|h| h.id.as_str()),
    );
    collect_dups(
        &mut out,
        "deployment",
        graph.deployments.iter().map(|d| d.id.as_str()),
    );
    collect_dups(
        &mut out,
        "routing_policy",
        graph.routing_policies.iter().map(|p| p.id.as_str()),
    );
    collect_dups(
        &mut out,
        "data_class",
        graph.data_classes.iter().map(|d| d.id.as_str()),
    );
    collect_dups(
        &mut out,
        "governance_rule",
        graph.governance_rules.iter().map(|r| r.id.as_str()),
    );
    collect_dups(
        &mut out,
        "budget",
        graph.budgets.iter().map(|b| b.id.as_str()),
    );
    collect_dups(
        &mut out,
        "tenant",
        graph.tenants.iter().map(|t| t.id.as_str()),
    );
    collect_dups(
        &mut out,
        "use_case",
        graph.use_cases.iter().map(|u| u.id.as_str()),
    );
    out
}

/// Emit one error per id that appears more than once in `ids`.
fn collect_dups<'a>(out: &mut Vec<Diagnostic>, kind: &str, ids: impl Iterator<Item = &'a str>) {
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    for id in ids {
        if !seen.insert(id) && reported.insert(id) {
            let mut d = Diagnostic::new(
                DUPLICATE_ID,
                Severity::Error,
                format!("duplicate {kind} id '{id}' declared in more than one config file"),
            );
            d.resource = Some(format!("{kind}.{id}"));
            out.push(d);
        }
    }
}

#[cfg(test)]
mod residency_tests {
    use super::*;
    use amicu_core::resource::{
        DataClass, Deployment, GovernanceRule, HardwareProfile, LlamaCppSettings, RuleEffect,
        Runtime, Sensitivity,
    };
    use amicu_core::units::ByteSize;

    fn hw(provider: &str, region: Option<&str>) -> HardwareProfile {
        HardwareProfile {
            id: "hw".into(),
            provider: provider.to_owned(),
            region: region.map(ToOwned::to_owned),
            accelerator: None,
            cpu: None,
            memory: ByteSize::from_bytes(1024),
            interconnect: None,
            price: None,
        }
    }

    fn deployment() -> Deployment {
        Deployment {
            id: "dep".into(),
            model: "m".into(),
            hardware_profile: "hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
            }),
        }
    }

    fn graph_with(hw: HardwareProfile) -> Graph {
        let mut g = Graph::new();
        g.hardware_profiles.push(hw);
        g.deployments.push(deployment());
        g
    }

    fn codes(g: &Graph) -> Vec<String> {
        residency_violations(g)
            .iter()
            .map(|d| d.code.to_string())
            .collect()
    }

    fn data_class(residency: &[&str]) -> DataClass {
        DataClass {
            id: "dc".into(),
            sensitivity: Sensitivity::Confidential,
            residency: residency.iter().map(ToString::to_string).collect(),
            allow_external_providers: false,
            handling: Vec::new(),
        }
    }

    fn rule(effect: RuleEffect, providers: &[&str], regions: &[&str]) -> GovernanceRule {
        GovernanceRule {
            id: "gr".into(),
            description: None,
            effect,
            data_classes: Vec::new(),
            allowed_providers: providers.iter().map(ToString::to_string).collect(),
            allowed_regions: regions.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn region_outside_allowlist_is_flagged() {
        let mut g = graph_with(hw("local_apple", Some("us-east-1")));
        g.data_classes.push(data_class(&["eu-central-1"]));
        assert_eq!(codes(&g), vec!["AMICU-POLICY-002"]);
    }

    #[test]
    fn missing_region_is_not_flagged() {
        let mut g = graph_with(hw("local_apple", None));
        g.data_classes.push(data_class(&["eu-central-1"]));
        assert!(codes(&g).is_empty());
    }

    #[test]
    fn provider_outside_allowlist_is_flagged() {
        let mut g = graph_with(hw("aws", None));
        g.governance_rules
            .push(rule(RuleEffect::Allow, &["local_apple"], &[]));
        assert_eq!(codes(&g), vec!["AMICU-POLICY-003"]);
    }

    #[test]
    fn deny_rules_flag_region_and_provider() {
        let mut g = graph_with(hw("aws", Some("us-east-1")));
        g.governance_rules
            .push(rule(RuleEffect::Deny, &["aws"], &["us-east-1"]));
        let c = codes(&g);
        assert!(c.contains(&"AMICU-POLICY-002".to_owned()));
        assert!(c.contains(&"AMICU-POLICY-003".to_owned()));
    }

    #[test]
    fn compliant_stack_has_no_violations() {
        let mut g = graph_with(hw("local_apple", Some("eu-central-1")));
        g.data_classes.push(data_class(&["eu-central-1"]));
        g.governance_rules
            .push(rule(RuleEffect::Allow, &["local_apple"], &["eu-central-1"]));
        assert!(codes(&g).is_empty());
    }

    #[test]
    fn empty_allowlists_permit_everything() {
        let g = graph_with(hw("anything", Some("anywhere")));
        assert!(codes(&g).is_empty());
    }

    #[test]
    fn deployment_without_hardware_is_skipped() {
        let mut g = Graph::new();
        g.deployments.push(deployment());
        g.data_classes.push(data_class(&["eu-central-1"]));
        assert!(codes(&g).is_empty());
    }
}
