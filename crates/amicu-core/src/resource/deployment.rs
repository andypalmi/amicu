use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::id::{DeploymentId, HardwareProfileId, ModelId};

/// KV-cache quantization dtype for llama.cpp's `--cache-type-k`/`--cache-type-v`,
/// spelled exactly as `llama-server` accepts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KvCacheType {
    F16,
    BF16,
    Q8_0,
    Q4_0,
    Q4_1,
    Q5_0,
    Q5_1,
    #[serde(rename = "iq4_nl")]
    Iq4Nl,
}

impl KvCacheType {
    /// Bytes per cached element (K or V) at this dtype.
    ///
    /// Basis: llama.cpp's `ggml` quantized-block layouts (`ggml/src/ggml-quants.c`,
    /// block size 32 elements per block) - `f16`/`bf16` are plain 2-byte scalars;
    /// the k-quant KV types pack 32 elements into a fixed-size block:
    /// `q8_0` = 34 B/32 elems, `q4_0` = 18 B/32, `q4_1` = 20 B/32, `q5_0` =
    /// 22 B/32, `q5_1` = 24 B/32, `iq4_nl` = 18 B/32. These are approximate
    /// per-element costs (`block_bytes / 32`), not a claim that llama.cpp
    /// stores fractional bytes.
    #[must_use]
    pub fn bytes_per_element(self) -> f64 {
        match self {
            KvCacheType::F16 | KvCacheType::BF16 => 2.0,
            KvCacheType::Q8_0 => 34.0 / 32.0,
            // q4_0 and iq4_nl share the same 18-byte/32-element block size.
            KvCacheType::Q4_0 | KvCacheType::Iq4Nl => 18.0 / 32.0,
            KvCacheType::Q4_1 => 20.0 / 32.0,
            KvCacheType::Q5_0 => 22.0 / 32.0,
            KvCacheType::Q5_1 => 24.0 / 32.0,
        }
    }

    /// The exact spelling `llama-server` accepts for `--cache-type-k`/`-v`
    /// (matches this type's `lowercase` serde form).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            KvCacheType::F16 => "f16",
            KvCacheType::BF16 => "bf16",
            KvCacheType::Q8_0 => "q8_0",
            KvCacheType::Q4_0 => "q4_0",
            KvCacheType::Q4_1 => "q4_1",
            KvCacheType::Q5_0 => "q5_0",
            KvCacheType::Q5_1 => "q5_1",
            KvCacheType::Iq4Nl => "iq4_nl",
        }
    }
}

impl std::fmt::Display for KvCacheType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `llama-server`'s `--flash-attn` value: a three-way toggle, not a bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlashAttention {
    On,
    Off,
    Auto,
}

impl FlashAttention {
    /// The exact spelling `llama-server` accepts for `--flash-attn`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FlashAttention::On => "on",
            FlashAttention::Off => "off",
            FlashAttention::Auto => "auto",
        }
    }
}

impl std::fmt::Display for FlashAttention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `llama-server`'s `--reasoning-format` value: how reasoning/thinking
/// blocks in the model's output are extracted and surfaced (amicu-planning#151).
/// The four-value set `common/chat.cpp`'s `common_reasoning_format_name`/
/// `common_reasoning_format_from_name` define: `none` (leave reasoning
/// inline), `deepseek` (current DeepSeek-R1-style extraction, in
/// `message.reasoning_content`), `deepseek_legacy` (the older extraction
/// behavior, `<think>` tags kept in `message.content` while also populating
/// `message.reasoning_content`), `auto` (runtime-detected, the CLI default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningFormat {
    None,
    Deepseek,
    DeepseekLegacy,
    Auto,
}

impl ReasoningFormat {
    /// The exact spelling `llama-server` accepts for `--reasoning-format`.
    /// Matches this type's `snake_case` serde/HCL form for every variant
    /// except [`ReasoningFormat::DeepseekLegacy`]: `common_reasoning_format_name`
    /// (`common/chat.cpp`) returns the CLI/argv value `"deepseek-legacy"`
    /// (hyphenated) for it, not `"deepseek_legacy"` - the HCL schema value
    /// stays underscored (matching every other `snake_case` enum in this
    /// crate), but the rendered argv token must be the hyphenated spelling
    /// or `llama-server` rejects it with "Unknown reasoning format".
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ReasoningFormat::None => "none",
            ReasoningFormat::Deepseek => "deepseek",
            ReasoningFormat::DeepseekLegacy => "deepseek-legacy",
            ReasoningFormat::Auto => "auto",
        }
    }
}

impl std::fmt::Display for ReasoningFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// vLLM's `--kv-cache-dtype` value set, distinct from [`KvCacheType`] (vLLM's
/// spellings and options do not match llama.cpp's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VllmKvCacheDtype {
    Auto,
    Fp8,
    Fp8E5M2,
    Fp8E4M3,
}

impl VllmKvCacheDtype {
    /// The exact spelling vLLM accepts for `--kv-cache-dtype`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            VllmKvCacheDtype::Auto => "auto",
            VllmKvCacheDtype::Fp8 => "fp8",
            VllmKvCacheDtype::Fp8E5M2 => "fp8_e5m2",
            VllmKvCacheDtype::Fp8E4M3 => "fp8_e4m3",
        }
    }
}

impl std::fmt::Display for VllmKvCacheDtype {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Speculative-decoding configuration for a llama.cpp deployment
/// (amicu-planning#151 PR C).
///
/// The draft model is a *reference to a second `model` resource*, not a path:
/// it is a real graph edge (checked by
/// [`crate::graph::Graph::missing_references`], resolved through the same
/// catalog the primary [`Deployment::model`] goes through), so the draft
/// GGUF's file path and weight size are derived from the registry exactly like
/// the primary model's are.
///
/// ## Upstream flag spellings
///
/// Verified against `common/arg.cpp` and `common/common.h` on
/// `ggml-org/llama.cpp@master`. llama.cpp renamed the speculative flag family
/// to a `--spec-draft-*` prefix and kept the older spellings as aliases on the
/// same `add_opt` - *except* `--draft`/`--draft-n`/`--draft-max` and
/// `--draft-min`/`--draft-n-min`, which were **removed** and now abort with
/// "the argument has been removed. use --spec-draft-n-max ...". So the
/// renderer emits, per field, the spelling the widest range of `llama-server`
/// builds accepts, preferring the current one wherever the legacy spelling
/// would hard-error:
///
/// | field | rendered flag | aliases | env |
/// |---|---|---|---|
/// | `model` | `--model-draft` | `--spec-draft-model`, `-md` | `LLAMA_ARG_SPEC_DRAFT_MODEL` |
/// | `n_max` | `--spec-draft-n-max` | *(legacy `--draft-max` removed)* | `LLAMA_ARG_SPEC_DRAFT_N_MAX` |
/// | `n_min` | `--spec-draft-n-min` | *(legacy `--draft-min` removed)* | `LLAMA_ARG_SPEC_DRAFT_N_MIN` |
/// | `p_min` | `--draft-p-min` | `--spec-draft-p-min` | `LLAMA_ARG_SPEC_DRAFT_P_MIN` |
/// | `n_gpu_layers` | `--n-gpu-layers-draft` | `--spec-draft-ngl`, `--gpu-layers-draft`, `-ngld` | `LLAMA_ARG_N_GPU_LAYERS_DRAFT` |
///
/// There is deliberately **no draft context-size field**: current upstream
/// exposes no draft context-size flag at all
/// (`common_params_speculative_draft` in `common/common.h` has no `n_ctx`
/// member, and `common/arg.cpp` registers no
/// `--ctx-size-draft`/`-cd`/`--spec-draft-c` option), so a typed field for it
/// could only render a flag `llama-server` would reject.
///
/// `--spec-type` is likewise not rendered: with the type left at its default,
/// `common/arg.cpp` infers the speculative type from the draft GGUF's own
/// metadata whenever a draft path is set, so passing the draft model alone is
/// how draft-model speculative decoding is turned on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeculativeSettings {
    /// The draft model: a reference to a second `model` resource. Must not be
    /// the deployment's own primary model (`AMICU-CAP-007`).
    pub model: ModelId,
    /// Maximum number of tokens drafted per step (`--spec-draft-n-max`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_max: Option<u32>,
    /// Minimum number of draft tokens to use (`--spec-draft-n-min`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_min: Option<u32>,
    /// Minimum draft probability, below which drafting stops
    /// (`--draft-p-min`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p_min: Option<f32>,
    /// Draft-model layers offloaded to the accelerator
    /// (`--n-gpu-layers-draft`); `-1` is llama-server's own "auto" default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_gpu_layers: Option<i32>,
}

/// Binds a model to a hardware profile with runtime-specific settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deployment {
    pub id: DeploymentId,
    pub model: ModelId,
    pub hardware_profile: HardwareProfileId,
    #[serde(flatten)]
    pub runtime: Runtime,
}

/// The serving runtime and its configuration. Serialized inline with a
/// `runtime` discriminator, e.g. `{ "runtime": "llama_cpp", ... }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "runtime", rename_all = "snake_case")]
pub enum Runtime {
    LlamaCpp(LlamaCppSettings),
    Vllm(VllmSettings),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlamaCppSettings {
    pub context_length: u32,
    pub port: u16,
    /// Layers offloaded to the accelerator; `-1` offloads all.
    pub n_gpu_layers: i32,
    /// Sampling temperature (`--temp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus sampling cutoff (`--top-p`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Top-k sampling cutoff (`--top-k`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Minimum-probability sampling cutoff (`--min-p`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f32>,
    /// Repetition penalty (`--repeat-penalty`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f32>,
    /// KV-cache dtype for the K tensor (`--cache-type-k`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_type_k: Option<KvCacheType>,
    /// KV-cache dtype for the V tensor (`--cache-type-v`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_type_v: Option<KvCacheType>,
    /// Logical batch size for prompt processing (`--batch-size`/`-b`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<u32>,
    /// Physical (micro-)batch size (`--ubatch-size`/`-ub`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ubatch_size: Option<u32>,
    /// Number of concurrent request slots (`--parallel`/`-np`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_slots: Option<u32>,
    /// Flash-attention mode (`--flash-attn`); a three-way toggle, not a bool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flash_attention: Option<FlashAttention>,
    /// Inline Jinja chat template text (`--chat-template`, amicu-planning#151).
    /// Mutually exclusive with [`LlamaCppSettings::chat_template_file`] -
    /// setting both is `AMICU-CAP-005` and renders neither flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template: Option<String>,
    /// Path to a Jinja chat template file (`--chat-template-file`,
    /// amicu-planning#151). See [`LlamaCppSettings::chat_template`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_file: Option<String>,
    /// Enable Jinja chat-template rendering (`--jinja`/`--no-jinja`,
    /// amicu-planning#151): a paired bare-flag toggle (`common/arg.cpp`'s
    /// `add_opt` for these registers both spellings under one option), not a
    /// value-having flag. `Some(true)` renders `--jinja`; `Some(false)`
    /// renders `--no-jinja` (so an explicit `false` here overrides an
    /// inherited `LLAMA_ARG_JINJA=1` from the environment rather than just
    /// omitting the flag); `None` renders nothing, leaving llama-server's own
    /// default/environment in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jinja: Option<bool>,
    /// Reasoning/thinking-block extraction format (`--reasoning-format`,
    /// amicu-planning#151).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_format: Option<ReasoningFormat>,
    /// Speculative decoding with a draft model (amicu-planning#151 PR C).
    /// `None` - the default - is today's behavior exactly: no draft model is
    /// resolved, no `--model-draft` is rendered, and the fit check counts only
    /// the primary model's weights. See [`SpeculativeSettings`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speculative: Option<SpeculativeSettings>,
    /// Raw `llama-server` flags appended verbatim after the typed flags
    /// (amicu-planning#150): the escape hatch for the ~98% of the flag
    /// surface with no typed field yet. Declaration order is preserved -
    /// it is argv order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// Environment variables applied to the spawned process, merged with
    /// (and never overriding) resolved secrets (amicu-planning#150).
    /// `BTreeMap` for deterministic HCL round-trip, like `Tenant.labels`
    /// and `Credential.env`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Runtime build pin (e.g. `"b4500"` or a git sha, amicu-planning#151).
    /// Stored/validated only; not yet rendered or enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Vision/audio multimodal projector settings (`multimodal { ... }`,
    /// amicu-planning#151 PR D): backs the `Modality::Vision` capability this
    /// adapter already declares (`crate::adapter::LlamaCppAdapter::capabilities`,
    /// in `amicu-llamacpp`) with an actual mmproj file to load. llama.cpp-only:
    /// a `multimodal` block on a `vllm {}` settings block is a schema error
    /// (`amicu_hcl::de`), not a field that exists on [`VllmSettings`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multimodal: Option<MultimodalSettings>,
}

/// `llama-server`'s multimodal projector settings (`--mmproj`/
/// `--mmproj-offload`, amicu-planning#151 PR D): both fields optional and
/// independent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MultimodalSettings {
    /// Path or HF-style reference to the multimodal projector GGUF
    /// (`--mmproj`/`-mm`). A plain string in this PR - not validated for
    /// existence at plan time (apply time surfaces a load failure), and not
    /// a second `model` resource reference; that form can come later if
    /// amicu needs to track the projector as its own downloadable artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projector: Option<String>,
    /// Whether to offload the projector to the accelerator
    /// (`--mmproj-offload`/`--no-mmproj-offload`; llama-server defaults this
    /// to enabled). `Some(true)`/`Some(false)` render the matching bare
    /// flag explicitly (like [`LlamaCppSettings::jinja`]); `None` renders
    /// nothing, leaving llama-server's own default/environment in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offload: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VllmSettings {
    pub context_length: u32,
    pub port: u16,
    /// Number of GPUs to shard the model across.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tensor_parallel_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_utilization: Option<f32>,
    /// KV-cache dtype (`--kv-cache-dtype`); vLLM's own value set (see
    /// [`VllmKvCacheDtype`]), validated at the schema layer to
    /// `auto|fp8|fp8_e5m2|fp8_e4m3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv_cache_dtype: Option<VllmKvCacheDtype>,
    /// Maximum number of concurrent sequences (`--max-num-seqs`), vLLM's
    /// rough equivalent of llama.cpp's `parallel_slots`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_num_seqs: Option<u32>,
    /// Maximum number of batched tokens per iteration
    /// (`--max-num-batched-tokens`), vLLM's rough equivalent of llama.cpp's
    /// `batch_size`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_num_batched_tokens: Option<u32>,
    /// Raw vLLM CLI flags appended verbatim by the vLLM renderer
    /// (amicu-planning#150; the renderer itself ships separately). See
    /// [`LlamaCppSettings::extra_args`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// Environment variables applied to the spawned process. See
    /// [`LlamaCppSettings::env`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}
