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
        temperature,
        top_p,
        top_k,
        min_p,
        repeat_penalty,
        cache_type_k,
        cache_type_v,
        batch_size,
        ubatch_size,
        parallel_slots,
        flash_attention,
        chat_template,
        chat_template_file,
        jinja,
        reasoning_format,
        speculative,
        extra_args,
        env,
        build,
    } = s;
    let mut settings = vec![
        setting("runtime", "llama_cpp"),
        setting("port", port.to_string()),
        setting("context_length", context_length.to_string()),
        setting("n_gpu_layers", n_gpu_layers.to_string()),
    ];
    if let Some(v) = temperature {
        settings.push(setting("temperature", v.to_string()));
    }
    if let Some(v) = top_p {
        settings.push(setting("top_p", v.to_string()));
    }
    if let Some(v) = top_k {
        settings.push(setting("top_k", v.to_string()));
    }
    if let Some(v) = min_p {
        settings.push(setting("min_p", v.to_string()));
    }
    if let Some(v) = repeat_penalty {
        settings.push(setting("repeat_penalty", v.to_string()));
    }
    if let Some(v) = cache_type_k {
        settings.push(setting("cache_type_k", v.to_string()));
    }
    if let Some(v) = cache_type_v {
        settings.push(setting("cache_type_v", v.to_string()));
    }
    if let Some(v) = batch_size {
        settings.push(setting("batch_size", v.to_string()));
    }
    if let Some(v) = ubatch_size {
        settings.push(setting("ubatch_size", v.to_string()));
    }
    if let Some(v) = parallel_slots {
        settings.push(setting("parallel_slots", v.to_string()));
    }
    if let Some(v) = flash_attention {
        settings.push(setting("flash_attention", v.to_string()));
    }
    if let Some(v) = chat_template {
        settings.push(setting("chat_template", v.clone()));
    }
    if let Some(v) = chat_template_file {
        settings.push(setting("chat_template_file", v.clone()));
    }
    if let Some(v) = jinja {
        settings.push(setting("jinja", v.to_string()));
    }
    if let Some(v) = reasoning_format {
        settings.push(setting("reasoning_format", v.to_string()));
    }
    if let Some(spec) = speculative {
        settings.push(setting("speculative_model", spec.model.as_str()));
        if let Some(v) = spec.n_max {
            settings.push(setting("speculative_n_max", v.to_string()));
        }
        if let Some(v) = spec.n_min {
            settings.push(setting("speculative_n_min", v.to_string()));
        }
        if let Some(v) = spec.p_min {
            settings.push(setting("speculative_p_min", v.to_string()));
        }
        if let Some(v) = spec.n_gpu_layers {
            settings.push(setting("speculative_n_gpu_layers", v.to_string()));
        }
    }
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
    if let Some(v) = build {
        settings.push(setting("build", v.clone()));
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
    if let Some(assumption) = kv_cache_assumption(runtime, launch.estimate.kv_cache) {
        assumptions.push(assumption);
    }
    if speculative_present(runtime) {
        assumptions.push(SPECULATIVE_ASSUMPTION.to_owned());
    }
    CostPerformance {
        cost,
        performance: None,
        assumptions,
        provenance: Some("heuristic".to_owned()),
        confidence: Some(amicu_plan::contract::Confidence::Low),
    }
}

/// What a `speculative {}` block does and does not contribute to the memory
/// estimate (amicu-planning#151 PR C): `amicu_registry::fit` adds the draft
/// model's weights to the fit check's weights total, but does not estimate the
/// draft model's own KV cache - that would need the draft model's layer/head
/// shape, which the registry does not generally carry.
const SPECULATIVE_ASSUMPTION: &str =
    "speculative: draft model weights included, draft KV cache not estimated";

/// Whether the deployment declares a llama.cpp speculative-decoding block.
fn speculative_present(runtime: &Runtime) -> bool {
    match runtime {
        Runtime::LlamaCpp(s) => s.speculative.is_some(),
        Runtime::Vllm(_) => false,
    }
}

/// Whether the deployment's runtime settings carry any raw `extra_args`.
fn extra_args_present(runtime: &Runtime) -> bool {
    match runtime {
        Runtime::LlamaCpp(s) => !s.extra_args.is_empty(),
        Runtime::Vllm(s) => !s.extra_args.is_empty(),
    }
}

/// A human-readable summary of the KV-cache estimate
/// (`e.g. "KV cache: 4GiB at q8_0 for 32768 ctx"`), when [`kv_cache_params`]
/// (`amicu_llamacpp::adapter`) found enough typed fields to compute one - see
/// [`amicu_registry::fit::kv_cache_bytes`]. `None` when the estimator had
/// nothing to fold in, matching `kv_bytes.is_none()`.
fn kv_cache_assumption(
    runtime: &Runtime,
    kv_bytes: Option<amicu_core::ByteSize>,
) -> Option<String> {
    let Runtime::LlamaCpp(s) = runtime else {
        return None;
    };
    let kv_bytes = kv_bytes?;
    let dtype = match (s.cache_type_k, s.cache_type_v) {
        (Some(k), Some(v)) if k == v => k.to_string(),
        (Some(k), Some(v)) => format!("k={k} v={v}"),
        (Some(k), None) => format!("k={k} v=f16"),
        (None, Some(v)) => format!("k=f16 v={v}"),
        (None, None) => "f16".to_owned(),
    };
    Some(format!(
        "KV cache: {kv_bytes} at {dtype} for {} ctx",
        s.context_length
    ))
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
                sha256: None,
            }],
            n_layers: None,
            n_kv_heads: None,
            head_dim: None,
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
                memory_bandwidth_gbps: None,
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
                ..Default::default()
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
        let result = build_plan(&graph(vec!["--verbose".to_owned()]), None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(cp.assumptions.iter().any(|a| a == ASSUMPTION));
    }

    #[test]
    fn assumption_absent_when_extra_args_empty() {
        let result = build_plan(&graph(Vec::new()), None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(!cp.assumptions.iter().any(|a| a == ASSUMPTION));
    }

    // --- amicu-planning#151: KV-cache estimator assumption string ----------

    #[test]
    fn kv_cache_assumption_present_when_shape_and_cache_type_are_set() {
        let mut g = graph(Vec::new());
        g.models[0].n_layers = Some(32);
        g.models[0].n_kv_heads = Some(8);
        g.models[0].head_dim = Some(128);
        let Runtime::LlamaCpp(s) = &mut g.deployments[0].runtime else {
            unreachable!("graph() always builds a llama_cpp deployment");
        };
        s.context_length = 32768;
        s.cache_type_k = Some(amicu_core::resource::KvCacheType::Q8_0);
        s.cache_type_v = Some(amicu_core::resource::KvCacheType::Q8_0);

        let result = build_plan(&g, None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(
            cp.assumptions
                .iter()
                .any(|a| a.starts_with("KV cache: ") && a.contains("q8_0") && a.contains("32768")),
            "expected a KV cache assumption, got {:?}",
            cp.assumptions
        );
    }

    #[test]
    fn kv_cache_assumption_absent_when_no_shape_metadata() {
        let result = build_plan(&graph(Vec::new()), None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(!cp.assumptions.iter().any(|a| a.starts_with("KV cache: ")));
    }

    #[test]
    fn new_llama_cpp_fields_threaded_into_runtime_settings() {
        let mut g = graph(Vec::new());
        let Runtime::LlamaCpp(s) = &mut g.deployments[0].runtime else {
            unreachable!("graph() always builds a llama_cpp deployment");
        };
        s.temperature = Some(0.6);
        s.flash_attention = Some(amicu_core::resource::FlashAttention::Auto);

        let result = build_plan(&g, None).unwrap();
        let settings = &result.items[0].action.runtime;
        assert_eq!(
            settings
                .iter()
                .find(|s| s.key == "temperature")
                .map(|s| s.value.as_str()),
            Some("0.6")
        );
        assert_eq!(
            settings
                .iter()
                .find(|s| s.key == "flash_attention")
                .map(|s| s.value.as_str()),
            Some("auto")
        );
    }

    #[test]
    fn extra_args_and_env_threaded_into_runtime_settings() {
        let mut g = graph(vec!["--verbose".to_owned()]);
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
        assert!(extra_args_setting.value.contains("--verbose"));

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

    // --- amicu-planning#151 PR C: speculative decoding ---------------------

    /// [`graph`] plus a `draft` model, with the deployment drafting from it.
    fn speculative_graph() -> Graph {
        let mut g = graph(Vec::new());
        let mut draft = model();
        draft.id = "draft".into();
        g.models.push(draft);
        let Runtime::LlamaCpp(s) = &mut g.deployments[0].runtime else {
            unreachable!("graph() always builds a llama_cpp deployment");
        };
        s.speculative = Some(amicu_core::resource::SpeculativeSettings {
            model: "draft".into(),
            n_max: Some(8),
            n_min: Some(2),
            p_min: Some(0.75),
            n_gpu_layers: Some(99),
        });
        g
    }

    #[test]
    fn speculative_assumption_present_when_a_draft_model_is_declared() {
        let result = build_plan(&speculative_graph(), None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(
            cp.assumptions.iter().any(|a| a == SPECULATIVE_ASSUMPTION),
            "expected the speculative assumption, got {:?}",
            cp.assumptions
        );
    }

    #[test]
    fn speculative_assumption_absent_without_a_speculative_block() {
        let result = build_plan(&graph(Vec::new()), None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(!cp.assumptions.iter().any(|a| a == SPECULATIVE_ASSUMPTION));
    }

    #[test]
    fn speculative_fields_threaded_into_runtime_settings() {
        let result = build_plan(&speculative_graph(), None).unwrap();
        let settings = &result.items[0].action.runtime;
        let value = |key: &str| {
            settings
                .iter()
                .find(|s| s.key == key)
                .map(|s| s.value.clone())
        };
        assert_eq!(value("speculative_model").as_deref(), Some("draft"));
        assert_eq!(value("speculative_n_max").as_deref(), Some("8"));
        assert_eq!(value("speculative_n_min").as_deref(), Some("2"));
        assert_eq!(value("speculative_p_min").as_deref(), Some("0.75"));
        assert_eq!(value("speculative_n_gpu_layers").as_deref(), Some("99"));
    }

    #[test]
    fn draft_model_weights_raise_the_planned_memory_estimate() {
        let baseline = build_plan(&graph(Vec::new()), None).unwrap();
        let with_draft = build_plan(&speculative_graph(), None).unwrap();
        let memory = |r: &PlanResult| {
            r.items[0]
                .action
                .cost_performance
                .as_ref()
                .unwrap()
                .assumptions
                .iter()
                .find(|a| a.starts_with("estimated peak memory "))
                .cloned()
                .expect("peak memory assumption present")
        };
        assert_ne!(
            memory(&baseline),
            memory(&with_draft),
            "the draft model's weights must move the estimate"
        );
    }
}
