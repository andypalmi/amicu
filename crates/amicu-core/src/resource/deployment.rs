use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::id::{DeploymentId, HardwareProfileId, ModelId};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlamaCppSettings {
    pub context_length: u32,
    pub port: u16,
    /// Layers offloaded to the accelerator; `-1` offloads all.
    pub n_gpu_layers: i32,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VllmSettings {
    pub context_length: u32,
    pub port: u16,
    /// Number of GPUs to shard the model across.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tensor_parallel_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_utilization: Option<f32>,
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
