//! Assemble a `DiagnosticReport` from a parsed graph.

use amicu_core::diagnostic::{Diagnostic, DiagnosticReport, Severity, catalog};
use amicu_core::graph::Graph;
use amicu_core::resource::Runtime;
use amicu_llamacpp::{LlamaCppAdapter, host_supervisor};
use amicu_runtime::RuntimeAdapter;

use crate::catalog::build_catalog;

/// Run reference and per-deployment runtime checks over `graph`.
pub(crate) fn report_for(graph: &Graph) -> DiagnosticReport {
    let mut diagnostics = Vec::new();

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
