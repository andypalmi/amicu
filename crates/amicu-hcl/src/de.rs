//! HCL text -> canonical [`Graph`].

use std::collections::BTreeMap;

use hcl::expr::TraversalOperator;
use hcl::{Body, Expression};
use serde::de::DeserializeOwned;

use amicu_core::graph::Graph;
use amicu_core::resource::{
    Accelerator, Artifact, Budget, Cpu, Credential, DataClass, Deployment, GgufVariant,
    GovernanceRule, HardwareProfile, LlamaCppSettings, Model, ModelSource, Money, Price, Quota,
    Region, Residency, RoutingPolicy, RoutingTarget, Runtime, Tenant, UseCase, VllmSettings,
};
use amicu_core::secret::SecretRef;
use amicu_core::units::ByteSize;

use crate::error::HclError;

type Result<T> = std::result::Result<T, HclError>;

/// Parse HCL source into the canonical resource graph.
pub fn parse_graph(src: &str) -> Result<Graph> {
    let body = hcl::parse(src)?;
    let mut graph = Graph::new();

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
        let f = Fields::new(block.body(), format!("{kind} {name}"));

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
                })
            })
            .collect::<Result<_>>()?,
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
                extra_args: s.string_vec("extra_args")?,
                env: match s.opt_block("env") {
                    Some(e) => e.string_map()?,
                    None => BTreeMap::new(),
                },
            })
        }
        "vllm" => {
            let s = f.block("vllm")?;
            Runtime::Vllm(VllmSettings {
                context_length: s.u32("context_length")?,
                port: s.u16("port")?,
                tensor_parallel_size: s.opt_u32("tensor_parallel_size")?,
                gpu_memory_utilization: s.opt_f32("gpu_memory_utilization")?,
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
        id: name.into(),
        model: f.reference("model")?.into(),
        hardware_profile: f.reference("hardware")?.into(),
        runtime,
    })
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
}

impl<'a> Fields<'a> {
    fn new(body: &'a Body, ctx: String) -> Self {
        Self { body, ctx }
    }

    fn missing(&self, key: &str) -> HclError {
        HclError::schema(format!("{}: missing `{key}`", self.ctx))
    }

    fn bad(&self, key: &str, want: &str) -> HclError {
        HclError::schema(format!("{}: `{key}` must be {want}", self.ctx))
    }

    fn attr(&self, key: &str) -> Option<&Expression> {
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
                let v =
                    secret_ref_from_expr(a.expr()).ok_or_else(|| block.secret_literal(a.key()))?;
                Ok((a.key().to_string(), v))
            })
            .collect()
    }

    fn opt_block(&self, key: &str) -> Option<Fields<'a>> {
        self.body
            .blocks()
            .find(|b| b.identifier() == key)
            .map(|b| Fields::new(b.body(), format!("{}.{key}", self.ctx)))
    }

    fn block(&self, key: &str) -> Result<Fields<'a>> {
        self.opt_block(key).ok_or_else(|| self.missing(key))
    }

    fn blocks(&self, key: &str) -> Vec<Fields<'a>> {
        self.body
            .blocks()
            .filter(|b| b.identifier() == key)
            .map(|b| Fields::new(b.body(), format!("{}.{key}", self.ctx)))
            .collect()
    }

    fn string_map(&self) -> Result<BTreeMap<String, String>> {
        self.body
            .attributes()
            .map(|a| match a.expr() {
                Expression::String(s) => Ok((a.key().to_string(), s.clone())),
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
