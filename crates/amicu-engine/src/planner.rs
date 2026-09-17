//! Turn a graph into an `amicu_plan::Plan` plus the launch artifacts `apply`
//! needs, diffing against prior state to choose the action kind.

use amicu_core::diagnostic::{Category, Diagnostic, DiagnosticCode, DiagnosticReport, Severity};
use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{
    Accelerator, Deployment, GgufVariant, HardwareProfile, LlamaCppSettings, Model, Quantization,
    Runtime,
};
use amicu_llamacpp::{LlamaCppAdapter, host_supervisor};
use amicu_plan::Plan;
use amicu_plan::contract::{ActionKind, Artifact, CostPerformance, PlannedAction, Range, Setting};
use amicu_registry::GgufResolver;
use amicu_runtime::{LaunchPlan, RuntimeAdapter};

use crate::catalog::build_catalog;

/// One line item of a plan: the contract action plus what `apply` needs.
pub struct PlanItem {
    pub action: PlannedAction,
    /// The launch to start; `None` for destroys.
    pub launch: Option<LaunchPlan>,
    /// The deployment this item targets.
    pub deployment: DeploymentId,
    /// Rendered command text for human display, when available.
    pub rendered: Option<String>,
}

/// The full result of planning a graph.
pub struct PlanResult {
    pub plan: Plan,
    pub items: Vec<PlanItem>,
    pub diagnostics: Vec<Diagnostic>,
}

impl PlanResult {
    /// Whether any diagnostic in the result is error-severity.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

/// Error returned by [`build_plan`] when it cannot produce a [`PlanResult`].
///
/// Carries every diagnostic collected before the failure (including the
/// failing deployment's own [`RuntimeAdapter::validate`] diagnostics), plus
/// the [`AdapterError`](amicu_core::adapter::AdapterError) that stopped
/// planning, folded into its own [`Diagnostic`] via
/// [`AdapterError::to_diagnostic`](amicu_core::adapter::AdapterError::to_diagnostic).
///
/// This exists so a caller that skips the prior [`crate::report_for`] gate
/// (amicu-planning#150 review hardening: a `CAP-002`/`CAP-003`
/// `extra_args`/`env` conflict makes
/// [`RuntimeAdapter::plan_launch`](amicu_runtime::RuntimeAdapter::plan_launch)
/// return `Err`, which used to propagate straight out of `build_plan` as a
/// bare `AdapterError` - losing every diagnostic already collected and
/// turning what should be a structured `422` into an opaque `500` for any
/// caller that relied on `build_plan` alone to gate) still gets a full
/// [`DiagnosticReport`] to act on.
#[derive(Debug, Clone)]
pub struct PlanError(pub DiagnosticReport);

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "planning failed: {} diagnostic(s)",
            self.0.diagnostics.len()
        )
    }
}

impl std::error::Error for PlanError {}

/// Build a plan for `graph`, diffing against `prior` when present.
///
/// # Errors
/// Returns [`PlanError`] carrying every diagnostic collected so far (plus the
/// triggering `AdapterError`, folded into a [`Diagnostic`]) if a runtime
/// adapter fails to render a launch plan for some deployment.
pub fn build_plan(graph: &Graph, prior: Option<&Graph>) -> Result<PlanResult, PlanError> {
    let adapter = LlamaCppAdapter::new(Box::new(build_catalog(graph)), host_supervisor());
    let mut diagnostics = Vec::new();
    let mut items = Vec::new();

    for d in &graph.deployments {
        if !matches!(d.runtime, Runtime::LlamaCpp(_)) {
            diagnostics.push(info(
                format!("deployment.{}", d.id),
                format!(
                    "runtime '{}' is not applied by the CLI (llama_cpp only); skipped",
                    runtime_label(&d.runtime)
                ),
            ));
            continue;
        }

        diagnostics.extend(adapter.validate(d));

        let kind = match prior.and_then(|p| find_deployment(p, &d.id).map(|prev| (p, prev))) {
            None => ActionKind::Create,
            // Unchanged only if the deployment and everything it references match.
            Some((pg, prev)) if resolve(pg, prev) == resolve(graph, d) => continue,
            Some(_) => ActionKind::Update,
        };

        let launch = match adapter.plan_launch(d) {
            Ok(launch) => launch,
            Err(err) => {
                // Fold the triggering error into the diagnostics already
                // collected (this deployment's `validate` output included)
                // and hand the caller the full report, rather than losing
                // that context behind a bare `AdapterError`.
                diagnostics.push(err.to_diagnostic());
                return Err(PlanError(DiagnosticReport::new(diagnostics)));
            }
        };
        let rendered = launch.rendered.as_text().to_owned();
        let action = action_for(kind, d, &launch, graph);
        items.push(PlanItem {
            action,
            launch: Some(launch),
            deployment: d.id.clone(),
            rendered: Some(rendered),
        });
    }

    // Deployments present in prior state but gone from the config are destroyed.
    if let Some(prior) = prior {
        for prev in &prior.deployments {
            if !matches!(prev.runtime, Runtime::LlamaCpp(_)) {
                continue;
            }
            if find_deployment(graph, &prev.id).is_none() {
                items.push(PlanItem {
                    action: destroy_action(&prev.id),
                    launch: None,
                    deployment: prev.id.clone(),
                    rendered: None,
                });
            }
        }
    }

    let plan = Plan {
        version: Plan::VERSION.to_owned(),
        actions: items.iter().map(|i| i.action.clone()).collect(),
        policy: None,
    };
    Ok(PlanResult {
        plan,
        items,
        diagnostics,
    })
}

fn find_deployment<'a>(graph: &'a Graph, id: &DeploymentId) -> Option<&'a Deployment> {
    graph.deployments.iter().find(|d| &d.id == id)
}

/// A deployment plus the referenced resources whose contents affect its launch.
///
/// Change detection compares this resolved closure, so editing a referenced
/// model or hardware profile (variant, quantization, size, ...) is seen as an
/// update even when the deployment block itself is byte-for-byte identical.
type Resolved<'a> = (
    &'a Deployment,
    Option<&'a Model>,
    Option<&'a HardwareProfile>,
);

fn resolve<'a>(graph: &'a Graph, d: &'a Deployment) -> Resolved<'a> {
    (
        d,
        graph.models.iter().find(|m| m.id == d.model),
        graph
            .hardware_profiles
            .iter()
            .find(|h| h.id == d.hardware_profile),
    )
}

fn destroy_action(id: &DeploymentId) -> PlannedAction {
    PlannedAction {
        kind: ActionKind::Destroy,
        address: format!("deployment.{id}"),
        artifact: None,
        runtime: Vec::new(),
        hardware: None,
        provider: None,
        region: None,
        cost_performance: None,
        required_secrets: Vec::new(),
        required_action: None,
    }
}

fn action_for(
    kind: ActionKind,
    d: &Deployment,
    launch: &LaunchPlan,
    graph: &Graph,
) -> PlannedAction {
    let model = graph.models.iter().find(|m| m.id == d.model);
    let hardware = graph
        .hardware_profiles
        .iter()
        .find(|h| h.id == d.hardware_profile);
    let variant = model.and_then(recommended_variant);

    PlannedAction {
        kind,
        address: format!("deployment.{}", d.id),
        artifact: variant.as_ref().map(variant_artifact),
        runtime: runtime_settings(d),
        hardware: hardware.map(hardware_label),
        provider: hardware.map(|h| h.provider.clone()),
        region: hardware.and_then(|h| h.region.as_ref().map(|r| r.code.clone())),
        cost_performance: Some(cost_performance(hardware, launch, &d.runtime)),
        required_secrets: launch.secret_refs.iter().map(|r| r.name.clone()).collect(),
        required_action: None,
    }
}

fn recommended_variant(model: &Model) -> Option<GgufVariant> {
    GgufResolver::from_variants(&model.variants)
        .recommended()
        .cloned()
}

fn variant_artifact(variant: &GgufVariant) -> Artifact {
    Artifact {
        name: variant.file.clone(),
        format: Some("gguf".to_owned()),
        quantization: Some(quantization_label(variant.quantization).to_owned()),
        estimated_download_size_bytes: variant.size.map(amicu_core::ByteSize::bytes),
    }
}

fn runtime_settings(d: &Deployment) -> Vec<Setting> {
    let Runtime::LlamaCpp(s) = &d.runtime else {
        return Vec::new();
    };
    let LlamaCppSettings {
        context_length,
        port,
        n_gpu_layers,
        extra_args,
        env,
    } = s;
    let mut settings = vec![
        setting("runtime", "llama_cpp"),
        setting("port", port.to_string()),
        setting("context_length", context_length.to_string()),
        setting("n_gpu_layers", n_gpu_layers.to_string()),
    ];
    if !extra_args.is_empty() {
        settings.push(setting("extra_args", extra_args.join(" ")));
    }
    if !env.is_empty() {
        // Keys only, never values: env values may carry secrets or
        // tuning data not meant for the plan display.
        settings.push(setting(
            "env",
            env.keys().cloned().collect::<Vec<_>>().join(","),
        ));
    }
    settings
}

fn setting(key: &str, value: impl Into<String>) -> Setting {
    Setting {
        key: key.to_owned(),
        value: value.into(),
    }
}

fn cost_performance(
    hardware: Option<&HardwareProfile>,
    launch: &LaunchPlan,
    runtime: &Runtime,
) -> CostPerformance {
    let cost = hardware.and_then(|h| h.price.as_ref()).map(|p| Range {
        min: p.per_hour.amount,
        max: p.per_hour.amount,
        unit: Some("usd_per_hour".to_owned()),
    });
    let mut assumptions = vec![format!(
        "estimated peak memory {}",
        launch.estimate.estimated_memory
    )];
    // extra_args is an escape hatch the estimator cannot interpret (e.g. a
    // KV-cache-dtype or speculative-decoding flag) - flag the whole estimate
    // as unverified rather than silently ignoring what raw flags might do to
    // memory/performance (amicu-planning#150). `confidence` is left alone:
    // it already reads Low for this heuristic path.
    if extra_args_present(runtime) {
        assumptions.push("unverified: raw runtime flags present".to_owned());
    }
    CostPerformance {
        cost,
        performance: None,
        assumptions,
        provenance: Some("heuristic".to_owned()),
        confidence: Some(amicu_plan::contract::Confidence::Low),
    }
}

/// Whether the deployment's runtime settings carry any raw `extra_args`.
fn extra_args_present(runtime: &Runtime) -> bool {
    match runtime {
        Runtime::LlamaCpp(s) => !s.extra_args.is_empty(),
        Runtime::Vllm(s) => !s.extra_args.is_empty(),
    }
}

fn hardware_label(hardware: &HardwareProfile) -> String {
    match &hardware.accelerator {
        Some(Accelerator { kind, model, .. }) => {
            format!("{}.{}", accelerator_token(*kind), model)
        }
        None => "cpu".to_owned(),
    }
}

fn accelerator_token(kind: amicu_core::resource::AcceleratorKind) -> &'static str {
    use amicu_core::resource::AcceleratorKind as K;
    match kind {
        K::Gpu => "gpu",
        K::Tpu => "tpu",
        K::AppleSilicon => "apple_silicon",
        K::Cpu => "cpu",
    }
}

fn quantization_label(q: Quantization) -> &'static str {
    match q {
        Quantization::F32 => "F32",
        Quantization::F16 => "F16",
        Quantization::Bf16 => "BF16",
        Quantization::Int8 => "INT8",
        Quantization::Int4 => "INT4",
        Quantization::Q4KM => "Q4_K_M",
        Quantization::Q5KM => "Q5_K_M",
        Quantization::Q6K => "Q6_K",
        Quantization::Q8_0 => "Q8_0",
    }
}

fn runtime_label(runtime: &Runtime) -> &'static str {
    match runtime {
        Runtime::LlamaCpp(_) => "llama_cpp",
        Runtime::Vllm(_) => "vllm",
    }
}

fn info(resource: String, message: String) -> Diagnostic {
    let mut d = Diagnostic::new(
        DiagnosticCode::new(Category::Cap, 1),
        Severity::Info,
        message,
    );
    d.resource = Some(resource);
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::ByteSize;
    use amicu_core::resource::{
        AcceleratorKind, Artifact, ArtifactFormat, ModelSource, SourceRegistry,
    };
    use std::collections::BTreeMap;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn model() -> Model {
        Model {
            id: "m".into(),
            source: ModelSource {
                registry: SourceRegistry::HuggingFace,
                reference: "org/tiny-1B".into(),
            },
            architecture: "llama".into(),
            modalities: Vec::new(),
            artifact: Artifact {
                format: ArtifactFormat::Gguf,
                quantization: None,
            },
            context_length: Some(8192),
            variants: vec![GgufVariant {
                quantization: Quantization::Q4KM,
                file: "m.q4_k_m.gguf".into(),
                size: Some(ByteSize::from_bytes(GIB)),
            }],
        }
    }

    fn hardware() -> HardwareProfile {
        HardwareProfile {
            id: "hw".into(),
            provider: "local".into(),
            region: None,
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::AppleSilicon,
                model: "m3_max".into(),
                count: 1,
                memory: ByteSize::from_bytes(48 * GIB),
            }),
            cpu: None,
            memory: ByteSize::from_bytes(48 * GIB),
            interconnect: None,
            price: None,
        }
    }

    fn deployment(extra_args: Vec<String>) -> Deployment {
        Deployment {
            id: "dep".into(),
            model: "m".into(),
            hardware_profile: "hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                extra_args,
                env: BTreeMap::new(),
            }),
        }
    }

    fn graph(extra_args: Vec<String>) -> Graph {
        let mut g = Graph::new();
        g.models.push(model());
        g.hardware_profiles.push(hardware());
        g.deployments.push(deployment(extra_args));
        g
    }

    const ASSUMPTION: &str = "unverified: raw runtime flags present";

    #[test]
    fn assumption_present_when_extra_args_nonempty() {
        let result = build_plan(&graph(vec!["--jinja".to_owned()]), None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(cp.assumptions.iter().any(|a| a == ASSUMPTION));
    }

    #[test]
    fn assumption_absent_when_extra_args_empty() {
        let result = build_plan(&graph(Vec::new()), None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(!cp.assumptions.iter().any(|a| a == ASSUMPTION));
    }

    #[test]
    fn extra_args_and_env_threaded_into_runtime_settings() {
        let mut g = graph(vec!["--jinja".to_owned()]);
        let Runtime::LlamaCpp(s) = &mut g.deployments[0].runtime else {
            unreachable!("graph() always builds a llama_cpp deployment");
        };
        s.env.insert("FOO".to_owned(), "secretvalue".to_owned());

        let result = build_plan(&g, None).unwrap();
        let settings = &result.items[0].action.runtime;

        let extra_args_setting = settings
            .iter()
            .find(|s| s.key == "extra_args")
            .expect("extra_args setting present");
        assert!(extra_args_setting.value.contains("--jinja"));

        let env_setting = settings
            .iter()
            .find(|s| s.key == "env")
            .expect("env setting present");
        assert!(env_setting.value.contains("FOO"));
        // Values never leak into the plan display, only key names.
        assert!(!env_setting.value.contains("secretvalue"));
    }

    // L1 regression (amicu-planning#150 review hardening): a caller that
    // calls `build_plan` directly, skipping the prior `report_for` gate every
    // existing caller (CLI `plan`/`apply`, `/plan`, `/recommend/customize`)
    // applies, must still get a structured diagnostic report back - not a
    // bare adapter error that turns into an opaque 500 one layer up.
    #[test]
    fn build_plan_called_directly_with_a_cap003_graph_returns_diagnostics_not_a_bare_error() {
        let g = graph(vec!["--host".to_owned(), "0.0.0.0".to_owned()]);

        let report = match build_plan(&g, None) {
            Ok(_) => panic!("a denylisted extra_args flag must not plan"),
            Err(PlanError(report)) => report,
        };

        assert!(!report.valid);
        assert!(
            report.diagnostics.iter().any(|d| d.code
                == amicu_core::diagnostic::catalog::CAP_EXTRA_ARG_DENYLISTED
                && d.severity == Severity::Error),
            "expected AMICU-CAP-003 in the plan error's report, got {:?}",
            report.diagnostics
        );
    }
}
