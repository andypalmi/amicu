//! Assemble a `DiagnosticReport` from a parsed graph.

use std::collections::HashSet;

use amicu_core::diagnostic::{
    Category, Diagnostic, DiagnosticCode, DiagnosticReport, Severity, catalog,
};
use amicu_core::graph::Graph;
use amicu_core::resource::Runtime;
use amicu_llamacpp::{LlamaCppAdapter, host_supervisor};
use amicu_runtime::RuntimeAdapter;

use crate::catalog::build_catalog;

/// `AMICU-SCHEMA-002` — the same id is declared in more than one config file.
const DUPLICATE_ID: DiagnosticCode = DiagnosticCode::new(Category::Schema, 2);

/// Run reference and per-deployment runtime checks over `graph`.
pub(crate) fn report_for(graph: &Graph) -> DiagnosticReport {
    let mut diagnostics = duplicate_ids(graph);

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
