//! Assemble a `DiagnosticReport` from a parsed graph.

use std::collections::HashSet;

use amicu_core::diagnostic::{Diagnostic, DiagnosticCode, DiagnosticReport, Severity, catalog};
use amicu_core::graph::Graph;
use amicu_core::resource::{Residency, RuleEffect, Runtime};
use amicu_llamacpp::{LlamaCppAdapter, host_supervisor};
use amicu_providers::resolve_residency;
use amicu_runtime::RuntimeAdapter;
use amicu_vllm::VllmAdapter;

use crate::catalog::build_catalog;

/// Run reference and per-deployment runtime checks over `graph`.
#[must_use]
pub fn report_for(graph: &Graph) -> DiagnosticReport {
    let mut diagnostics = duplicate_ids(graph);
    diagnostics.extend(residency_violations(graph));
    diagnostics.extend(amicu_hcl::scan_runtime_env_secrets(graph));
    diagnostics.extend(scaling_diagnostics(graph));

    for missing in graph.missing_references() {
        let mut d = Diagnostic::new(
            catalog::REF_UNKNOWN_RESOURCE,
            Severity::Error,
            missing.to_string(),
        );
        d.resource = Some(format!("{}.{}", missing.from_kind, missing.from_id));
        diagnostics.push(d);
    }

    // Dispatch to the adapter matching each deployment's own runtime family
    // (amicu-planning#152) - previously this only ever ran llama.cpp's
    // adapter, so a vllm deployment got no fit/capability validation at all.
    let catalog = build_catalog(graph);
    let llama_adapter = LlamaCppAdapter::new(Box::new(catalog.clone()), host_supervisor());
    let vllm_adapter = VllmAdapter::new(Box::new(catalog), amicu_vllm::host_supervisor());
    for deployment in &graph.deployments {
        let diags = match &deployment.runtime {
            Runtime::LlamaCpp(_) => llama_adapter.validate(deployment),
            Runtime::Vllm(_) => vllm_adapter.validate(deployment),
        };
        for mut d in diags {
            if d.resource.is_none() {
                d.resource = Some(format!("deployment.{}", deployment.id));
            }
            diagnostics.push(d);
        }
    }

    DiagnosticReport::new(diagnostics)
}

/// Hardware-profile `provider` value whose deployments honor `scaling {}`.
const SCALING_PROVIDER: &str = "modal";

/// Validate each deployment's `scaling {}` block (amicu-planning#180):
/// `AMICU-CAP-008` for invalid values, `AMICU-CAP-009` when the deployment's
/// provider has no scaling mapping yet.
fn scaling_diagnostics(graph: &Graph) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for d in &graph.deployments {
        let Some(scaling) = &d.scaling else {
            continue;
        };
        let resource = format!("deployment.{}", d.id);
        for problem in scaling.problems() {
            let mut diag = Diagnostic::new(
                catalog::CAP_SCALING_INVALID,
                Severity::Error,
                format!("deployment `{}`: invalid scaling: {problem}", d.id),
            );
            diag.resource = Some(resource.clone());
            out.push(diag);
        }
        let provider = graph
            .hardware_profiles
            .iter()
            .find(|h| h.id == d.hardware_profile)
            .map(|h| h.provider.as_str());
        if let Some(provider) = provider
            && provider != SCALING_PROVIDER
        {
            let mut diag = Diagnostic::new(
                catalog::CAP_SCALING_UNSUPPORTED_PROVIDER,
                Severity::Error,
                format!(
                    "deployment `{}`: `scaling {{}}` is not supported for provider `{provider}` yet (only `{SCALING_PROVIDER}`); remove the block",
                    d.id
                ),
            );
            diag.resource = Some(resource.clone());
            out.push(diag);
        }
    }
    out
}

/// Reject deployments whose hardware region/provider violate the configured
/// residency/region allowlist or match a deny rule.
///
/// Two distinct region checks run: a `data_class` residency check compares the
/// hardware region's *jurisdiction* (resolved from the provider catalog) against
/// the declared allowed jurisdictions; the `governance_rule`
/// allowed/denied-regions check operates on region *codes*.
///
/// v1 limitation: scope is graph-wide (a union over every `data_class` and
/// `governance_rule`), not per-deployment, because no `deployment -> data_class`
/// DSL edge exists yet to scope residency precisely per data class.
fn residency_violations(graph: &Graph) -> Vec<Diagnostic> {
    let jurisdiction_allow: HashSet<&Residency> = graph
        .data_classes
        .iter()
        .flat_map(|d| d.residency.iter())
        .collect();
    let region_allow: HashSet<&str> = graph
        .governance_rules
        .iter()
        .filter(|r| r.effect == RuleEffect::Allow)
        .flat_map(|r| r.allowed_regions.iter())
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

        let provider = hw.provider.as_str();

        if let Some(region) = hw.region.as_ref().map(|r| r.code.as_str()) {
            // data_class residency: compare the region's jurisdiction. An
            // unresolvable region (provider/region not in the catalog) cannot be
            // proven to violate a constraint, so it is left unflagged here.
            if !jurisdiction_allow.is_empty()
                && let Some(residency) = resolve_residency(provider, region)
                && !jurisdiction_allow.contains(&residency)
            {
                out.push(policy(
                    catalog::POLICY_REGION_RESIDENCY,
                    deployment.id.as_str(),
                    format!(
                        "region '{region}' ({}) violates the data residency allowlist",
                        residency.as_token()
                    ),
                ));
            }

            // governance_rule allowed/denied regions: match on region code.
            if !region_allow.is_empty() && !region_allow.contains(region) {
                out.push(policy(
                    catalog::POLICY_REGION_RESIDENCY,
                    deployment.id.as_str(),
                    format!("region '{region}' violates the allowed-regions list"),
                ));
            } else if region_deny.contains(region) {
                out.push(policy(
                    catalog::POLICY_REGION_RESIDENCY,
                    deployment.id.as_str(),
                    format!("region '{region}' is denied by a governance rule"),
                ));
            }
        }

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
    collect_dups(
        &mut out,
        "credential",
        graph.credentials.iter().map(|c| c.id.as_str()),
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
                catalog::SCHEMA_DUPLICATE_ID,
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
        DataClass, Deployment, GovernanceRule, HardwareProfile, LlamaCppSettings, Region,
        RuleEffect, Runtime, Sensitivity,
    };
    use amicu_core::units::ByteSize;

    fn hw(provider: &str, region: Option<&str>) -> HardwareProfile {
        HardwareProfile {
            id: "hw".into(),
            provider: provider.to_owned(),
            region: region.map(Region::from_code),
            accelerator: None,
            cpu: None,
            memory: ByteSize::from_bytes(1024),
            interconnect: None,
            price: None,
        }
    }

    fn deployment() -> Deployment {
        Deployment {
            scaling: None,
            id: "dep".into(),
            model: "m".into(),
            hardware_profile: "hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                ..Default::default()
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

    fn data_class(residency: &[Residency]) -> DataClass {
        DataClass {
            id: "dc".into(),
            sensitivity: Sensitivity::Confidential,
            residency: residency.to_vec(),
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
    fn region_jurisdiction_outside_allowlist_is_flagged() {
        // aws us-east-1 resolves to the US jurisdiction, outside an EU allowlist.
        let mut g = graph_with(hw("aws", Some("us-east-1")));
        g.data_classes.push(data_class(&[Residency::Eu]));
        assert_eq!(codes(&g), vec!["AMICU-POLICY-002"]);
    }

    #[test]
    fn region_jurisdiction_within_allowlist_is_ok() {
        let mut g = graph_with(hw("aws", Some("eu-central-1")));
        g.data_classes.push(data_class(&[Residency::Eu]));
        assert_eq!(codes(&g), Vec::<String>::new());
    }

    #[test]
    fn unresolvable_region_is_not_flagged() {
        // No catalog entry for provider/region, so the jurisdiction is unknown
        // and the residency constraint cannot be proven violated.
        let mut g = graph_with(hw("local_apple", Some("cupertino")));
        g.data_classes.push(data_class(&[Residency::Eu]));
        assert_eq!(codes(&g), Vec::<String>::new());
    }

    #[test]
    fn missing_region_is_not_flagged() {
        let mut g = graph_with(hw("aws", None));
        g.data_classes.push(data_class(&[Residency::Eu]));
        assert_eq!(codes(&g), Vec::<String>::new());
    }

    #[test]
    fn region_outside_governance_allowlist_is_flagged() {
        // Governance allowed_regions match on code, independent of jurisdiction.
        let mut g = graph_with(hw("aws", Some("us-east-1")));
        g.governance_rules
            .push(rule(RuleEffect::Allow, &[], &["eu-central-1"]));
        assert_eq!(codes(&g), vec!["AMICU-POLICY-002"]);
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
        let mut g = graph_with(hw("aws", Some("eu-central-1")));
        g.data_classes.push(data_class(&[Residency::Eu]));
        g.governance_rules
            .push(rule(RuleEffect::Allow, &["aws"], &["eu-central-1"]));
        assert_eq!(codes(&g), Vec::<String>::new());
    }

    #[test]
    fn empty_allowlists_permit_everything() {
        let g = graph_with(hw("anything", Some("anywhere")));
        assert_eq!(codes(&g), Vec::<String>::new());
    }

    #[test]
    fn deployment_without_hardware_is_skipped() {
        let mut g = Graph::new();
        g.deployments.push(deployment());
        g.data_classes.push(data_class(&[Residency::Eu]));
        assert_eq!(codes(&g), Vec::<String>::new());
    }
}

// --- amicu-planning#152: dispatch regression -------------------------------
//
// Before this PR, `report_for` (and `build_plan`) hardcoded `LlamaCppAdapter`
// and skipped any non-llama.cpp deployment outright (`if !matches!(runtime,
// Runtime::LlamaCpp(_)) { continue; }`), so a vLLM deployment got *no*
// fit/capability validation at all, no matter how badly it overflowed its
// hardware. This module pins the fixed behavior: a vLLM deployment now runs
// through `VllmAdapter::validate` exactly like a llama.cpp one runs through
// `LlamaCppAdapter::validate`.
#[cfg(test)]
mod vllm_dispatch_tests {
    use super::*;
    use amicu_core::resource::{
        Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Deployment, GgufVariant,
        HardwareProfile, LlamaCppSettings, Model, ModelSource, Quantization, SourceRegistry,
        VllmSettings,
    };
    use amicu_core::units::ByteSize;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn model(reference: &str) -> Model {
        Model {
            id: "m".into(),
            source: ModelSource {
                registry: SourceRegistry::HuggingFace,
                reference: reference.to_owned(),
            },
            architecture: "llama".into(),
            modalities: Vec::new(),
            artifact: Artifact {
                format: ArtifactFormat::Safetensors,
                quantization: None,
            },
            context_length: Some(8192),
            variants: Vec::new(),
            n_layers: None,
            n_kv_heads: None,
            n_kv_layers: None,
            head_dim: None,
        }
    }

    fn hw(vram_gib: u64) -> HardwareProfile {
        HardwareProfile {
            id: "hw".into(),
            provider: "on_prem".into(),
            region: None,
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::Gpu,
                model: "rtx4090".into(),
                count: 1,
                memory: ByteSize::from_bytes(vram_gib * GIB),
                memory_bandwidth_gbps: None,
            }),
            cpu: None,
            memory: ByteSize::from_bytes(64 * GIB),
            interconnect: None,
            price: None,
        }
    }

    fn vllm_deployment(settings: VllmSettings) -> Deployment {
        Deployment {
            scaling: None,
            id: "dep".into(),
            model: "m".into(),
            hardware_profile: "hw".into(),
            runtime: Runtime::Vllm(settings),
        }
    }

    /// A vLLM deployment whose weights (70B @ bf16 default, ~130 GiB) cannot
    /// possibly fit a single 24 GiB consumer GPU at any `gpu_memory_utilization`.
    /// This is the exact shape of graph that, before this PR, `report_for`
    /// produced zero diagnostics for.
    #[test]
    fn vllm_deployment_now_yields_a_fit_diagnostic() {
        let mut g = Graph::new();
        g.models.push(model("meta-llama/Llama-3.1-70B-Instruct"));
        g.hardware_profiles.push(hw(24));
        g.deployments.push(vllm_deployment(VllmSettings {
            context_length: 8192,
            port: 8000,
            tensor_parallel_size: Some(1),
            gpu_memory_utilization: Some(0.9),
            ..Default::default()
        }));

        let report = report_for(&g);
        assert!(!report.valid, "expected the report to be invalid");
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.severity == Severity::Error
                    && (d.code == catalog::MEM_VRAM_EXCEEDED
                        || d.code == catalog::MEM_ARTIFACT_OVERSIZE)),
            "expected a memory fit error, got {:?}",
            report.diagnostics
        );
    }

    #[test]
    fn vllm_deployment_that_fits_has_no_error() {
        let mut g = Graph::new();
        g.models.push(model("meta-llama/Llama-3.1-8B-Instruct"));
        g.hardware_profiles.push(hw(80));
        g.deployments.push(vllm_deployment(VllmSettings {
            context_length: 8192,
            port: 8000,
            tensor_parallel_size: Some(1),
            gpu_memory_utilization: Some(0.9),
            ..Default::default()
        }));

        let report = report_for(&g);
        assert!(
            !report
                .diagnostics
                .iter()
                .any(|d| d.severity == Severity::Error),
            "unexpected error(s): {:?}",
            report.diagnostics
        );
    }

    /// GGUF-on-vLLM: a warning, not a block (amicu-planning#152's
    /// `AMICU-CAP-006`), reached the same way through the dispatcher.
    #[test]
    fn vllm_deployment_with_gguf_artifact_warns_through_report_for() {
        let mut g = Graph::new();
        let mut m = model("meta-llama/Llama-3.1-8B-Instruct");
        m.artifact.format = ArtifactFormat::Gguf;
        g.models.push(m);
        g.hardware_profiles.push(hw(80));
        g.deployments.push(vllm_deployment(VllmSettings {
            context_length: 8192,
            port: 8000,
            tensor_parallel_size: Some(1),
            gpu_memory_utilization: Some(0.9),
            ..Default::default()
        }));

        let report = report_for(&g);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == catalog::CAP_VLLM_GGUF_EXPERIMENTAL
                    && d.severity == Severity::Warning)
        );
        assert!(report.valid, "a warning must not invalidate the report");
    }

    /// A single graph mixing both runtime families: a llama.cpp deployment
    /// that comfortably fits its GPU, and a badly oversized vLLM deployment
    /// on its own separate (small) GPU. Confirms dispatch doesn't just route
    /// each deployment to the right adapter (covered above) but also
    /// attributes each diagnostic to its own deployment's resource address -
    /// the llama.cpp deployment must come back clean even though the vLLM
    /// deployment right next to it in the same graph fails its fit check.
    #[test]
    fn mixed_runtime_graph_attributes_fit_error_to_the_right_deployment() {
        let mut g = Graph::new();

        // llama.cpp: a tiny Q4_K_M GGUF model on a generously sized GPU.
        let mut llama_model = model("org/tiny-llama");
        llama_model.id = "llama_model".into();
        llama_model.artifact = Artifact {
            format: ArtifactFormat::Gguf,
            quantization: None,
        };
        llama_model.variants = vec![GgufVariant {
            quantization: Quantization::Q4KM,
            file: "tiny.Q4_K_M.gguf".into(),
            size: Some(ByteSize::from_bytes(4 * GIB)),
            sha256: None,
        }];
        g.models.push(llama_model);
        g.hardware_profiles.push(HardwareProfile {
            id: "llama_hw".into(),
            ..hw(24)
        });
        g.deployments.push(Deployment {
            scaling: None,
            id: "llama_dep".into(),
            model: "llama_model".into(),
            hardware_profile: "llama_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8080,
                n_gpu_layers: -1,
                ..Default::default()
            }),
        });

        // vLLM: a 70B model (bf16, ~130 GiB) on a 24 GiB GPU - can't possibly
        // fit at any `gpu_memory_utilization`, same shape as
        // `vllm_deployment_now_yields_a_fit_diagnostic` above.
        g.models.push(model("meta-llama/Llama-3.1-70B-Instruct"));
        g.hardware_profiles.push(HardwareProfile {
            id: "vllm_hw".into(),
            ..hw(24)
        });
        g.deployments.push(Deployment {
            scaling: None,
            id: "vllm_dep".into(),
            model: "meta-llama/Llama-3.1-70B-Instruct".into(),
            hardware_profile: "vllm_hw".into(),
            runtime: Runtime::Vllm(VllmSettings {
                context_length: 8192,
                port: 8000,
                tensor_parallel_size: Some(1),
                gpu_memory_utilization: Some(0.9),
                ..Default::default()
            }),
        });

        let report = report_for(&g);

        let llama_errors: Vec<_> = report
            .diagnostics
            .iter()
            .filter(|d| {
                d.severity == Severity::Error
                    && d.resource.as_deref() == Some("deployment.llama_dep")
            })
            .collect();
        assert!(
            llama_errors.is_empty(),
            "llama.cpp deployment got unexpected error(s) from a neighboring \
             oversized vLLM deployment: {llama_errors:?}"
        );

        let vllm_error = report.diagnostics.iter().find(|d| {
            d.severity == Severity::Error && d.resource.as_deref() == Some("deployment.vllm_dep")
        });
        assert!(
            vllm_error.is_some(),
            "expected the oversized vLLM deployment to get its own fit error, got {:?}",
            report.diagnostics
        );
    }
}

#[cfg(test)]
mod scaling_tests {
    use super::*;
    use amicu_core::resource::{
        Deployment, HardwareProfile, LlamaCppSettings, Runtime, ScalingSettings,
    };
    use amicu_core::units::ByteSize;

    fn graph(provider: &str, scaling: Option<ScalingSettings>) -> Graph {
        let mut g = Graph::new();
        g.hardware_profiles.push(HardwareProfile {
            id: "hw".into(),
            provider: provider.to_owned(),
            region: None,
            accelerator: None,
            cpu: None,
            memory: ByteSize::from_bytes(1024),
            interconnect: None,
            price: None,
        });
        g.deployments.push(Deployment {
            scaling,
            id: "dep".into(),
            model: "m".into(),
            hardware_profile: "hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings::default()),
        });
        g
    }

    fn codes(g: &Graph) -> Vec<String> {
        scaling_diagnostics(g)
            .iter()
            .map(|d| d.code.to_string())
            .collect()
    }

    fn valid() -> ScalingSettings {
        ScalingSettings {
            min_replicas: Some(0),
            max_replicas: Some(2),
            target_concurrency: Some(8),
            scaledown_window_secs: Some(300),
        }
    }

    #[test]
    fn valid_scaling_on_modal_is_clean_and_absent_scaling_is_clean() {
        assert_eq!(codes(&graph("modal", Some(valid()))), Vec::<String>::new());
        assert_eq!(codes(&graph("local_apple", None)), Vec::<String>::new());
    }

    #[test]
    fn invalid_values_are_cap_008() {
        let bad = [
            ScalingSettings {
                max_replicas: Some(0),
                ..ScalingSettings::default()
            },
            ScalingSettings {
                target_concurrency: Some(0),
                ..ScalingSettings::default()
            },
            ScalingSettings {
                scaledown_window_secs: Some(0),
                ..ScalingSettings::default()
            },
            ScalingSettings {
                min_replicas: Some(3),
                max_replicas: Some(2),
                ..ScalingSettings::default()
            },
        ];
        for s in bad {
            assert_eq!(
                codes(&graph("modal", Some(s.clone()))),
                vec!["AMICU-CAP-008"],
                "{s:?}"
            );
        }
    }

    #[test]
    fn non_modal_provider_is_cap_009_error() {
        let g = graph("local_apple", Some(valid()));
        assert_eq!(codes(&g), vec!["AMICU-CAP-009"]);
        assert!(
            scaling_diagnostics(&g)
                .iter()
                .all(|d| d.severity == Severity::Error)
        );
        assert!(
            report_for(&g)
                .diagnostics
                .iter()
                .any(|d| d.code.to_string() == "AMICU-CAP-009")
        );
    }
}
