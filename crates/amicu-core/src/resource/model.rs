use serde::{Deserialize, Serialize};

use super::Modality;
use crate::id::ModelId;
use crate::units::ByteSize;

/// A servable model: where it comes from, its architecture and modalities,
/// its artifact format/quantization, context needs and GGUF variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: ModelId,
    pub source: ModelSource,
    /// Architecture family, e.g. `"llama"`, `"qwen2"`, `"mixtral"`.
    pub architecture: String,
    pub modalities: Vec<Modality>,
    pub artifact: Artifact,
    /// Maximum context length the weights support, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// Available GGUF quantization variants, when the artifact is GGUF.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<GgufVariant>,
    /// Transformer layer count, declared (not derived - there is no GGUF
    /// header parser in-tree). Feeds the KV-cache estimator
    /// (`amicu_registry::fit::kv_cache_bytes`) together with
    /// [`Model::n_kv_heads`] and [`Model::head_dim`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_layers: Option<u32>,
    /// Number of key/value attention heads (post-GQA, if applicable),
    /// declared. See [`Model::n_layers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_kv_heads: Option<u32>,
    /// Per-head dimension, declared. See [`Model::n_layers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_dim: Option<u32>,
    /// Number of layers that hold a KV cache growing with context, declared.
    /// Hybrid-attention models (Qwen3.5 / Qwen3-Next: linear attention on most
    /// layers, full attention on a few) keep a per-token KV cache on the
    /// full-attention layers only; set this below [`Model::n_layers`] so the
    /// estimator does not overcount. Must be positive and, when `n_layers` is
    /// set, at most `n_layers` (enforced when parsing HCL). Unset means every
    /// layer does (a plain transformer); set without `n_layers`, it alone
    /// supplies the layer count - see [`Model::kv_layers`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_kv_layers: Option<u32>,
}

impl Model {
    /// The model id a local llama.cpp server reports in `/v1/models` (passed as
    /// `--alias`), and the `model` the printed usage example sends. The source
    /// reference (`org/model`, a URL), except for a local file or a URL, whose
    /// reference is an absolute path or may carry a token and must not leak into
    /// argv or `/v1/models`: the HCL model id is used then.
    #[must_use]
    pub fn served_name(&self) -> &str {
        match self.source.registry {
            SourceRegistry::Local | SourceRegistry::Url => self.id.as_str(),
            _ => &self.source.reference,
        }
    }

    /// Layer count the KV-cache estimate multiplies by: [`Model::n_kv_layers`]
    /// when declared (hybrid attention), else [`Model::n_layers`].
    #[must_use]
    pub fn kv_layers(&self) -> Option<u32> {
        self.n_kv_layers.or(self.n_layers).filter(|n| *n > 0)
    }
}

/// Where a model's weights are fetched from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSource {
    pub registry: SourceRegistry,
    /// Registry-specific reference, e.g. a HF repo `"org/model"` or a URL.
    pub reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRegistry {
    HuggingFace,
    Ollama,
    Url,
    Local,
}

/// The on-disk weight format and, where applicable, its quantization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub format: ArtifactFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<Quantization>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    Gguf,
    Safetensors,
    Pytorch,
    Onnx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quantization {
    F32,
    F16,
    Bf16,
    Fp8,
    Int8,
    Int4,
    Q4KM,
    Q5KM,
    Q6K,
    Q8_0,
}

/// A concrete GGUF file at a specific quantization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GgufVariant {
    pub quantization: Quantization,
    /// File name within the source repository.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<ByteSize>,
    /// SHA-256 digest of the file, lowercase hex.
    /// Validated (64 hex characters) and normalized to lowercase at HCL
    /// parse time. Verified against the resolved download artifact's
    /// checksum before `download_gguf` fetches bytes, when both the
    /// variant and the caller's expectation are known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}
