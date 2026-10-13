//! HCL text -> canonical [`Graph`].

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use hcl::expr::TraversalOperator;
use hcl::{Body, Expression};
use serde::de::DeserializeOwned;

use amicu_core::graph::Graph;
use amicu_core::resource::{
    Accelerator, Artifact, Budget, Cpu, Credential, DataClass, Deployment, FlashAttention,
    GgufVariant, GovernanceRule, HardwareProfile, KvCacheType, LlamaCppSettings, Model,
    ModelSource, Money, MultimodalSettings, Price, Quota, ReasoningFormat, Region, Residency,
    RoutingPolicy, RoutingTarget, Runtime, ScalingSettings, SpeculativeSettings, Tenant, UseCase,
    VllmKvCacheDtype, VllmSettings,
};
use amicu_core::secret::SecretRef;
use amicu_core::units::ByteSize;

use crate::error::HclError;

type Result<T> = std::result::Result<T, HclError>;

/// Parse HCL source into the canonical resource graph.
pub fn parse_graph(src: &str) -> Result<Graph> {
    let body = hcl::parse(src)?;
    let mut graph = Graph::new();

    if let Some(attr) = body.attributes().next() {
        return Err(HclError::unknown_field(
            "top level",
            "attribute",
            attr.key(),
            None,
        ));
    }

    for block in body.blocks() {
        if block.identifier() != "resource" {
            return Err(HclError::schema(format!(
                "unexpected top-level block `{}`, only `resource` blocks are allowed",
                block.identifier()
            )));
        }
        let labels = block.labels();
        let [kind, name] = labels else {
            return Err(HclError::schema(
                "a `resource` block requires exactly two labels: kind and name",
            ));
        };
        let (kind, name) = (kind.as_str(), name.as_str());
        let registry = Registry::default();
        let f = Fields::new(block.body(), format!("{kind} {name}"), &registry);

        match kind {
            "model" => graph.models.push(model(name, &f)?),
            "hardware_profile" => graph.hardware_profiles.push(hardware_profile(name, &f)?),
            "deployment" => graph.deployments.push(deployment(name, &f)?),
            "routing_policy" => graph.routing_policies.push(routing_policy(name, &f)?),
            "data_class" => graph.data_classes.push(data_class(name, &f)?),
            "governance_rule" => graph.governance_rules.push(governance_rule(name, &f)?),
            "budget" => graph.budgets.push(budget(name, &f)?),
            "tenant" => graph.tenants.push(tenant(name, &f)?),
            "use_case" => graph.use_cases.push(use_case(name, &f)?),
            "credential" => graph.credentials.push(credential(name, &f)?),
            other => return Err(HclError::schema(format!("unknown resource kind `{other}`"))),
        }
        // Every accessor the builder above called recorded the key it read;
        // anything left over in a body is unknown to the schema. The known
        // set therefore comes from the parser's own reads, not a second list.
        registry.ensure_exhaustive()?;
    }

    Ok(graph)
}

fn model(name: &str, f: &Fields) -> Result<Model> {
    let src = f.block("source")?;
    Ok(Model {
        id: name.into(),
        source: ModelSource {
            registry: src.enum_val("registry")?,
            reference: src.string("reference")?,
        },
        architecture: f.string("architecture")?,
        modalities: f.enum_vec("modalities")?,
        artifact: {
            let a = f.block("artifact")?;
            Artifact {
                format: a.enum_val("format")?,
                quantization: a.opt_enum("quantization")?,
            }
        },
        context_length: f.opt_u32("context_length")?,
        variants: f
            .blocks("variant")
            .into_iter()
            .map(|v| -> Result<GgufVariant> {
                Ok(GgufVariant {
                    quantization: v.enum_val("quantization")?,
                    file: v.string("file")?,
                    size: v.opt_byte_size("size")?,
                    sha256: v.opt_sha256("sha256")?,
                })
            })
            .collect::<Result<_>>()?,
        // Declared shape metadata feeding the KV-cache estimator
        // (amicu-planning#151) - there is no GGUF header parser in-tree, so
        // these are never derived.
        n_layers: f.opt_u32("n_layers")?,
        n_kv_heads: f.opt_u32("n_kv_heads")?,
        head_dim: f.opt_u32("head_dim")?,
        n_kv_layers: f.opt_u32("n_kv_layers")?,
    })
}

fn hardware_profile(name: &str, f: &Fields) -> Result<HardwareProfile> {
    Ok(HardwareProfile {
        id: name.into(),
        provider: f.string("provider")?,
        region: f.opt_string("region")?.map(Region::from_code),
        accelerator: f
            .opt_block("accelerator")
            .map(|a| -> Result<Accelerator> {
                Ok(Accelerator {
                    kind: a.enum_val("kind")?,
                    model: a.string("model")?,
                    count: a.u32("count")?,
                    memory: a.byte_size("memory")?,
                    memory_bandwidth_gbps: a.opt_u32("bandwidth_gbps")?,
                })
            })
            .transpose()?,
        cpu: f
            .opt_block("cpu")
            .map(|c| -> Result<Cpu> {
                Ok(Cpu {
                    model: c.string("model")?,
                    cores: c.u32("cores")?,
                })
            })
            .transpose()?,
        memory: f.byte_size("memory")?,
        interconnect: f.opt_enum("interconnect")?,
        price: f
            .opt_block("price")
            .map(|p| -> Result<Price> {
                Ok(Price {
                    per_hour: money(&p.block("per_hour")?)?,
                })
            })
            .transpose()?,
    })
}

fn deployment(name: &str, f: &Fields) -> Result<Deployment> {
    let tag = f.string("runtime")?;
    let runtime = match tag.as_str() {
        "llama_cpp" => {
            let s = f.block("llama_cpp")?;
            Runtime::LlamaCpp(LlamaCppSettings {
                context_length: s.u32("context_length")?,
                port: s.u16("port")?,
                n_gpu_layers: s.i32("n_gpu_layers")?,
                temperature: s.opt_f32("temperature")?,
                top_p: s.opt_f32("top_p")?,
                top_k: s.opt_u32("top_k")?,
                min_p: s.opt_f32("min_p")?,
                repeat_penalty: s.opt_f32("repeat_penalty")?,
                cache_type_k: s.opt_enum::<KvCacheType>("cache_type_k")?,
                cache_type_v: s.opt_enum::<KvCacheType>("cache_type_v")?,
                batch_size: s.opt_u32("batch_size")?,
                ubatch_size: s.opt_u32("ubatch_size")?,
                parallel_slots: s.opt_u32("parallel_slots")?,
                flash_attention: s.opt_enum::<FlashAttention>("flash_attention")?,
                chat_template: s.opt_string("chat_template")?,
                chat_template_file: s.opt_string("chat_template_file")?,
                jinja: s.opt_bool("jinja")?,
                reasoning_format: s.opt_enum::<ReasoningFormat>("reasoning_format")?,
                // `speculative { model = model.<id>.id ... }` - the draft
                // model is a reference expression, parsed with the same
                // `reference` helper the deployment's primary `model` uses
                // (amicu-planning#151 PR C), so it is a real graph edge.
                speculative: s
                    .opt_block("speculative")
                    .map(|spec| -> Result<SpeculativeSettings> {
                        Ok(SpeculativeSettings {
                            model: spec.reference("model")?.into(),
                            n_max: spec.opt_u32("n_max")?,
                            n_min: spec.opt_u32("n_min")?,
                            p_min: spec.opt_f32("p_min")?,
                            n_gpu_layers: spec.opt_i32("n_gpu_layers")?,
                        })
                    })
                    .transpose()?,
                extra_args: s.string_vec("extra_args")?,
                env: match s.opt_block("env") {
                    Some(e) => e.string_map()?,
                    None => BTreeMap::new(),
                },
                build: s.opt_build("build")?,
                multimodal: s
                    .opt_block("multimodal")
                    .map(|m| -> Result<MultimodalSettings> {
                        Ok(MultimodalSettings {
                            projector: m.opt_string("projector")?,
                            offload: m.opt_bool("offload")?,
                        })
                    })
                    .transpose()?,
            })
        }
        "vllm" => {
            let s = f.block("vllm")?;
            // `multimodal { ... }` is llama.cpp-only (amicu-planning#151 PR
            // D: it backs `--mmproj`/`--mmproj-offload`, which have no vLLM
            // equivalent, and `VllmSettings` carries no such field to parse
            // it into) - reject it structurally here rather than silently
            // dropping it, which would otherwise happen since nothing else
            // reads a `vllm {}` body's unrecognized blocks.
            if s.opt_block("multimodal").is_some() {
                return Err(HclError::schema(
                    "a `multimodal { ... }` block is only valid inside a `llama_cpp {}` \
                     settings block, not `vllm {}`",
                ));
            }
            Runtime::Vllm(VllmSettings {
                context_length: s.u32("context_length")?,
                port: s.u16("port")?,
                tensor_parallel_size: s.opt_u32("tensor_parallel_size")?,
                gpu_memory_utilization: s.opt_f32("gpu_memory_utilization")?,
                kv_cache_dtype: s.opt_enum::<VllmKvCacheDtype>("kv_cache_dtype")?,
                max_num_seqs: s.opt_u32("max_num_seqs")?,
                max_num_batched_tokens: s.opt_u32("max_num_batched_tokens")?,
                enforce_eager: s.opt_bool("enforce_eager")?,
                extra_args: s.string_vec("extra_args")?,
                env: match s.opt_block("env") {
                    Some(e) => e.string_map()?,
                    None => BTreeMap::new(),
                },
            })
        }
        other => return Err(HclError::schema(format!("unknown runtime `{other}`"))),
    };
    Ok(Deployment {
        scaling: scaling_settings(f)?,
        id: name.into(),
        model: f.reference("model")?.into(),
        hardware_profile: f.reference("hardware")?.into(),
        runtime,
    })
}

/// The deployment-level `scaling { ... }` block (amicu-planning#180).
/// `scaledown_window` is a duration string (`"15m"`); value ranges are
/// checked later by `AMICU-CAP-008`, only the syntax is checked here.
fn scaling_settings(f: &Fields) -> Result<Option<ScalingSettings>> {
    f.opt_block("scaling")
        .map(|s| -> Result<ScalingSettings> {
            Ok(ScalingSettings {
                min_replicas: s.opt_u32("min_replicas")?,
                max_replicas: s.opt_u32("max_replicas")?,
                target_concurrency: s.opt_u32("target_concurrency")?,
                scaledown_window_secs: s
                    .opt_string("scaledown_window")?
                    .map(|w| {
                        ScalingSettings::parse_window(&w).ok_or_else(|| {
                            HclError::schema(format!(
                                "`scaledown_window` must be a duration like \"900s\", \"15m\" or \"1h\", got \"{w}\""
                            ))
                        })
                    })
                    .transpose()?,
            })
        })
        .transpose()
}

fn routing_policy(name: &str, f: &Fields) -> Result<RoutingPolicy> {
    Ok(RoutingPolicy {
        id: name.into(),
        aliases: f.string_vec("aliases")?,
        targets: f
            .blocks("target")
            .into_iter()
            .map(|t| -> Result<RoutingTarget> {
                Ok(RoutingTarget {
                    deployment: t.reference("deployment")?.into(),
                    weight: t.u32("weight")?,
                })
            })
            .collect::<Result<_>>()?,
        failover: f
            .opt_block("failover")
            .map(|p| -> Result<amicu_core::resource::FailoverPolicy> {
                Ok(amicu_core::resource::FailoverPolicy {
                    enabled: p.bool("enabled")?,
                    health_check_interval_secs: p.opt_u32("health_check_interval_secs")?,
                    max_failures: p.opt_u32("max_failures")?,
                })
            })
            .transpose()?,
        optimize_for: f.opt_enum("optimize_for")?,
    })
}

fn data_class(name: &str, f: &Fields) -> Result<DataClass> {
    Ok(DataClass {
        id: name.into(),
        sensitivity: f.enum_val("sensitivity")?,
        // Residency jurisdictions arrive as bare tokens ("eu", "us", ...); an
        // unknown token maps to Residency::Other rather than erroring.
        residency: f
            .string_vec("residency")?
            .into_iter()
            .map(Residency::from_token)
            .collect(),
        allow_external_providers: f.opt_bool("allow_external_providers")?.unwrap_or(false),
        handling: f.string_vec("handling")?,
    })
}

fn governance_rule(name: &str, f: &Fields) -> Result<GovernanceRule> {
    Ok(GovernanceRule {
        id: name.into(),
        description: f.opt_string("description")?,
        effect: f.enum_val("effect")?,
        data_classes: f
            .reference_vec("data_classes")?
            .into_iter()
            .map(Into::into)
            .collect(),
        allowed_providers: f.string_vec("allowed_providers")?,
        allowed_regions: f.string_vec("allowed_regions")?,
    })
}

fn budget(name: &str, f: &Fields) -> Result<Budget> {
    Ok(Budget {
        id: name.into(),
        tenant: f.opt_reference("tenant")?.map(Into::into),
        limit: money(&f.block("limit")?)?,
        period: f.enum_val("period")?,
        alert_thresholds: f.f64_vec("alert_thresholds")?,
    })
}

fn tenant(name: &str, f: &Fields) -> Result<Tenant> {
    Ok(Tenant {
        id: name.into(),
        display_name: f.opt_string("display_name")?,
        quota: f
            .opt_block("quota")
            .map(|q| -> Result<Quota> {
                Ok(Quota {
                    max_requests_per_minute: q.opt_u32("max_requests_per_minute")?,
                    max_concurrent: q.opt_u32("max_concurrent")?,
                })
            })
            .transpose()?,
        labels: match f.opt_block("labels") {
            Some(l) => l.string_map()?,
            None => BTreeMap::new(),
        },
    })
}

fn use_case(name: &str, f: &Fields) -> Result<UseCase> {
    Ok(UseCase {
        id: name.into(),
        description: f.opt_string("description")?,
        modalities: f.enum_vec("modalities")?,
        min_context_length: f.opt_u32("min_context_length")?,
        max_latency_ms: f.opt_u32("max_latency_ms")?,
        required_data_class: f.opt_reference("required_data_class")?.map(Into::into),
        priority: f.opt_enum("priority")?,
    })
}

/// A `credential` resource: a named bundle of secret-designated fields
/// (amicu-planning#119). Every field here is structurally enforced to be a
/// `secret(...)` reference by [`Fields::opt_secret_ref`] / [`Fields::secret_map`];
/// a literal is a hard parse-time rejection.
fn credential(name: &str, f: &Fields) -> Result<Credential> {
    Ok(Credential {
        id: name.into(),
        api_key: f.opt_secret_ref("api_key")?,
        token: f.opt_secret_ref("token")?,
        password: f.opt_secret_ref("password")?,
        env: f.secret_map("env")?,
    })
}

fn money(f: &Fields) -> Result<Money> {
    Ok(Money {
        amount: f.f64("amount")?,
        currency: f.string("currency")?,
    })
}

/// A cursor over an HCL block body with typed, contextual accessors.
struct Fields<'a> {
    body: &'a Body,
    ctx: String,
    /// Keys (attributes and block identifiers) the parser has asked for.
    seen: Seen,
    registry: Registry<'a>,
}

type Seen = Rc<RefCell<BTreeSet<String>>>;

/// Every body visited while parsing one resource, with the keys read from
/// it, so [`Registry::ensure_exhaustive`] can reject the leftovers
/// (amicu-planning#157).
#[derive(Clone, Default)]
struct Registry<'a> {
    bodies: Rc<RefCell<Vec<Visited<'a>>>>,
    /// Structural problems noticed while reading (duplicate singleton
    /// blocks, labelled nested blocks); reported before unknown keys.
    problems: Rc<RefCell<Vec<String>>>,
}

type Visited<'a> = (&'a Body, String, Seen);

impl Registry<'_> {
    /// Error on the first attribute or block no accessor ever read.
    fn ensure_exhaustive(&self) -> Result<()> {
        if let Some(p) = self.problems.borrow().first() {
            return Err(HclError::schema(p.clone()));
        }
        for (body, ctx, seen) in self.bodies.borrow().iter() {
            let seen = seen.borrow();
            let unknown = body
                .attributes()
                .map(|a| ("attribute", a.key()))
                .chain(body.blocks().map(|b| ("block", b.identifier())))
                .find(|(_, key)| !seen.contains(*key));
            if let Some((kind, key)) = unknown {
                let suggestion = closest(key, seen.iter().map(String::as_str));
                return Err(HclError::unknown_field(ctx.clone(), kind, key, suggestion));
            }
        }
        Ok(())
    }
}

/// The candidate within a small edit distance of `key`, if any.
fn closest<'c>(key: &str, candidates: impl Iterator<Item = &'c str>) -> Option<String> {
    let max = if key.chars().count() <= 4 { 1 } else { 2 };
    // For very short keys a one-letter difference in the first letter is a
    // different word (`cpu` vs `gpu`), not a typo.
    let short = key.chars().count() <= 3;
    candidates
        .filter(|c| !short || c.chars().next() == key.chars().next())
        .map(|c| (edit_distance(key, c), c))
        .filter(|(d, _)| *d <= max)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c.to_string())
}

/// Levenshtein distance.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != *cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

impl<'a> Fields<'a> {
    fn new(body: &'a Body, ctx: String, registry: &Registry<'a>) -> Self {
        let mut bodies = registry.bodies.borrow_mut();
        // Re-visiting the same body (e.g. `opt_block` called twice) must
        // share one seen-set, or the first visit's reads would be lost.
        let seen =
            if let Some((_, _, seen)) = bodies.iter().find(|(b, _, _)| std::ptr::eq(*b, body)) {
                Rc::clone(seen)
            } else {
                let seen = Seen::default();
                bodies.push((body, ctx.clone(), Rc::clone(&seen)));
                seen
            };
        Self {
            body,
            ctx,
            seen,
            registry: registry.clone(),
        }
    }

    fn mark(&self, key: &str) {
        self.seen.borrow_mut().insert(key.to_string());
    }

    fn missing(&self, key: &str) -> HclError {
        // A misspelled required attribute shows up as "missing"; point at the
        // near-miss instead of leaving the user to spot the typo.
        let seen = self.seen.borrow();
        let near = self
            .body
            .attributes()
            .map(hcl::Attribute::key)
            .chain(self.body.blocks().map(hcl::Block::identifier))
            .filter(|k| !seen.contains(*k))
            .find(|k| closest(k, std::iter::once(key)).is_some());
        match near {
            Some(k) => HclError::schema(format!(
                "{}: missing `{key}` (found unknown `{k}`; did you mean `{key}`?)",
                self.ctx
            )),
            None => HclError::schema(format!("{}: missing `{key}`", self.ctx)),
        }
    }

    fn bad(&self, key: &str, want: &str) -> HclError {
        HclError::schema(format!("{}: `{key}` must be {want}", self.ctx))
    }

    fn attr(&self, key: &str) -> Option<&Expression> {
        self.mark(key);
        self.body
            .attributes()
            .find(|a| a.key() == key)
            .map(hcl::Attribute::expr)
    }

    fn req(&self, key: &str) -> Result<&Expression> {
        self.attr(key).ok_or_else(|| self.missing(key))
    }

    fn string(&self, key: &str) -> Result<String> {
        match self.req(key)? {
            Expression::String(s) => Ok(s.clone()),
            _ => Err(self.bad(key, "a string")),
        }
    }

    fn opt_string(&self, key: &str) -> Result<Option<String>> {
        self.attr(key)
            .map_or(Ok(None), |_| self.string(key).map(Some))
    }

    fn u64(&self, key: &str) -> Result<u64> {
        match self.req(key)? {
            Expression::Number(n) => n
                .as_u64()
                .ok_or_else(|| self.bad(key, "a non-negative integer")),
            _ => Err(self.bad(key, "an integer")),
        }
    }

    fn i64(&self, key: &str) -> Result<i64> {
        match self.req(key)? {
            Expression::Number(n) => n.as_i64().ok_or_else(|| self.bad(key, "an integer")),
            _ => Err(self.bad(key, "an integer")),
        }
    }

    fn u32(&self, key: &str) -> Result<u32> {
        u32::try_from(self.u64(key)?).map_err(|_| self.bad(key, "in u32 range"))
    }

    fn u16(&self, key: &str) -> Result<u16> {
        u16::try_from(self.u64(key)?).map_err(|_| self.bad(key, "in u16 range"))
    }

    fn i32(&self, key: &str) -> Result<i32> {
        i32::try_from(self.i64(key)?).map_err(|_| self.bad(key, "in i32 range"))
    }

    fn opt_u32(&self, key: &str) -> Result<Option<u32>> {
        self.attr(key).map_or(Ok(None), |_| self.u32(key).map(Some))
    }

    fn opt_i32(&self, key: &str) -> Result<Option<i32>> {
        self.attr(key).map_or(Ok(None), |_| self.i32(key).map(Some))
    }

    fn f64(&self, key: &str) -> Result<f64> {
        match self.req(key)? {
            Expression::Number(n) => n.as_f64().ok_or_else(|| self.bad(key, "a number")),
            _ => Err(self.bad(key, "a number")),
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn opt_f32(&self, key: &str) -> Result<Option<f32>> {
        self.attr(key)
            .map_or(Ok(None), |_| self.f64(key).map(|v| Some(v as f32)))
    }

    fn bool(&self, key: &str) -> Result<bool> {
        match self.req(key)? {
            Expression::Bool(b) => Ok(*b),
            _ => Err(self.bad(key, "a boolean")),
        }
    }

    fn opt_bool(&self, key: &str) -> Result<Option<bool>> {
        self.attr(key)
            .map_or(Ok(None), |_| self.bool(key).map(Some))
    }

    fn string_vec(&self, key: &str) -> Result<Vec<String>> {
        let Some(expr) = self.attr(key) else {
            return Ok(Vec::new());
        };
        let Expression::Array(items) = expr else {
            return Err(self.bad(key, "an array of strings"));
        };
        items
            .iter()
            .map(|e| match e {
                Expression::String(s) => Ok(s.clone()),
                _ => Err(self.bad(key, "an array of strings")),
            })
            .collect()
    }

    fn f64_vec(&self, key: &str) -> Result<Vec<f64>> {
        let Some(expr) = self.attr(key) else {
            return Ok(Vec::new());
        };
        let Expression::Array(items) = expr else {
            return Err(self.bad(key, "an array of numbers"));
        };
        items
            .iter()
            .map(|e| match e {
                Expression::Number(n) => n
                    .as_f64()
                    .ok_or_else(|| self.bad(key, "an array of numbers")),
                _ => Err(self.bad(key, "an array of numbers")),
            })
            .collect()
    }

    fn enum_val<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        let s = self.string(key)?;
        from_variant(&s).ok_or_else(|| self.bad(key, "a known enum value"))
    }

    fn opt_enum<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.attr(key)
            .map_or(Ok(None), |_| self.enum_val(key).map(Some))
    }

    fn enum_vec<T: DeserializeOwned>(&self, key: &str) -> Result<Vec<T>> {
        self.string_vec(key)?
            .iter()
            .map(|s| from_variant(s).ok_or_else(|| self.bad(key, "an array of known enum values")))
            .collect()
    }

    fn byte_size(&self, key: &str) -> Result<ByteSize> {
        self.string(key)?
            .parse()
            .map_err(|_| self.bad(key, "a byte size like \"48GiB\""))
    }

    fn opt_byte_size(&self, key: &str) -> Result<Option<ByteSize>> {
        self.attr(key)
            .map_or(Ok(None), |_| self.byte_size(key).map(Some))
    }

    /// A `sha256` attribute: exactly 64 hex characters (case-insensitive),
    /// normalized to lowercase. Hand-validated rather than a plain
    /// pass-through (amicu-planning#151) - it must be a well-formed digest
    /// before it is ever compared against a downloaded artifact's checksum
    /// (see `amicu_llamacpp::download::download_gguf`).
    fn opt_sha256(&self, key: &str) -> Result<Option<String>> {
        self.opt_string(key)?
            .map(|s| {
                if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    Ok(s.to_lowercase())
                } else {
                    Err(self.bad(key, "64 hex characters (a sha256 digest)"))
                }
            })
            .transpose()
    }

    /// A `build` attribute: non-empty and containing no whitespace (a build
    /// tag like `"b4500"` or a git sha, amicu-planning#151). Stored/validated
    /// only - not yet rendered or enforced.
    fn opt_build(&self, key: &str) -> Result<Option<String>> {
        self.opt_string(key)?
            .map(|s| {
                if s.is_empty() || s.chars().any(char::is_whitespace) {
                    Err(self.bad(key, "non-empty with no whitespace"))
                } else {
                    Ok(s)
                }
            })
            .transpose()
    }

    fn reference(&self, key: &str) -> Result<String> {
        reference_id(self.req(key)?).ok_or_else(|| self.bad(key, "a resource reference"))
    }

    fn opt_reference(&self, key: &str) -> Result<Option<String>> {
        self.attr(key)
            .map_or(Ok(None), |_| self.reference(key).map(Some))
    }

    fn reference_vec(&self, key: &str) -> Result<Vec<String>> {
        let Some(expr) = self.attr(key) else {
            return Ok(Vec::new());
        };
        let Expression::Array(items) = expr else {
            return Err(self.bad(key, "an array of references"));
        };
        items
            .iter()
            .map(|e| reference_id(e).ok_or_else(|| self.bad(key, "an array of references")))
            .collect()
    }

    /// A secret-designated attribute: only a `secret("<ref>")` function-call
    /// expression is accepted (amicu-planning#119). A raw string literal, or
    /// anything else, is a hard parse-time rejection with
    /// `AMICU-POLICY-004` - not a heuristic, since the field is
    /// schema-directed. See [`secret_ref_from_expr`].
    fn secret_ref(&self, key: &str) -> Result<SecretRef> {
        let expr = self.req(key)?;
        secret_ref_from_expr(expr).ok_or_else(|| self.secret_literal(key))
    }

    fn opt_secret_ref(&self, key: &str) -> Result<Option<SecretRef>> {
        self.attr(key)
            .map_or(Ok(None), |_| self.secret_ref(key).map(Some))
    }

    fn secret_literal(&self, key: &str) -> HclError {
        HclError::secret_literal(self.ctx.clone(), key)
    }

    /// A nested block whose every attribute value must be a `secret(...)`
    /// reference, e.g. `env { HF_TOKEN = secret("vault://...") }`. Used for
    /// credential-bearing `env` maps (amicu-planning#119): every *value* is
    /// secret-designated the same way a bare `api_key`/`token`/`password`
    /// attribute is.
    fn secret_map(&self, key: &str) -> Result<BTreeMap<String, SecretRef>> {
        let Some(block) = self.opt_block(key) else {
            return Ok(BTreeMap::new());
        };
        block
            .body
            .attributes()
            .map(|a| {
                block.mark(a.key());
                let v =
                    secret_ref_from_expr(a.expr()).ok_or_else(|| block.secret_literal(a.key()))?;
                Ok((a.key().to_string(), v))
            })
            .collect()
    }

    fn check_nested(&self, key: &str, matching: &[&hcl::Block], singleton: bool) {
        let mut problems = self.registry.problems.borrow_mut();
        if singleton && matching.len() > 1 {
            problems.push(format!(
                "{}: duplicate `{key}` block (only one is allowed)",
                self.ctx
            ));
        }
        if matching.iter().any(|b| !b.labels().is_empty()) {
            problems.push(format!("{}: `{key}` block takes no labels", self.ctx));
        }
    }

    fn opt_block(&self, key: &str) -> Option<Fields<'a>> {
        self.mark(key);
        let all: Vec<&hcl::Block> = self
            .body
            .blocks()
            .filter(|b| b.identifier() == key)
            .collect();
        self.check_nested(key, &all, true);
        self.body
            .blocks()
            .find(|b| b.identifier() == key)
            .map(|b| Fields::new(b.body(), format!("{}.{key}", self.ctx), &self.registry))
    }

    fn block(&self, key: &str) -> Result<Fields<'a>> {
        self.opt_block(key).ok_or_else(|| self.missing(key))
    }

    fn blocks(&self, key: &str) -> Vec<Fields<'a>> {
        self.mark(key);
        let all: Vec<&hcl::Block> = self
            .body
            .blocks()
            .filter(|b| b.identifier() == key)
            .collect();
        self.check_nested(key, &all, false);
        self.body
            .blocks()
            .filter(|b| b.identifier() == key)
            .map(|b| Fields::new(b.body(), format!("{}.{key}", self.ctx), &self.registry))
            .collect()
    }

    fn string_map(&self) -> Result<BTreeMap<String, String>> {
        self.body
            .attributes()
            .map(|a| match a.expr() {
                Expression::String(s) => {
                    // Free-form map: every key is intentionally accepted.
                    self.mark(a.key());
                    Ok((a.key().to_string(), s.clone()))
                }
                _ => Err(self.bad(a.key(), "a string")),
            })
            .collect()
    }
}

/// Deserialize a `snake_case` enum variant from its DSL spelling.
fn from_variant<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// Extract a `secret("<ref>")` reference expression (amicu-planning#119).
///
/// Only a single-argument call to the bare, unnamespaced function `secret`
/// with a string-literal argument counts; anything else - including a raw
/// string literal, a different function, or a namespaced call - returns
/// `None` so the caller reports the field as a rejected literal. This is the
/// one grammar extension the structural enforcement relies on: it fits the
/// existing HCL expression grammar (`hcl::Expression::FuncCall`) with no new
/// parser changes.
///
/// The syntax check alone is not sufficient: `secret("sk-live-...")` has the
/// right *shape* even though the argument is a raw secret, not a reference.
/// [`crate::secret_scan::looks_like_secret_value`] runs the same
/// high-confidence detectors the free-text warning uses against the
/// argument itself and also returns `None` (hard reject, same
/// `AMICU-POLICY-004` path) when it matches, closing that smuggling route
/// without flagging ordinary reference names (`vault://kv/...`, `HF_TOKEN`).
fn secret_ref_from_expr(expr: &Expression) -> Option<SecretRef> {
    let Expression::FuncCall(call) = expr else {
        return None;
    };
    if call.name.is_namespaced() || call.name.name.as_str() != "secret" {
        return None;
    }
    match call.args.as_slice() {
        [Expression::String(reference)]
            if !reference.is_empty() && !crate::secret_scan::looks_like_secret_value(reference) =>
        {
            Some(SecretRef::new(reference.clone()))
        }
        _ => None,
    }
}

/// Extract the referenced id from either a quoted string or a traversal such
/// as `model.support_chat.id` (the id is the block name, `support_chat`).
fn reference_id(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(s.clone()),
        Expression::Traversal(t) => match t.operators.first() {
            Some(TraversalOperator::GetAttr(id)) => Some(id.as_str().to_string()),
            _ => None,
        },
        _ => None,
    }
}
