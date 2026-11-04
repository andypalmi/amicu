//! Where a model's KV-cache cost comes from, and a small built-in table of
//! known costs for models the head formula gets wrong.
//!
//! Precedence, per attribute: an explicit `kv_bytes_per_token` /
//! `kv_fixed_bytes` on the model resource, then a built-in [`KnownKvCost`]
//! matched by the model's source, then the standard head formula
//! (`n_layers`/`n_kv_heads`/`head_dim`, see [`crate::fit::kv_cache_bytes`]).
//!
//! A built-in entry describes one serving setup (its cache format and the
//! runtime that serves it). On any other runtime it is withheld: the head
//! formula is known to be wrong for such a model, so the estimate is left out
//! rather than computed from it.

use amicu_core::resource::{Model, Runtime, SourceRegistry};
use amicu_core::units::ByteSize;

/// A known KV-cache cost for one model, as served by vLLM at its native cache
/// format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownKvCost {
    /// Display name, e.g. `"DeepSeek-V4.1-Flash"`.
    pub name: &'static str,
    /// Hugging Face references this entry matches (case-insensitively).
    pub hf_references: &'static [&'static str],
    /// Bytes per token of context, across all layers.
    pub bytes_per_token: ByteSize,
    /// Per-sequence state that does not grow with context.
    pub fixed_bytes: ByteSize,
    /// The cache format the figures are for, e.g. `"FP4"`.
    pub cache_format: &'static str,
}

/// The built-in table. More models will be added; each figure is for the
/// model's native cache format as served by vLLM.
pub const KNOWN_KV_COSTS: &[KnownKvCost] = &[KnownKvCost {
    name: "DeepSeek-V4.1-Flash",
    // The -1017 release ships the same config.json.
    hf_references: &[
        "deepseek-ai/DeepSeek-V4.1-Flash",
        "deepseek-ai/DeepSeek-V4.1-Flash-1017",
    ],
    // Only 4 of 40 layers store KV, compressed 2:1 in the encoder half, at
    // the native FP4 cache.
    bytes_per_token: ByteSize::from_bytes(890),
    // Sliding window: (40 layers + 3 draft layers) x 128 tokens x 528 B.
    fixed_bytes: ByteSize::from_bytes(2_906_112),
    cache_format: "FP4",
}];

/// The built-in entry for `model`, matched by its Hugging Face reference.
#[must_use]
pub fn known_kv_cost(model: &Model) -> Option<&'static KnownKvCost> {
    if model.source.registry != SourceRegistry::HuggingFace {
        return None;
    }
    let reference = model.source.reference.trim();
    KNOWN_KV_COSTS.iter().find(|k| {
        k.hf_references
            .iter()
            .any(|r| r.eq_ignore_ascii_case(reference))
    })
}

/// Where the per-token part of a KV-cache estimate comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvCostSource {
    /// The model resource states `kv_bytes_per_token`.
    Stated(ByteSize),
    /// A built-in entry applies on this runtime.
    BuiltIn(&'static KnownKvCost),
    /// The head formula (needs the model's shape metadata).
    Heads,
    /// A built-in entry exists but does not apply on this runtime, and the
    /// head formula is wrong for the model: no estimate.
    Withheld(&'static KnownKvCost),
}

/// A model's resolved KV-cache cost on one runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvCost {
    /// Source of the part that grows with context.
    pub source: KvCostSource,
    /// Fixed per-sequence state: the model's `kv_fixed_bytes`, else the
    /// built-in entry's when it applies, else zero.
    pub fixed_per_sequence: ByteSize,
    /// Whether [`Self::fixed_per_sequence`] came from the built-in entry.
    pub fixed_is_built_in: bool,
}

impl KvCost {
    /// The per-token byte figure, when stated or built in (`None` for the
    /// head formula or a withheld entry).
    #[must_use]
    pub fn bytes_per_token(&self) -> Option<ByteSize> {
        match self.source {
            KvCostSource::Stated(b) => Some(b),
            KvCostSource::BuiltIn(k) => Some(k.bytes_per_token),
            KvCostSource::Heads | KvCostSource::Withheld(_) => None,
        }
    }

    /// The fixed state for `sequences` concurrent sequences.
    #[must_use]
    pub fn fixed_for(&self, sequences: u32) -> ByteSize {
        ByteSize::from_bytes(
            self.fixed_per_sequence
                .bytes()
                .saturating_mul(u64::from(sequences)),
        )
    }
}

/// The serving runtime a KV-cache cost is resolved for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvRuntime {
    LlamaCpp,
    Vllm,
}

impl From<&Runtime> for KvRuntime {
    fn from(runtime: &Runtime) -> Self {
        match runtime {
            Runtime::LlamaCpp(_) => Self::LlamaCpp,
            Runtime::Vllm(_) => Self::Vllm,
        }
    }
}

/// Resolve `model`'s KV-cache cost on `runtime`, applying the precedence in
/// the module docs. Built-in entries apply only on vLLM.
#[must_use]
pub fn resolve_kv_cost(model: &Model, runtime: KvRuntime) -> KvCost {
    let known = known_kv_cost(model);
    let applies = runtime == KvRuntime::Vllm;
    let source = match (model.kv_bytes_per_token, known) {
        (Some(stated), _) => KvCostSource::Stated(stated),
        (None, Some(k)) if applies => KvCostSource::BuiltIn(k),
        (None, Some(k)) => KvCostSource::Withheld(k),
        (None, None) => KvCostSource::Heads,
    };
    let built_in_fixed = known.filter(|_| applies).map(|k| k.fixed_bytes);
    let (fixed_per_sequence, fixed_is_built_in) = match (model.kv_fixed_bytes, built_in_fixed) {
        (Some(explicit), _) => (explicit, false),
        (None, Some(b)) => (b, true),
        (None, None) => (ByteSize::from_bytes(0), false),
    };
    KvCost {
        source,
        fixed_per_sequence,
        fixed_is_built_in,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::resource::{Artifact, ArtifactFormat, ModelSource};

    fn model(registry: SourceRegistry, reference: &str) -> Model {
        Model {
            id: "m".into(),
            source: ModelSource {
                registry,
                reference: reference.to_owned(),
            },
            architecture: "deepseek".into(),
            modalities: Vec::new(),
            artifact: Artifact {
                format: ArtifactFormat::Safetensors,
                quantization: None,
            },
            context_length: None,
            variants: Vec::new(),
            n_layers: None,
            n_kv_heads: None,
            head_dim: None,
            n_kv_layers: None,
            kv_bytes_per_token: None,
            kv_fixed_bytes: None,
        }
    }

    fn flash() -> Model {
        model(
            SourceRegistry::HuggingFace,
            "deepseek-ai/DeepSeek-V4.1-Flash",
        )
    }

    fn vllm() -> KvRuntime {
        KvRuntime::Vllm
    }

    fn llama() -> KvRuntime {
        KvRuntime::LlamaCpp
    }

    #[test]
    fn matches_the_hugging_face_reference_case_insensitively() {
        let k = known_kv_cost(&model(
            SourceRegistry::HuggingFace,
            "DEEPSEEK-AI/deepseek-v4.1-flash",
        ))
        .expect("entry");
        assert_eq!(k.bytes_per_token.bytes(), 890);
        assert_eq!(k.fixed_bytes.bytes(), 2_906_112);
        assert_eq!(
            known_kv_cost(&model(
                SourceRegistry::HuggingFace,
                "deepseek-ai/DeepSeek-V4.1-Flash-1017",
            ))
            .map(|k| k.name),
            Some("DeepSeek-V4.1-Flash")
        );
        assert_eq!(
            known_kv_cost(&model(
                SourceRegistry::Url,
                "deepseek-ai/DeepSeek-V4.1-Flash"
            )),
            None
        );
        assert_eq!(
            known_kv_cost(&model(
                SourceRegistry::HuggingFace,
                "deepseek-ai/DeepSeek-V4-Flash"
            )),
            None
        );
    }

    #[test]
    fn built_in_entry_applies_on_vllm() {
        let cost = resolve_kv_cost(&flash(), vllm());
        assert!(matches!(cost.source, KvCostSource::BuiltIn(k) if k.name == "DeepSeek-V4.1-Flash"));
        assert_eq!(cost.bytes_per_token(), Some(ByteSize::from_bytes(890)));
        assert_eq!(cost.fixed_per_sequence.bytes(), 2_906_112);
        assert!(cost.fixed_is_built_in);
    }

    #[test]
    fn explicit_values_win_over_the_built_in_entry() {
        let mut m = flash();
        m.kv_bytes_per_token = Some(ByteSize::from_bytes(1000));
        m.kv_fixed_bytes = Some(ByteSize::from_bytes(5));
        let cost = resolve_kv_cost(&m, vllm());
        assert_eq!(
            cost.source,
            KvCostSource::Stated(ByteSize::from_bytes(1000))
        );
        assert_eq!(cost.fixed_per_sequence.bytes(), 5);
        assert!(!cost.fixed_is_built_in);
        // Per attribute: an explicit per-token figure keeps the built-in
        // fixed state.
        m.kv_fixed_bytes = None;
        let cost = resolve_kv_cost(&m, vllm());
        assert_eq!(cost.fixed_per_sequence.bytes(), 2_906_112);
        assert!(cost.fixed_is_built_in);
    }

    #[test]
    fn built_in_entry_is_withheld_on_other_runtimes_without_a_head_fallback() {
        let mut m = flash();
        m.n_layers = Some(40);
        m.n_kv_heads = Some(8);
        m.head_dim = Some(128);
        let cost = resolve_kv_cost(&m, llama());
        assert!(matches!(cost.source, KvCostSource::Withheld(_)));
        assert_eq!(cost.bytes_per_token(), None);
        assert_eq!(cost.fixed_per_sequence.bytes(), 0);
        // An explicit figure is honoured on any runtime.
        m.kv_bytes_per_token = Some(ByteSize::from_bytes(890));
        assert_eq!(
            resolve_kv_cost(&m, llama()).source,
            KvCostSource::Stated(ByteSize::from_bytes(890))
        );
    }

    #[test]
    fn unknown_models_use_the_head_formula() {
        let m = model(SourceRegistry::HuggingFace, "Qwen/Qwen3-8B");
        let cost = resolve_kv_cost(&m, vllm());
        assert_eq!(cost.source, KvCostSource::Heads);
        assert_eq!(cost.fixed_per_sequence.bytes(), 0);
    }
}
