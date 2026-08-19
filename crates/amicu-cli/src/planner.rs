//! Turn a graph into an `amicu_plan::Plan` plus the launch artifacts `apply`
//! needs, diffing against prior state to choose the action kind.

use amicu_core::diagnostic::{Category, Diagnostic, DiagnosticCode, Severity};
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
use crate::error::CliError;

/// One line item of a plan: the contract action plus what `apply` needs.
pub(crate) struct PlanItem {
    pub action: PlannedAction,
    /// The launch to start; `None` for destroys.
    pub launch: Option<LaunchPlan>,
    /// The deployment this item targets.
    pub deployment: DeploymentId,
    /// Rendered command text for human display, when available.
    pub rendered: Option<String>,
}

/// The full result of planning a graph.
pub(crate) struct PlanResult {
    pub plan: Plan,
    pub items: Vec<PlanItem>,
    pub diagnostics: Vec<Diagnostic>,
}

impl PlanResult {
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

/// Build a plan for `graph`, diffing against `prior` when present.
pub(crate) fn build_plan(graph: &Graph, prior: Option<&Graph>) -> Result<PlanResult, CliError> {
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

        let launch = adapter.plan_launch(d)?;
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
        region: hardware.and_then(|h| h.region.clone()),
        cost_performance: Some(cost_performance(hardware, launch)),
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
    } = s;
    vec![
        setting("runtime", "llama_cpp"),
        setting("port", port.to_string()),
        setting("context_length", context_length.to_string()),
        setting("n_gpu_layers", n_gpu_layers.to_string()),
    ]
}

fn setting(key: &str, value: impl Into<String>) -> Setting {
    Setting {
        key: key.to_owned(),
        value: value.into(),
    }
}

fn cost_performance(hardware: Option<&HardwareProfile>, launch: &LaunchPlan) -> CostPerformance {
    let cost = hardware.and_then(|h| h.price.as_ref()).map(|p| Range {
        min: p.per_hour.amount,
        max: p.per_hour.amount,
        unit: Some("usd_per_hour".to_owned()),
    });
    CostPerformance {
        cost,
        performance: None,
        assumptions: vec![format!(
            "estimated peak memory {}",
            launch.estimate.estimated_memory
        )],
        provenance: Some("heuristic".to_owned()),
        confidence: Some(amicu_plan::contract::Confidence::Low),
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
