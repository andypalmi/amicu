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
/// No repo doc lists llama-server's accepted values as of this writing, so
/// this is the four-value set `common/arg.cpp` documents for
/// `--reasoning-format`: `none` (leave reasoning inline), `deepseek`
/// (current DeepSeek-R1-style extraction), `deepseek_legacy` (the older
/// extraction behavior kept for compatibility), `auto` (runtime-detected).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningFormat {
    None,
    Deepseek,
    DeepseekLegacy,
    Auto,
}

impl ReasoningFormat {
    /// The exact spelling `llama-server` accepts for `--reasoning-format`
    /// (matches this type's `snake_case` serde form).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ReasoningFormat::None => "none",
            ReasoningFormat::Deepseek => "deepseek",
            ReasoningFormat::DeepseekLegacy => "deepseek_legacy",
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
    /// Enable Jinja chat-template rendering (`--jinja`, amicu-planning#151):
    /// a bare flag, not a value-having one. `Some(true)` renders `--jinja`;
    /// `Some(false)` and `None` both render nothing, since llama-server has
    /// no `--no-jinja` to explicitly disable it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jinja: Option<bool>,
    /// Reasoning/thinking-block extraction format (`--reasoning-format`,
    /// amicu-planning#151).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_format: Option<ReasoningFormat>,
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
