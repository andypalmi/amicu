//! Plain-data capability descriptor for a runtime family.

use amicu_core::resource::{ArtifactFormat, Modality, Quantization, Runtime};
use serde::{Deserialize, Serialize};

/// The serving-runtime family an adapter implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    LlamaCpp,
    Vllm,
}

impl RuntimeKind {
    /// The runtime family a deployment's settings select.
    #[must_use]
    pub fn of(runtime: &Runtime) -> Self {
        match runtime {
            Runtime::LlamaCpp(_) => Self::LlamaCpp,
            Runtime::Vllm(_) => Self::Vllm,
        }
    }

    /// The environment variable this runtime reads its endpoint API key from
    /// (amicu-planning#191). Verified against upstream: llama-server's
    /// `--api-key` is `set_env("LLAMA_API_KEY")` (`common/arg.cpp`), vLLM's is
    /// `VLLM_API_KEY` (`vllm/envs.py`). amicu sets it, from the resolved
    /// secret, in the process/container environment - never on argv.
    #[must_use]
    pub const fn api_key_env_var(self) -> &'static str {
        match self {
            Self::LlamaCpp => "LLAMA_API_KEY",
            Self::Vllm => "VLLM_API_KEY",
        }
    }

    /// Stable lowercase token, matching the DSL/JSON discriminator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama_cpp",
            Self::Vllm => "vllm",
        }
    }
}

impl std::fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Tensor/pipeline parallelism a runtime can spread a model across.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parallelism {
    /// Whether the runtime can shard a single layer across GPUs.
    pub tensor: bool,
    /// Whether the runtime can split layers into pipeline stages.
    pub pipeline: bool,
    /// The largest number of accelerators the runtime addresses.
    pub max_gpus: u32,
}

/// An execution platform a runtime can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Cpu,
    Cuda,
    Rocm,
    Metal,
}

/// Side-effect-free descriptor of what a runtime adapter can serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    /// The runtime family this descriptor belongs to.
    pub runtime: RuntimeKind,
    /// Artifact formats the runtime can load.
    pub artifact_formats: Vec<ArtifactFormat>,
    /// Execution platforms the runtime supports.
    pub platforms: Vec<Platform>,
    /// Parallelism the runtime offers.
    pub parallelism: Parallelism,
    /// Quantizations the runtime can run.
    pub quantizations: Vec<Quantization>,
    /// Whether the runtime can attach LoRA adapters.
    pub lora: bool,
    /// Whether the runtime exposes tool/function calling.
    pub tool_calling: bool,
    /// Input/output modalities beyond text the runtime handles.
    pub multimodal: Vec<Modality>,
}
