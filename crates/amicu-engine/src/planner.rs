//! Turn a graph into an `amicu_plan::Plan` plus the launch artifacts `apply`
//! needs, diffing against prior state to choose the action kind.

use std::collections::BTreeMap;
use std::path::Path;

use amicu_core::diagnostic::{Diagnostic, DiagnosticReport, Severity};
use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{
    Accelerator, Deployment, GgufVariant, HardwareProfile, LlamaCppSettings, Model, Quantization,
    Runtime, VllmSettings,
};
use amicu_llamacpp::{LlamaCppAdapter, host_supervisor};
use amicu_plan::Plan;
use amicu_plan::contract::{ActionKind, Artifact, CostPerformance, PlannedAction, Range, Setting};
use amicu_registry::GgufResolver;
use amicu_runtime::{LaunchPlan, RuntimeAdapter};
use amicu_vllm::VllmAdapter;

use crate::catalog::build_catalog;
use crate::fingerprint::{EnvProbe, Fingerprint, NoProbe};

/// One line item of a plan: the contract action plus what `apply` needs.
pub struct PlanItem {
    pub action: PlannedAction,
    /// The launch to start; `None` for destroys.
    pub launch: Option<LaunchPlan>,
    /// The deployment this item targets.
    pub deployment: DeploymentId,
    /// Rendered command text for human display, when available.
    pub rendered: Option<String>,
    /// The out-of-graph inputs this launch starts with; recorded in state once
    /// the start is confirmed. `None` for destroys.
    pub fingerprint: Option<Fingerprint>,
    /// Why an Update was planned although the config is unchanged (a secret
    /// pin, image or binary path changed since the last start).
    pub reason: Option<String>,
}

/// The full result of planning a graph.
pub struct PlanResult {
    pub plan: Plan,
    pub items: Vec<PlanItem>,
    pub diagnostics: Vec<Diagnostic>,
    /// The current fingerprint of every deployment in the desired graph,
    /// changed or not, so unchanged ones can have a baseline recorded.
    pub fingerprints: BTreeMap<DeploymentId, Fingerprint>,
}

/// Everything beyond the two graphs that planning needs.
pub struct PlanInputs<'a> {
    /// The directory holding `amicu.hcl`; see [`build_plan_in`].
    pub project_root: Option<&'a Path>,
    /// Fingerprints recorded when each prior deployment was last confirmed
    /// started. A deployment with none is never reported as changed by them.
    pub recorded: &'a BTreeMap<DeploymentId, Fingerprint>,
    /// Reads the environment-dependent inputs (image, binary path).
    pub probe: &'a dyn EnvProbe,
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
/// (review hardening: a `CAP-002`/`CAP-003`
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
    build_plan_in(graph, prior, None)
}

/// [`build_plan`] for a project rooted at `project_root` (the directory holding
/// `amicu.hcl`): GGUF paths in the launch are absolute,
/// `<root>/models/<model id>/<file>`, because the launchd/systemd service has
/// no useful working directory. With `None` they stay relative to the cwd.
///
/// # Errors
/// As [`build_plan`].
pub fn build_plan_in(
    graph: &Graph,
    prior: Option<&Graph>,
    project_root: Option<&Path>,
) -> Result<PlanResult, PlanError> {
    build_plan_with(
        graph,
        prior,
        &PlanInputs {
            project_root,
            recorded: &BTreeMap::new(),
            probe: &NoProbe,
        },
    )
}

/// [`build_plan_in`] that also detects changes outside the graph: a
/// deployment whose config is unchanged is still planned as an Update when
/// its recorded [`Fingerprint`] provably differs from the current one.
///
/// # Errors
/// As [`build_plan`].
pub fn build_plan_with(
    graph: &Graph,
    prior: Option<&Graph>,
    inputs: &PlanInputs<'_>,
) -> Result<PlanResult, PlanError> {
    let project_root = inputs.project_root;
    // Dispatch to the adapter matching each deployment's own runtime family -
    // previously any non-llama.cpp deployment was
    // skipped entirely (an info diagnostic, no launch, never applied).
    let catalog = build_catalog(graph);
    let mut llama_adapter = LlamaCppAdapter::new(Box::new(catalog.clone()), host_supervisor());
    if let Some(root) = project_root {
        llama_adapter = llama_adapter.with_models_dir(root.join("models"));
    }
    let vllm_adapter = VllmAdapter::new(Box::new(catalog), amicu_vllm::host_supervisor());
    let mut diagnostics = Vec::new();
    let mut items = Vec::new();
    let mut fingerprints = BTreeMap::new();

    for d in &graph.deployments {
        let adapter: &dyn RuntimeAdapter = match &d.runtime {
            Runtime::LlamaCpp(_) => &llama_adapter,
            Runtime::Vllm(_) => &vllm_adapter,
        };

        for mut diag in crate::checks::apply_memory_check(d, adapter.validate(d)) {
            if diag.resource.is_none() {
                diag.resource = Some(format!("deployment.{}", d.id));
            }
            diagnostics.push(diag);
        }

        let hardware = graph
            .hardware_profiles
            .iter()
            .find(|h| h.id == d.hardware_profile);
        let fingerprint = Fingerprint::compute(d, hardware, inputs.probe);
        fingerprints.insert(d.id.clone(), fingerprint.clone());

        let (kind, reason) =
            match prior.and_then(|p| find_deployment(p, &d.id).map(|prev| (p, prev))) {
                None => (ActionKind::Create, None),
                // Unchanged only if the deployment and everything it references
                // match, and nothing it was started with has changed outside
                // the config.
                Some((pg, prev)) if resolve(pg, prev) == resolve(graph, d) => {
                    let changes = inputs
                        .recorded
                        .get(&d.id)
                        .map(|r| fingerprint.changes_from(r))
                        .unwrap_or_default();
                    if changes.is_empty() {
                        continue;
                    }
                    (
                        ActionKind::Update,
                        Some(format!(
                            "{} changed since it was last started",
                            changes.join(", ")
                        )),
                    )
                }
                Some((pg, prev)) => (
                    ActionKind::Update,
                    describe_config_changes(resolve(pg, prev), resolve(graph, d)),
                ),
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
            fingerprint: Some(fingerprint),
            reason,
        });
    }

    // Deployments present in prior state but gone from the config are destroyed.
    if let Some(prior) = prior {
        for prev in &prior.deployments {
            if find_deployment(graph, &prev.id).is_none() {
                items.push(PlanItem {
                    action: destroy_action(&prev.id),
                    launch: None,
                    deployment: prev.id.clone(),
                    rendered: None,
                    fingerprint: None,
                    reason: None,
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
        fingerprints,
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

/// Why a deployment whose configuration changed is an update: what changed
/// (`field: old -> new`, secrets are references so only the reference name is
/// ever shown) and why that needs a restart. `None` when no field differs
/// (nothing to say beyond the update itself).
fn describe_config_changes(prev: Resolved<'_>, next: Resolved<'_>) -> Option<String> {
    use std::collections::BTreeMap;

    const SHOWN: usize = 6;

    fn flatten(prefix: &str, v: &serde_json::Value, out: &mut BTreeMap<String, String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    flatten(&key, v, out);
                }
            }
            serde_json::Value::String(s) => {
                out.insert(prefix.to_owned(), s.clone());
            }
            other => {
                out.insert(prefix.to_owned(), other.to_string());
            }
        }
    }

    fn fields<T: serde::Serialize>(label: &str, v: Option<&T>) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(v) = v.and_then(|v| serde_json::to_value(v).ok()) {
            flatten(label, &v, &mut out);
        }
        out
    }

    let collect = |r: Resolved<'_>| {
        let mut all = fields("", Some(r.0));
        all.extend(fields("model", r.1));
        all.extend(fields("hardware", r.2));
        all
    };
    let (old, new) = (collect(prev), collect(next));
    let mut keys: Vec<&String> = old.keys().chain(new.keys()).collect();
    keys.sort();
    keys.dedup();
    // Launch-relevant fields first, so the cut at `SHOWN` never hides them.
    let mut diffs: Vec<(bool, String)> = keys
        .into_iter()
        .filter_map(|k| {
            let (a, b) = (old.get(k), new.get(k));
            if a == b {
                return None;
            }
            Some((is_launch_field(k), describe_change(k, a, b)))
        })
        .collect();
    if diffs.is_empty() {
        return None;
    }
    let launch = diffs.iter().any(|(l, _)| *l);
    // Stable: launch fields first, key order otherwise preserved.
    diffs.sort_by_key(|(l, _)| !*l);
    let mut changes: Vec<String> = diffs.into_iter().map(|(_, c)| c).collect();
    if changes.len() > SHOWN {
        let more = changes.len() - SHOWN;
        changes.truncate(SHOWN);
        changes.push(format!("and {more} more"));
    }
    let joined = changes.join(", ");
    Some(if launch {
        format!(
            "{joined}. Launch settings are fixed when the service starts, so it is restarted to apply them"
        )
    } else {
        format!(
            "{joined}. The recorded configuration differs from the last start, so it is updated"
        )
    })
}

/// Whether a flattened field is baked into the launch command (so a change
/// needs a restart): everything except descriptive metadata.
fn is_launch_field(key: &str) -> bool {
    const NON_LAUNCH: [&str; 4] = ["name", "description", "labels", "tags"];
    let head = key.split('.').next().unwrap_or(key);
    !NON_LAUNCH.contains(&head)
}

/// One change line. Values under `env.*` and `extra_args` can hold literal
/// secrets, so only the key is named; other values are truncated.
fn describe_change(key: &str, old: Option<&String>, new: Option<&String>) -> String {
    const MAX: usize = 40;
    if key.starts_with("env.") {
        return format!("{key} changed");
    }
    if key == "extra_args" || key.starts_with("extra_args.") {
        return "extra_args changed".to_owned();
    }
    let short = |s: &str| -> String {
        if s.chars().count() > MAX {
            let cut: String = s.chars().take(MAX).collect();
            format!("{cut}...")
        } else {
            s.to_owned()
        }
    };
    match (old, new) {
        (Some(a), Some(b)) => format!("{key}: {} -> {}", short(a), short(b)),
        (None, Some(b)) => format!("{key}: added ({})", short(b)),
        (Some(a), None) => format!("{key}: removed (was {})", short(a)),
        (None, None) => key.to_owned(),
    }
}

/// A destroy plan item for `id` that needs no deployment in any graph (for
/// an instance known only from its stop record).
#[must_use]
pub fn destroy_item(id: &DeploymentId) -> PlanItem {
    PlanItem {
        action: destroy_action(id),
        launch: None,
        deployment: id.clone(),
        rendered: None,
        fingerprint: None,
        reason: None,
    }
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
        runtime: runtime_settings_with_scaling(d),
        hardware: hardware.map(hardware_label),
        provider: hardware.map(|h| h.provider.clone()),
        region: hardware.and_then(|h| h.region.as_ref().map(|r| r.code.clone())),
        cost_performance: Some(cost_performance(hardware, launch, &d.runtime)),
        required_secrets: launch
            .secret_refs
            .iter()
            .chain(launch.api_key.as_ref())
            .map(|r| r.name.clone())
            .collect(),
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

/// Runtime settings plus the deployment's explicitly-set `scaling {}` fields,
/// so a non-default cap is visible in the plan.
fn runtime_settings_with_scaling(d: &Deployment) -> Vec<Setting> {
    let mut settings = runtime_settings(d);
    // The endpoint API key is shown redacted, and is never resolved at plan
    // time: the plan carries the reference only.
    if d.api_key.is_some() {
        settings.push(setting("api_key", API_KEY_REDACTED));
    }
    if !d.memory_check.is_enforce() {
        settings.push(setting("memory_check", d.memory_check.as_str()));
    }
    if let Some(s) = &d.scaling {
        if let Some(v) = s.min_replicas {
            settings.push(setting("scaling_min_replicas", v.to_string()));
        }
        if let Some(v) = s.max_replicas {
            settings.push(setting("scaling_max_replicas", v.to_string()));
        }
        if let Some(v) = s.target_concurrency {
            settings.push(setting("scaling_target_concurrency", v.to_string()));
        }
        if let Some(v) = s.scaledown_window_secs {
            settings.push(setting(
                "scaling_scaledown_window",
                amicu_core::resource::ScalingSettings::format_window(v),
            ));
        }
    }
    settings
}

/// How a deployment's `api_key` appears in a plan.
const API_KEY_REDACTED: &str = "******";

fn runtime_settings(d: &Deployment) -> Vec<Setting> {
    match &d.runtime {
        Runtime::LlamaCpp(s) => llama_cpp_runtime_settings(s),
        Runtime::Vllm(s) => vllm_runtime_settings(s),
    }
}

fn llama_cpp_runtime_settings(s: &LlamaCppSettings) -> Vec<Setting> {
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
        multimodal,
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
    push_speculative_settings(&mut settings, speculative.as_ref());
    extend_with_extra_args_and_env(&mut settings, extra_args, env);
    if let Some(v) = build {
        settings.push(setting("build", v.clone()));
    }
    push_multimodal_settings(&mut settings, multimodal.as_ref());
    settings
}

/// Append the `speculative {}` block's fields as display settings
fn push_speculative_settings(
    settings: &mut Vec<Setting>,
    speculative: Option<&amicu_core::resource::SpeculativeSettings>,
) {
    let Some(spec) = speculative else {
        return;
    };
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

/// Append the `multimodal {}` block's fields as display settings
fn push_multimodal_settings(
    settings: &mut Vec<Setting>,
    multimodal: Option<&amicu_core::resource::MultimodalSettings>,
) {
    let Some(m) = multimodal else {
        return;
    };
    if let Some(v) = &m.projector {
        settings.push(setting("multimodal_projector", v.clone()));
    }
    if let Some(v) = m.offload {
        settings.push(setting("multimodal_offload", v.to_string()));
    }
}

fn vllm_runtime_settings(s: &VllmSettings) -> Vec<Setting> {
    let VllmSettings {
        context_length,
        port,
        tensor_parallel_size,
        gpu_memory_utilization,
        kv_cache_dtype,
        max_num_seqs,
        max_num_batched_tokens,
        enforce_eager,
        extra_args,
        env,
    } = s;
    let mut settings = vec![
        setting("runtime", "vllm"),
        setting("port", port.to_string()),
        setting("context_length", context_length.to_string()),
    ];
    if let Some(v) = tensor_parallel_size {
        settings.push(setting("tensor_parallel_size", v.to_string()));
    }
    if let Some(v) = gpu_memory_utilization {
        settings.push(setting("gpu_memory_utilization", v.to_string()));
    }
    if let Some(v) = kv_cache_dtype {
        settings.push(setting("kv_cache_dtype", v.to_string()));
    }
    if let Some(v) = max_num_seqs {
        settings.push(setting("max_num_seqs", v.to_string()));
    }
    if let Some(v) = max_num_batched_tokens {
        settings.push(setting("max_num_batched_tokens", v.to_string()));
    }
    if let Some(v) = enforce_eager {
        settings.push(setting("enforce_eager", v.to_string()));
    }
    extend_with_extra_args_and_env(&mut settings, extra_args, env);
    settings
}

/// Append the shared `extra_args`/`env` settings both runtime families carry
/// (the escape hatch): `extra_args` joined for display, and
/// `env` key names only - never values, which may carry secrets or tuning
/// data not meant for the plan display.
fn extend_with_extra_args_and_env(
    settings: &mut Vec<Setting>,
    extra_args: &[String],
    env: &std::collections::BTreeMap<String, String>,
) {
    if !extra_args.is_empty() {
        settings.push(setting("extra_args", extra_args.join(" ")));
    }
    if !env.is_empty() {
        settings.push(setting(
            "env",
            env.keys().cloned().collect::<Vec<_>>().join(","),
        ));
    }
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
    // vLLM has no flat overhead band to stand in for an unknown KV cache, so
    // when the model shape is unknown the figure is weights only - say so
    // rather than let it read as a full peak.
    let kv_unknown = matches!(runtime, Runtime::Vllm(_)) && launch.estimate.kv_cache.is_none();
    let mut assumptions = vec![format!(
        "estimated peak memory {}{}",
        launch.estimate.estimated_memory,
        if kv_unknown { " (weights only)" } else { "" }
    )];
    // extra_args is an escape hatch the estimator cannot interpret (e.g. a
    // KV-cache-dtype or speculative-decoding flag) - flag the whole estimate
    // as unverified rather than silently ignoring what raw flags might do to
    // memory/performance. `confidence` is left alone:
    // it already reads Low for this heuristic path.
    if extra_args_present(runtime) {
        assumptions.push("unverified: raw runtime flags present".to_owned());
    }
    // vLLM's fit estimate is deliberately narrower than llama.cpp's (no flat
    // overhead band - see `amicu_registry::fit::check_vllm_fit`'s doc
    // comment): call that out on every vLLM plan, not just when a fit
    // problem is found.
    if matches!(runtime, Runtime::Vllm(_)) {
        assumptions.push(if kv_unknown {
            "kv cache: not estimated (model shape unknown; set n_layers, n_kv_heads, head_dim on the model)"
                .to_owned()
        } else {
            "vllm fit estimate: weights + kv cache only".to_owned()
        });
    }
    // A `multimodal` block's projector weights aren't sized anywhere - no
    // GGUF header parser in-tree, and the estimator only ever sees the main
    // model's variant size (see `fit.rs`'s doc
    // comment for the same "no parser" limitation on the main weights).
    if multimodal_present(runtime) {
        assumptions.push("multimodal: projector weights not estimated".to_owned());
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
/// estimate: `amicu_registry::fit` adds the draft
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

/// Whether the deployment carries a `multimodal { ... }` block.
/// `vllm` never does - `VllmSettings` has no such
/// field (a `multimodal` block on a `vllm {}` settings block is a schema
/// error, `amicu_hcl::de`).
fn multimodal_present(runtime: &Runtime) -> bool {
    matches!(runtime, Runtime::LlamaCpp(s) if s.multimodal.is_some())
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
    let kv_bytes = kv_bytes?;
    match runtime {
        Runtime::LlamaCpp(s) => {
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
        Runtime::Vllm(s) => {
            let dtype = s
                .kv_cache_dtype
                .map_or_else(|| "auto".to_owned(), |d| d.to_string());
            Some(format!(
                "KV cache: {kv_bytes} at {dtype} for {} ctx",
                s.context_length
            ))
        }
    }
}

fn hardware_label(hardware: &HardwareProfile) -> String {
    match &hardware.accelerator {
        Some(Accelerator {
            kind, model, count, ..
        }) => {
            let base = format!("{}.{}", accelerator_token(*kind), model);
            // Single-accelerator output stays unchanged; only show a count
            // when it is above 1.
            if *count > 1 {
                format!("{base} x{count}")
            } else {
                base
            }
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
        Quantization::Fp8 => "FP8",
        Quantization::Int8 => "INT8",
        Quantization::Int4 => "INT4",
        Quantization::Q4KM => "Q4_K_M",
        Quantization::Q5KM => "Q5_K_M",
        Quantization::Q6K => "Q6_K",
        Quantization::Q8_0 => "Q8_0",
    }
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
            n_kv_layers: None,
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

    #[test]
    fn hardware_label_shows_count_only_above_one() {
        let mut hw = hardware();
        let acc = hw.accelerator.as_mut().unwrap();
        acc.kind = AcceleratorKind::Gpu;
        acc.model = "l4".into();
        assert_eq!(hardware_label(&hw), "gpu.l4");
        hw.accelerator.as_mut().unwrap().count = 2;
        assert_eq!(hardware_label(&hw), "gpu.l4 x2");
    }

    fn deployment(extra_args: Vec<String>) -> Deployment {
        Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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

    /// A llama.cpp deployment whose weights cannot fit the hardware.
    fn oversized(memory_check: amicu_core::resource::MemoryCheck) -> Graph {
        let mut g = graph(Vec::new());
        g.hardware_profiles[0].memory = ByteSize::from_bytes(GIB);
        g.hardware_profiles[0].accelerator.as_mut().unwrap().memory = ByteSize::from_bytes(GIB);
        g.deployments[0].memory_check = memory_check;
        g
    }

    #[test]
    fn memory_check_enforce_blocks_the_plan() {
        let result =
            build_plan(&oversized(amicu_core::resource::MemoryCheck::Enforce), None).unwrap();
        assert!(result.has_errors(), "{:?}", result.diagnostics);
    }

    #[test]
    fn memory_check_warn_plans_and_shows_the_bypass_in_json() {
        use amicu_core::resource::MemoryCheck;
        let result = build_plan(&oversized(MemoryCheck::Warn), None).unwrap();
        assert!(!result.has_errors(), "{:?}", result.diagnostics);
        assert!(result.diagnostics.iter().any(|d| {
            d.severity == Severity::Warning && d.message.contains("bypassed by config")
        }));
        assert_eq!(result.items.len(), 1);
        let json = serde_json::to_value(&result.plan).unwrap();
        let runtime = json["actions"][0]["runtime"].as_array().unwrap();
        assert!(
            runtime
                .iter()
                .any(|s| s["key"] == "memory_check" && s["value"] == "warn"),
            "{runtime:?}"
        );
        // Enforce (default) adds no setting.
        let fits = build_plan(&graph(Vec::new()), None).unwrap();
        assert!(
            !fits.items[0]
                .action
                .runtime
                .iter()
                .any(|s| s.key == "memory_check")
        );
    }

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

    // --- vLLM never silently drops the KV cache --------

    fn vllm_graph(shaped: bool) -> Graph {
        let mut g = graph(Vec::new());
        g.models[0].artifact.format = ArtifactFormat::Safetensors;
        g.models[0].variants.clear();
        if shaped {
            g.models[0].n_layers = Some(32);
            g.models[0].n_kv_heads = Some(8);
            g.models[0].head_dim = Some(128);
        }
        g.deployments[0].runtime = Runtime::Vllm(VllmSettings {
            context_length: 200_000,
            ..Default::default()
        });
        g
    }

    fn assumptions(g: &Graph) -> Vec<String> {
        let result = build_plan(g, None).unwrap();
        result.items[0]
            .action
            .cost_performance
            .as_ref()
            .unwrap()
            .assumptions
            .clone()
    }

    #[test]
    fn vllm_unknown_shape_says_kv_cache_not_estimated() {
        let a = assumptions(&vllm_graph(false));
        assert!(
            a.iter().any(|s| s.starts_with(
                "kv cache: not estimated (model shape unknown; set n_layers, n_kv_heads, head_dim"
            )),
            "got {a:?}"
        );
        assert!(a[0].ends_with("(weights only)"), "got {a:?}");
        assert!(!a.iter().any(|s| s.contains("weights + kv cache only")));
    }

    #[test]
    fn vllm_known_shape_includes_kv_term_and_scales_with_context() {
        let kv = |ctx: u32| {
            let mut g = vllm_graph(true);
            let Runtime::Vllm(s) = &mut g.deployments[0].runtime else {
                unreachable!()
            };
            s.context_length = ctx;
            let result = build_plan(&g, None).unwrap();
            let a = result.items[0].action.cost_performance.clone().unwrap();
            assert!(!a.assumptions[0].contains("weights only"));
            assert!(!a.assumptions.iter().any(|s| s.contains("not estimated")));
            a.assumptions
                .into_iter()
                .find(|s| s.starts_with("KV cache: "))
                .expect("KV cache line")
        };
        // Differing contexts render differently-sized KV lines.
        assert_ne!(kv(8192), kv(200_000));
        assert!(kv(200_000).contains("200000 ctx"));
    }

    // --- KV-cache estimator assumption string ----------

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

    // --- multimodal assumption string -------------

    #[test]
    fn multimodal_assumption_present_when_block_is_set() {
        let mut g = graph(Vec::new());
        let Runtime::LlamaCpp(s) = &mut g.deployments[0].runtime else {
            unreachable!("graph() always builds a llama_cpp deployment");
        };
        s.multimodal = Some(amicu_core::resource::MultimodalSettings {
            projector: Some("/models/mmproj-f16.gguf".to_owned()),
            offload: Some(true),
        });

        let result = build_plan(&g, None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(
            cp.assumptions
                .iter()
                .any(|a| a == "multimodal: projector weights not estimated"),
            "expected the multimodal assumption, got {:?}",
            cp.assumptions
        );
    }

    #[test]
    fn multimodal_assumption_absent_when_block_is_unset() {
        let result = build_plan(&graph(Vec::new()), None).unwrap();
        let cp = result.items[0].action.cost_performance.as_ref().unwrap();
        assert!(
            !cp.assumptions
                .iter()
                .any(|a| a == "multimodal: projector weights not estimated")
        );
    }

    #[test]
    fn multimodal_fields_threaded_into_runtime_settings() {
        let mut g = graph(Vec::new());
        let Runtime::LlamaCpp(s) = &mut g.deployments[0].runtime else {
            unreachable!("graph() always builds a llama_cpp deployment");
        };
        s.multimodal = Some(amicu_core::resource::MultimodalSettings {
            projector: Some("/models/mmproj-f16.gguf".to_owned()),
            offload: Some(false),
        });

        let result = build_plan(&g, None).unwrap();
        let settings = &result.items[0].action.runtime;
        assert_eq!(
            settings
                .iter()
                .find(|s| s.key == "multimodal_projector")
                .map(|s| s.value.as_str()),
            Some("/models/mmproj-f16.gguf")
        );
        assert_eq!(
            settings
                .iter()
                .find(|s| s.key == "multimodal_offload")
                .map(|s| s.value.as_str()),
            Some("false")
        );
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

    // L1 regression : a caller that
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

    // --- speculative decoding ---------------------

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

    // --- scaling {} ------------------------------------

    fn with_scaling(mut g: Graph, s: amicu_core::resource::ScalingSettings) -> Graph {
        g.deployments[0].scaling = Some(s);
        g
    }

    #[test]
    fn scaling_is_shown_in_the_plan_only_when_set() {
        let plain = build_plan(&graph(Vec::new()), None).unwrap();
        assert!(
            !plain.items[0]
                .action
                .runtime
                .iter()
                .any(|s| s.key.starts_with("scaling_"))
        );

        let g = with_scaling(
            graph(Vec::new()),
            amicu_core::resource::ScalingSettings {
                min_replicas: Some(0),
                max_replicas: Some(1),
                target_concurrency: Some(16),
                scaledown_window_secs: Some(900),
            },
        );
        let result = build_plan(&g, None).unwrap();
        let get = |k: &str| {
            result.items[0]
                .action
                .runtime
                .iter()
                .find(|s| s.key == k)
                .map(|s| s.value.clone())
        };
        assert_eq!(get("scaling_min_replicas").as_deref(), Some("0"));
        assert_eq!(get("scaling_max_replicas").as_deref(), Some("1"));
        assert_eq!(get("scaling_target_concurrency").as_deref(), Some("16"));
        assert_eq!(get("scaling_scaledown_window").as_deref(), Some("15m"));
    }

    #[test]
    fn changing_scaling_is_an_update() {
        let prior = graph(Vec::new());
        let next = with_scaling(
            graph(Vec::new()),
            amicu_core::resource::ScalingSettings {
                max_replicas: Some(1),
                ..Default::default()
            },
        );
        let result = build_plan(&next, Some(&prior)).unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].action.kind, ActionKind::Update);
        // Unchanged scaling is a no-op.
        assert!(build_plan(&next, Some(&next)).unwrap().items.is_empty());
    }

    // --- api_key ---------------------------------------

    fn with_key(mut g: Graph, name: &str) -> Graph {
        g.deployments[0].api_key = Some(amicu_core::secret::SecretRef::new(name));
        g
    }

    #[test]
    fn api_key_is_shown_redacted_and_never_resolved_at_plan_time() {
        let plain = build_plan(&graph(Vec::new()), None).unwrap();
        assert!(
            !plain.items[0]
                .action
                .runtime
                .iter()
                .any(|s| s.key == "api_key")
        );

        let result = build_plan(&with_key(graph(Vec::new()), "CHAT_API_KEY"), None).unwrap();
        let item = &result.items[0];
        let shown = item
            .action
            .runtime
            .iter()
            .find(|s| s.key == "api_key")
            .expect("api_key setting");
        assert_eq!(shown.value, "******");
        // The reference travels in the launch plan and the required secrets
        // (by name), never a value; the rendered launch has no key flag.
        let launch = item.launch.as_ref().unwrap();
        assert_eq!(
            launch.api_key,
            Some(amicu_core::secret::SecretRef::new("CHAT_API_KEY"))
        );
        assert!(
            item.action
                .required_secrets
                .contains(&"CHAT_API_KEY".to_owned())
        );
        let rendered = item.rendered.as_deref().unwrap();
        assert!(!rendered.contains("--api-key") && !rendered.contains("CHAT_API_KEY"));
        // The plan contract carries the redaction.
        assert!(format!("{:?}", result.plan).contains("******"));
    }

    #[test]
    fn changing_the_api_key_reference_is_an_update() {
        let prior = with_key(graph(Vec::new()), "OLD_KEY");
        let next = with_key(graph(Vec::new()), "NEW_KEY");
        let result = build_plan(&next, Some(&prior)).unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].action.kind, ActionKind::Update);
        assert!(build_plan(&next, Some(&next)).unwrap().items.is_empty());
    }

    #[test]
    fn an_update_from_a_config_change_says_what_changed_and_why_it_restarts() {
        let prior = graph(Vec::new());
        let mut next = graph(Vec::new());
        let Runtime::LlamaCpp(s) = &mut next.deployments[0].runtime else {
            unreachable!("graph() builds a llama_cpp deployment")
        };
        let old = s.context_length;
        s.context_length = old + 4096;
        let item = &build_plan(&next, Some(&prior)).unwrap().items[0];
        assert_eq!(item.action.kind, ActionKind::Update);
        let reason = item.reason.as_deref().expect("reason line");
        assert!(
            reason.contains(&format!("context_length: {old} -> {}", old + 4096)),
            "{reason}"
        );
        assert!(reason.contains("restarted"), "{reason}");

        // Adding an api_key names the field and shows only the reference.
        let keyed = with_key(graph(Vec::new()), "CHAT_API_KEY");
        let item = &build_plan(&keyed, Some(&prior)).unwrap().items[0];
        let reason = item.reason.as_deref().expect("reason line");
        assert!(reason.contains("api_key"), "{reason}");
    }

    #[test]
    fn the_reason_line_never_prints_env_values_or_extra_args() {
        let prior = graph(Vec::new());
        let mut next = graph(Vec::new());
        let Runtime::LlamaCpp(s) = &mut next.deployments[0].runtime else {
            unreachable!("graph() builds a llama_cpp deployment")
        };
        s.env
            .insert("HF_TOKEN".to_owned(), "hf_literal_secret_value".to_owned());
        s.extra_args = vec!["--seed".to_owned(), "sk-literal-token".to_owned()];
        let item = &build_plan(&next, Some(&prior)).unwrap().items[0];
        let reason = item.reason.as_deref().expect("reason line");
        assert!(!reason.contains("hf_literal_secret_value"), "{reason}");
        assert!(!reason.contains("sk-literal-token"), "{reason}");
        assert!(!reason.contains("--seed"), "{reason}");
        assert!(reason.contains("env.HF_TOKEN changed"), "{reason}");
        assert!(reason.contains("extra_args changed"), "{reason}");
    }

    #[test]
    fn launch_fields_sort_first_and_metadata_only_changes_do_not_claim_a_restart() {
        assert!(is_launch_field("context_length"));
        assert!(is_launch_field("env.X"));
        assert!(!is_launch_field("description"));
        assert!(!is_launch_field("labels.team"));
        let long = "x".repeat(100);
        let shown = describe_change("chat_template", Some(&long), None);
        assert!(shown.len() < 80, "{shown}");
    }

    // --- change detection beyond the graph -------------

    use crate::fingerprint::Fact;
    use amicu_core::secret::SecretRef;

    struct Probe {
        image: Fact,
        binary: Fact,
    }

    impl EnvProbe for Probe {
        fn image(&self, _: &Deployment, _: Option<&HardwareProfile>) -> Fact {
            self.image.clone()
        }
        fn binary(&self, _: &Deployment, _: Option<&HardwareProfile>) -> Fact {
            self.binary.clone()
        }
    }

    fn probe(image: Fact, binary: Fact) -> Probe {
        Probe { image, binary }
    }

    fn known(v: &str) -> Fact {
        Fact::Known(v.to_owned())
    }

    fn keyed(version: Option<&str>) -> Graph {
        let mut g = graph(Vec::new());
        let mut r = SecretRef::new("key");
        r.version = version.map(str::to_owned);
        g.deployments[0].api_key = Some(r);
        g
    }

    /// Plan `g` against itself with `recorded` taken from `then`, probed as `now`.
    fn replan(g: &Graph, then: &Probe, now: &Probe) -> PlanResult {
        let mut recorded = BTreeMap::new();
        for d in &g.deployments {
            recorded.insert(d.id.clone(), Fingerprint::compute(d, None, then));
        }
        build_plan_with(
            g,
            Some(g),
            &PlanInputs {
                project_root: None,
                recorded: &recorded,
                probe: now,
            },
        )
        .unwrap()
    }

    #[test]
    fn identical_inputs_plan_no_changes() {
        let p = || probe(known("img:1"), known("/bin/llama"));
        assert!(replan(&keyed(Some("3")), &p(), &p()).items.is_empty());
    }

    #[test]
    fn a_secret_version_change_is_an_update() {
        let p = || probe(Fact::Unknown, Fact::Unknown);
        let mut recorded = BTreeMap::new();
        let before = keyed(Some("3"));
        recorded.insert(
            before.deployments[0].id.clone(),
            Fingerprint::compute(&before.deployments[0], None, &p()),
        );
        let after = keyed(Some("4"));
        let r = build_plan_with(
            &after,
            Some(&after),
            &PlanInputs {
                project_root: None,
                recorded: &recorded,
                probe: &p(),
            },
        )
        .unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].action.kind, ActionKind::Update);
        assert!(r.items[0].reason.as_deref().unwrap().contains("secret"));
        assert!(r.items[0].fingerprint.is_some());
    }

    #[test]
    fn an_image_change_is_an_update() {
        let r = replan(
            &graph(Vec::new()),
            &probe(known("img:1"), Fact::NotApplicable),
            &probe(known("img:2"), Fact::NotApplicable),
        );
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].action.kind, ActionKind::Update);
        assert!(r.items[0].reason.as_deref().unwrap().contains("image"));
    }

    #[test]
    fn a_binary_path_change_is_an_update() {
        let r = replan(
            &graph(Vec::new()),
            &probe(Fact::NotApplicable, known("/old/llama-server")),
            &probe(Fact::NotApplicable, known("/new/llama-server")),
        );
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].action.kind, ActionKind::Update);
        assert!(r.items[0].reason.as_deref().unwrap().contains("binary"));
    }

    #[test]
    fn unknown_or_unrecorded_inputs_never_invent_a_change() {
        // Binary recorded, now not found: unknown is not a change.
        let r = replan(
            &graph(Vec::new()),
            &probe(Fact::NotApplicable, known("/bin/llama")),
            &probe(Fact::NotApplicable, Fact::Unknown),
        );
        assert!(r.items.is_empty());
        // Nothing recorded at all (older state): never a change.
        let g = graph(Vec::new());
        let r = build_plan_with(
            &g,
            Some(&g),
            &PlanInputs {
                project_root: None,
                recorded: &BTreeMap::new(),
                probe: &probe(known("img:9"), known("/bin/x")),
            },
        )
        .unwrap();
        assert!(r.items.is_empty());
        // ... but the baseline is reported for adoption.
        assert!(r.fingerprints.values().all(|f| f.image.is_some()));
    }

    #[test]
    fn fingerprints_never_contain_raw_values() {
        let fp = Fingerprint::compute(
            &keyed(Some("3")).deployments[0],
            None,
            &probe(known("registry/secret-image"), known("/home/u/bin")),
        );
        let json = serde_json::to_string(&fp).unwrap_or_default();
        assert!(!json.contains("secret-image") && !json.contains("/home/u"));
    }

    #[test]
    fn a_new_deployment_is_a_create_with_its_fingerprint() {
        let g = keyed(None);
        let r = build_plan(&g, None).unwrap();
        assert_eq!(r.items[0].action.kind, ActionKind::Create);
        assert!(r.items[0].fingerprint.is_some());
    }
}
