//! Canonical [`Graph`] -> HCL text.

use hcl::expr::{FuncCall, Traversal, TraversalOperator, Variable};
use hcl::{Block, Body, Expression, Identifier};
use serde::Serialize;

use amicu_core::graph::Graph;
use amicu_core::resource::{
    Credential, Deployment, HardwareProfile, Model, Money, Residency, Runtime, ScalingSettings,
    SpeculativeSettings, Tenant, VllmSettings,
};
use amicu_core::secret::SecretRef;

use crate::error::HclError;

/// Render a graph as HCL text that re-parses into an equal graph.
///
/// # Contract
///
/// The round-trip is only lossless when every resource id, every tenant label
/// key, and every reference edge value is a valid HCL identifier, because ids
/// and references are emitted as bare traversals (`model.<id>.id`) and label
/// keys as bare attribute names. These serialize identically to the DSL/JSON
/// string form, so a graph built or deserialized outside
/// [`parse_graph`](crate::parse_graph) may carry a value that is not
/// identifier-safe (empty, leading digit, or containing `.` or whitespace) and
/// would otherwise reparse to a *different* value. A dangling reference
/// (pointing at an id absent from the graph) is the case where such a value can
/// slip through, since it is never checked against a real id.
///
/// Rather than corrupt such a graph silently, this function validates every id,
/// label key, and reference up front and returns [`HclError::InvalidIdentifier`]
/// on the first offender. For any graph it accepts,
/// `parse_graph(&format_graph(g)?)` recovers an equal graph.
///
/// # Errors
///
/// Returns [`HclError::InvalidIdentifier`] if a resource id, tenant label key,
/// or reference edge value is not a valid, round-trip-safe HCL identifier.
pub fn format_graph(graph: &Graph) -> Result<String, HclError> {
    validate_identifiers(graph)?;

    let mut body = Body::builder();
    for m in &graph.models {
        body = body.add_block(model_block(m));
    }
    for h in &graph.hardware_profiles {
        body = body.add_block(hardware_block(h));
    }
    for d in &graph.deployments {
        body = body.add_block(deployment_block(d));
    }
    for p in &graph.routing_policies {
        body = body.add_block(routing_block(p));
    }
    for c in &graph.data_classes {
        body = body.add_block(data_class_block(c));
    }
    for r in &graph.governance_rules {
        body = body.add_block(governance_block(r));
    }
    for b in &graph.budgets {
        body = body.add_block(budget_block(b));
    }
    for t in &graph.tenants {
        body = body.add_block(tenant_block(t));
    }
    for u in &graph.use_cases {
        body = body.add_block(use_case_block(u));
    }
    for c in &graph.credentials {
        body = body.add_block(credential_block(c));
    }
    Ok(hcl::to_string(&body.build()).expect("HCL body always serializes"))
}

/// Reject any resource id, tenant label key, or reference edge value that is
/// not a valid HCL identifier, since such values cannot round-trip through the
/// emitted HCL.
fn validate_identifiers(graph: &Graph) -> Result<(), HclError> {
    let ids = graph
        .models
        .iter()
        .map(|m| ("model", m.id.as_str()))
        .chain(
            graph
                .hardware_profiles
                .iter()
                .map(|h| ("hardware_profile", h.id.as_str())),
        )
        .chain(
            graph
                .deployments
                .iter()
                .map(|d| ("deployment", d.id.as_str())),
        )
        .chain(
            graph
                .routing_policies
                .iter()
                .map(|p| ("routing_policy", p.id.as_str())),
        )
        .chain(
            graph
                .data_classes
                .iter()
                .map(|c| ("data_class", c.id.as_str())),
        )
        .chain(
            graph
                .governance_rules
                .iter()
                .map(|r| ("governance_rule", r.id.as_str())),
        )
        .chain(graph.budgets.iter().map(|b| ("budget", b.id.as_str())))
        .chain(graph.tenants.iter().map(|t| ("tenant", t.id.as_str())))
        .chain(graph.use_cases.iter().map(|u| ("use_case", u.id.as_str())))
        .chain(
            graph
                .credentials
                .iter()
                .map(|c| ("credential", c.id.as_str())),
        );

    for (kind, id) in ids {
        if Identifier::new(id).is_err() {
            return Err(HclError::invalid_identifier(
                format!("{kind} resource id"),
                id,
            ));
        }
    }

    for t in &graph.tenants {
        for key in t.labels.keys() {
            if Identifier::new(key).is_err() {
                return Err(HclError::invalid_identifier(
                    format!("tenant `{}` label key", t.id.as_str()),
                    key,
                ));
            }
        }
    }

    for d in &graph.deployments {
        let env = match &d.runtime {
            Runtime::LlamaCpp(s) => &s.env,
            Runtime::Vllm(s) => &s.env,
        };
        for key in env.keys() {
            if Identifier::new(key).is_err() {
                return Err(HclError::invalid_identifier(
                    format!("deployment `{}` runtime env key", d.id.as_str()),
                    key.as_str(),
                ));
            }
        }
    }

    validate_references(graph)
}

/// Reject any reference edge value that is not a valid HCL identifier. A
/// dangling reference (one that matches no resource id) is where such a value
/// can slip through, so every edge is checked independently of resolution.
fn validate_references(graph: &Graph) -> Result<(), HclError> {
    let check = |resource: String, value: &str| -> Result<(), HclError> {
        if Identifier::new(value).is_err() {
            return Err(HclError::invalid_identifier(resource, value));
        }
        Ok(())
    };

    for d in &graph.deployments {
        let id = d.id.as_str();
        check(
            format!("deployment `{id}` model reference"),
            d.model.as_str(),
        )?;
        check(
            format!("deployment `{id}` hardware reference"),
            d.hardware_profile.as_str(),
        )?;
        // The llama.cpp `speculative {}` draft model is emitted as a
        // `model.<id>.id` traversal like any other edge, so it needs the same
        // identifier check.
        if let Runtime::LlamaCpp(s) = &d.runtime
            && let Some(spec) = &s.speculative
        {
            check(
                format!("deployment `{id}` speculative draft model reference"),
                spec.model.as_str(),
            )?;
        }
    }
    for p in &graph.routing_policies {
        for t in &p.targets {
            check(
                format!(
                    "routing_policy `{}` target deployment reference",
                    p.id.as_str()
                ),
                t.deployment.as_str(),
            )?;
        }
    }
    for r in &graph.governance_rules {
        for dc in &r.data_classes {
            check(
                format!("governance_rule `{}` data_class reference", r.id.as_str()),
                dc.as_str(),
            )?;
        }
    }
    for b in &graph.budgets {
        if let Some(tenant) = &b.tenant {
            check(
                format!("budget `{}` tenant reference", b.id.as_str()),
                tenant.as_str(),
            )?;
        }
    }
    for u in &graph.use_cases {
        if let Some(dc) = &u.required_data_class {
            check(
                format!("use_case `{}` required_data_class reference", u.id.as_str()),
                dc.as_str(),
            )?;
        }
    }

    Ok(())
}

fn model_block(m: &Model) -> Block {
    let mut body = Body::builder()
        .add_block(
            Block::builder("source")
                .add_attribute(("registry", variant(&m.source.registry)))
                .add_attribute(("reference", m.source.reference.clone()))
                .build(),
        )
        .add_attribute(("architecture", m.architecture.clone()))
        .add_attribute(("modalities", string_array(&m.modalities)));

    let mut artifact =
        Block::builder("artifact").add_attribute(("format", variant(&m.artifact.format)));
    if let Some(q) = &m.artifact.quantization {
        artifact = artifact.add_attribute(("quantization", variant(q)));
    }
    body = body.add_block(artifact.build());

    if let Some(len) = m.context_length {
        body = body.add_attribute(("context_length", len));
    }
    for v in &m.variants {
        let mut vb = Block::builder("variant")
            .add_attribute(("quantization", variant(&v.quantization)))
            .add_attribute(("file", v.file.clone()));
        if let Some(size) = v.size {
            vb = vb.add_attribute(("size", size.to_string()));
        }
        if let Some(sha256) = &v.sha256 {
            vb = vb.add_attribute(("sha256", sha256.clone()));
        }
        body = body.add_block(vb.build());
    }
    if let Some(v) = m.n_layers {
        body = body.add_attribute(("n_layers", v));
    }
    if let Some(v) = m.n_kv_heads {
        body = body.add_attribute(("n_kv_heads", v));
    }
    if let Some(v) = m.head_dim {
        body = body.add_attribute(("head_dim", v));
    }
    if let Some(v) = m.n_kv_layers {
        body = body.add_attribute(("n_kv_layers", v));
    }
    resource("model", m.id.as_str(), body.build())
}

fn hardware_block(h: &HardwareProfile) -> Block {
    let mut body = Body::builder().add_attribute(("provider", h.provider.clone()));
    if let Some(region) = &h.region {
        // HCL carries only the region code; residency is resolved elsewhere.
        body = body.add_attribute(("region", region.code.clone()));
    }
    if let Some(a) = &h.accelerator {
        let mut ab = Block::builder("accelerator")
            .add_attribute(("kind", variant(&a.kind)))
            .add_attribute(("model", a.model.clone()))
            .add_attribute(("count", a.count))
            .add_attribute(("memory", a.memory.to_string()));
        if let Some(bandwidth) = a.memory_bandwidth_gbps {
            ab = ab.add_attribute(("bandwidth_gbps", bandwidth));
        }
        body = body.add_block(ab.build());
    }
    if let Some(c) = &h.cpu {
        body = body.add_block(
            Block::builder("cpu")
                .add_attribute(("model", c.model.clone()))
                .add_attribute(("cores", c.cores))
                .build(),
        );
    }
    body = body.add_attribute(("memory", h.memory.to_string()));
    if let Some(i) = &h.interconnect {
        body = body.add_attribute(("interconnect", variant(i)));
    }
    if let Some(p) = &h.price {
        body = body.add_block(
            Block::builder("price")
                .add_block(money_block("per_hour", &p.per_hour))
                .build(),
        );
    }
    if let Some(r) = &h.runpod {
        let mut rb = Block::builder("runpod").add_attribute(("mode", variant(&r.mode)));
        if let Some(cloud) = &r.cloud {
            rb = rb.add_attribute(("cloud", variant(cloud)));
        }
        if let Some(v) = &r.network_volume {
            rb = rb.add_attribute(("network_volume", v.as_str()));
        }
        if let Some(v) = r.flashboot {
            rb = rb.add_attribute(("flashboot", v));
        }
        body = body.add_block(rb.build());
    }
    resource("hardware_profile", h.id.as_str(), body.build())
}

fn deployment_block(d: &Deployment) -> Block {
    let mut body = Body::builder()
        .add_attribute(("model", reference("model", d.model.as_str())))
        .add_attribute((
            "hardware",
            reference("hardware_profile", d.hardware_profile.as_str()),
        ));
    match &d.runtime {
        Runtime::LlamaCpp(s) => {
            let mut b = Block::builder("llama_cpp")
                .add_attribute(("context_length", s.context_length))
                .add_attribute(("port", s.port))
                .add_attribute(("n_gpu_layers", s.n_gpu_layers));
            if let Some(v) = s.temperature {
                b = b.add_attribute(("temperature", f64::from(v)));
            }
            if let Some(v) = s.top_p {
                b = b.add_attribute(("top_p", f64::from(v)));
            }
            if let Some(v) = s.top_k {
                b = b.add_attribute(("top_k", v));
            }
            if let Some(v) = s.min_p {
                b = b.add_attribute(("min_p", f64::from(v)));
            }
            if let Some(v) = s.repeat_penalty {
                b = b.add_attribute(("repeat_penalty", f64::from(v)));
            }
            if let Some(v) = &s.cache_type_k {
                b = b.add_attribute(("cache_type_k", variant(v)));
            }
            if let Some(v) = &s.cache_type_v {
                b = b.add_attribute(("cache_type_v", variant(v)));
            }
            if let Some(v) = s.batch_size {
                b = b.add_attribute(("batch_size", v));
            }
            if let Some(v) = s.ubatch_size {
                b = b.add_attribute(("ubatch_size", v));
            }
            if let Some(v) = s.parallel_slots {
                b = b.add_attribute(("parallel_slots", v));
            }
            if let Some(v) = &s.flash_attention {
                b = b.add_attribute(("flash_attention", variant(v)));
            }
            if let Some(v) = &s.chat_template {
                b = b.add_attribute(("chat_template", v.clone()));
            }
            if let Some(v) = &s.chat_template_file {
                b = b.add_attribute(("chat_template_file", v.clone()));
            }
            if let Some(v) = s.jinja {
                b = b.add_attribute(("jinja", v));
            }
            if let Some(v) = &s.reasoning_format {
                b = b.add_attribute(("reasoning_format", variant(v)));
            }
            if let Some(spec) = &s.speculative {
                b = b.add_block(speculative_block(spec));
            }
            if !s.extra_args.is_empty() {
                b = b.add_attribute(("extra_args", str_array(&s.extra_args)));
            }
            if !s.env.is_empty() {
                b = b.add_block(env_block(&s.env));
            }
            if let Some(v) = &s.build {
                b = b.add_attribute(("build", v.clone()));
            }
            if let Some(m) = &s.multimodal {
                let mut mb = Block::builder("multimodal");
                if let Some(v) = &m.projector {
                    mb = mb.add_attribute(("projector", v.clone()));
                }
                if let Some(v) = m.offload {
                    mb = mb.add_attribute(("offload", v));
                }
                b = b.add_block(mb.build());
            }
            body = body
                .add_attribute(("runtime", "llama_cpp"))
                .add_block(b.build());
        }
        Runtime::Vllm(s) => {
            body = body
                .add_attribute(("runtime", "vllm"))
                .add_block(vllm_block(s));
        }
    }
    if let Some(key) = &d.api_key {
        body = body.add_attribute(("api_key", secret_call(key)));
    }
    if !d.memory_check.is_enforce() {
        body = body.add_attribute(("memory_check", d.memory_check.as_str()));
    }
    if let Some(sc) = &d.scaling {
        body = body.add_block(scaling_block(sc));
    }
    resource("deployment", d.id.as_str(), body.build())
}

/// A deployment's `scaling {}` block; the window is
/// written as a canonical duration string (`"15m"`).
fn scaling_block(sc: &ScalingSettings) -> Block {
    let mut b = Block::builder("scaling");
    if let Some(v) = sc.min_replicas {
        b = b.add_attribute(("min_replicas", v));
    }
    if let Some(v) = sc.max_replicas {
        b = b.add_attribute(("max_replicas", v));
    }
    if let Some(v) = sc.target_concurrency {
        b = b.add_attribute(("target_concurrency", v));
    }
    if let Some(v) = sc.scaledown_window_secs {
        b = b.add_attribute(("scaledown_window", ScalingSettings::format_window(v)));
    }
    b.build()
}

/// A llama.cpp `speculative {}` block. `model` is
/// emitted as a `model.<id>.id` traversal - the same reference form the
/// deployment's own `model` attribute uses - because it is a real graph edge,
/// not a path string. Its identifier-safety is checked up front by
/// [`validate_references`].
fn speculative_block(s: &SpeculativeSettings) -> Block {
    let mut b = Block::builder("speculative")
        .add_attribute(("model", reference("model", s.model.as_str())));
    if let Some(v) = s.n_max {
        b = b.add_attribute(("n_max", v));
    }
    if let Some(v) = s.n_min {
        b = b.add_attribute(("n_min", v));
    }
    if let Some(v) = s.p_min {
        b = b.add_attribute(("p_min", f64::from(v)));
    }
    if let Some(v) = s.n_gpu_layers {
        b = b.add_attribute(("n_gpu_layers", v));
    }
    b.build()
}

fn vllm_block(s: &VllmSettings) -> Block {
    let mut b = Block::builder("vllm")
        .add_attribute(("context_length", s.context_length))
        .add_attribute(("port", s.port));
    if let Some(tp) = s.tensor_parallel_size {
        b = b.add_attribute(("tensor_parallel_size", tp));
    }
    if let Some(util) = s.gpu_memory_utilization {
        b = b.add_attribute(("gpu_memory_utilization", f64::from(util)));
    }
    if let Some(v) = &s.kv_cache_dtype {
        b = b.add_attribute(("kv_cache_dtype", variant(v)));
    }
    if let Some(v) = s.max_num_seqs {
        b = b.add_attribute(("max_num_seqs", v));
    }
    if let Some(v) = s.max_num_batched_tokens {
        b = b.add_attribute(("max_num_batched_tokens", v));
    }
    if let Some(v) = s.enforce_eager {
        b = b.add_attribute(("enforce_eager", v));
    }
    if !s.extra_args.is_empty() {
        b = b.add_attribute(("extra_args", str_array(&s.extra_args)));
    }
    if !s.env.is_empty() {
        b = b.add_block(env_block(&s.env));
    }
    b.build()
}

/// A runtime settings block's `env {}` map, in `BTreeMap` (deterministic) key
/// order. Keys are validated as HCL identifiers up front by
/// [`validate_identifiers`], so [`Identifier::unchecked`] here never corrupts
/// a key.
fn env_block(env: &std::collections::BTreeMap<String, String>) -> Block {
    let mut eb = Block::builder("env");
    for (k, v) in env {
        eb = eb.add_attribute((Identifier::unchecked(k), v.clone()));
    }
    eb.build()
}

fn routing_block(p: &amicu_core::resource::RoutingPolicy) -> Block {
    let mut body = Body::builder();
    if !p.aliases.is_empty() {
        body = body.add_attribute(("aliases", str_array(&p.aliases)));
    }
    for t in &p.targets {
        body = body.add_block(
            Block::builder("target")
                .add_attribute(("deployment", reference("deployment", t.deployment.as_str())))
                .add_attribute(("weight", t.weight))
                .build(),
        );
    }
    if let Some(fo) = &p.failover {
        let mut fb = Block::builder("failover").add_attribute(("enabled", fo.enabled));
        if let Some(v) = fo.health_check_interval_secs {
            fb = fb.add_attribute(("health_check_interval_secs", v));
        }
        if let Some(v) = fo.max_failures {
            fb = fb.add_attribute(("max_failures", v));
        }
        body = body.add_block(fb.build());
    }
    if let Some(o) = &p.optimize_for {
        body = body.add_attribute(("optimize_for", variant(o)));
    }
    resource("routing_policy", p.id.as_str(), body.build())
}

fn data_class_block(c: &amicu_core::resource::DataClass) -> Block {
    let mut body = Body::builder().add_attribute(("sensitivity", variant(&c.sensitivity)));
    if !c.residency.is_empty() {
        body = body.add_attribute(("residency", residency_array(&c.residency)));
    }
    body = body.add_attribute(("allow_external_providers", c.allow_external_providers));
    if !c.handling.is_empty() {
        body = body.add_attribute(("handling", str_array(&c.handling)));
    }
    resource("data_class", c.id.as_str(), body.build())
}

fn governance_block(r: &amicu_core::resource::GovernanceRule) -> Block {
    let mut body = Body::builder();
    if let Some(desc) = &r.description {
        body = body.add_attribute(("description", desc.clone()));
    }
    body = body.add_attribute(("effect", variant(&r.effect)));
    if !r.data_classes.is_empty() {
        let refs: Vec<Expression> = r
            .data_classes
            .iter()
            .map(|dc| reference("data_class", dc.as_str()))
            .collect();
        body = body.add_attribute(("data_classes", Expression::Array(refs)));
    }
    if !r.allowed_providers.is_empty() {
        body = body.add_attribute(("allowed_providers", str_array(&r.allowed_providers)));
    }
    if !r.allowed_regions.is_empty() {
        body = body.add_attribute(("allowed_regions", str_array(&r.allowed_regions)));
    }
    resource("governance_rule", r.id.as_str(), body.build())
}

fn budget_block(b: &amicu_core::resource::Budget) -> Block {
    let mut body = Body::builder();
    if let Some(tenant) = &b.tenant {
        body = body.add_attribute(("tenant", reference("tenant", tenant.as_str())));
    }
    body = body
        .add_block(money_block("limit", &b.limit))
        .add_attribute(("period", variant(&b.period)));
    if !b.alert_thresholds.is_empty() {
        body = body.add_attribute((
            "alert_thresholds",
            Expression::from(b.alert_thresholds.clone()),
        ));
    }
    resource("budget", b.id.as_str(), body.build())
}

fn tenant_block(t: &Tenant) -> Block {
    let mut body = Body::builder();
    if let Some(name) = &t.display_name {
        body = body.add_attribute(("display_name", name.clone()));
    }
    if let Some(q) = &t.quota {
        let mut qb = Block::builder("quota");
        if let Some(v) = q.max_requests_per_minute {
            qb = qb.add_attribute(("max_requests_per_minute", v));
        }
        if let Some(v) = q.max_concurrent {
            qb = qb.add_attribute(("max_concurrent", v));
        }
        body = body.add_block(qb.build());
    }
    if !t.labels.is_empty() {
        let mut lb = Block::builder("labels");
        for (k, v) in &t.labels {
            lb = lb.add_attribute((Identifier::unchecked(k), v.clone()));
        }
        body = body.add_block(lb.build());
    }
    resource("tenant", t.id.as_str(), body.build())
}

fn use_case_block(u: &amicu_core::resource::UseCase) -> Block {
    let mut body = Body::builder();
    if let Some(desc) = &u.description {
        body = body.add_attribute(("description", desc.clone()));
    }
    if !u.modalities.is_empty() {
        body = body.add_attribute(("modalities", string_array(&u.modalities)));
    }
    if let Some(v) = u.min_context_length {
        body = body.add_attribute(("min_context_length", v));
    }
    if let Some(v) = u.max_latency_ms {
        body = body.add_attribute(("max_latency_ms", v));
    }
    if let Some(dc) = &u.required_data_class {
        body = body.add_attribute(("required_data_class", reference("data_class", dc.as_str())));
    }
    if let Some(p) = &u.priority {
        body = body.add_attribute(("priority", variant(p)));
    }
    resource("use_case", u.id.as_str(), body.build())
}

/// A `credential` resource: every secret-designated
/// field always emits a `secret(...)` reference, matching what
/// `amicu_hcl::de` accepts back on parse (see `secret_ref_from_expr`).
fn credential_block(c: &Credential) -> Block {
    let mut body = Body::builder();
    if let Some(v) = &c.api_key {
        body = body.add_attribute(("api_key", secret_call(v)));
    }
    if let Some(v) = &c.token {
        body = body.add_attribute(("token", secret_call(v)));
    }
    if let Some(v) = &c.password {
        body = body.add_attribute(("password", secret_call(v)));
    }
    if !c.env.is_empty() {
        let mut eb = Block::builder("env");
        for (k, v) in &c.env {
            eb = eb.add_attribute((Identifier::unchecked(k), secret_call(v)));
        }
        body = body.add_block(eb.build());
    }
    resource("credential", c.id.as_str(), body.build())
}

/// Render a `SecretRef` as the `secret("<ref>")` reference expression that
/// `amicu_hcl::de::secret_ref_from_expr` parses back.
fn secret_call(secret: &SecretRef) -> Expression {
    Expression::FuncCall(Box::new(
        FuncCall::builder("secret").arg(secret.name.clone()).build(),
    ))
}

fn money_block(ident: &str, m: &Money) -> Block {
    Block::builder(ident)
        .add_attribute(("amount", m.amount))
        .add_attribute(("currency", m.currency.clone()))
        .build()
}

fn resource(kind: &str, id: &str, inner: Body) -> Block {
    Block::builder("resource")
        .add_label(kind)
        .add_label(id)
        .add_structures(inner.into_inner())
        .build()
}

/// A reference expression such as `model.support_chat.id`.
fn reference(kind: &str, id: &str) -> Expression {
    Traversal::new(
        Variable::unchecked(kind),
        [
            TraversalOperator::GetAttr(Identifier::unchecked(id)),
            TraversalOperator::GetAttr(Identifier::unchecked("id")),
        ],
    )
    .into()
}

/// The DSL spelling of a `snake_case` enum value.
fn variant<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => unreachable!("resource enums serialize as strings"),
    }
}

fn string_array<T: Serialize>(values: &[T]) -> Expression {
    Expression::from(values.iter().map(variant).collect::<Vec<_>>())
}

fn str_array(values: &[String]) -> Expression {
    Expression::from(values.to_vec())
}

/// Render residency jurisdictions as their DSL tokens. Unlike [`variant`], this
/// does not panic on [`Residency::Other`], which serializes as an object.
fn residency_array(values: &[Residency]) -> Expression {
    Expression::from(
        values
            .iter()
            .map(|r| r.as_token().to_owned())
            .collect::<Vec<_>>(),
    )
}
