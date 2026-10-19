use serde::{Deserialize, Serialize};

use super::gpu::{GpuCard, GpuResolveError, resolve_gpu};
use super::{Money, Region};
use crate::id::HardwareProfileId;
use crate::units::ByteSize;

/// A hardware target: provider, accelerator/CPU, memory, interconnect,
/// region and price metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HardwareProfile {
    pub id: HardwareProfileId,
    /// Cloud or infra provider, e.g. `"aws"`, `"lambda"`, `"on_prem"`.
    pub provider: String,
    /// The region the hardware sits in. Parsed from the DSL as a bare code with
    /// residency unresolved (see [`Region::from_code`]); the jurisdiction is
    /// resolved against the provider catalog at check time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accelerator: Option<Accelerator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Cpu>,
    /// System memory available to the runtime.
    pub memory: ByteSize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interconnect: Option<Interconnect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    /// `provider = "runpod"` only: how the deployment runs there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runpod: Option<RunpodSettings>,
}

impl HardwareProfile {
    /// The RunPod mode this profile selects (`Serverless` unless a `runpod`
    /// block says otherwise).
    #[must_use]
    pub fn runpod_mode(&self) -> RunpodMode {
        self.runpod.map(|r| r.mode).unwrap_or_default()
    }
}

/// The `runpod { mode, cloud }` block of a hardware profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RunpodSettings {
    #[serde(default)]
    pub mode: RunpodMode,
    /// Pod mode only; `None` means [`RunpodCloud::Secure`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<RunpodCloud>,
}

/// A load-balancing serverless endpoint (scales to zero) or an always-on pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunpodMode {
    #[default]
    Serverless,
    Pod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunpodCloud {
    #[default]
    Secure,
    Community,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accelerator {
    pub kind: AcceleratorKind,
    /// Accelerator model, e.g. `"h100-sxm-80gb"` (see [`resolve_gpu`]), `"m3_max"`.
    pub model: String,
    pub count: u32,
    /// Memory per accelerator.
    pub memory: ByteSize,
    /// Memory bandwidth of a single accelerator, in GB/s. Forward-looking:
    /// recorded for catalog/planning purposes but not yet read by the fit
    /// check (`amicu_registry::fit`), which evaluates capacity only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bandwidth_gbps: Option<u32>,
}

impl Accelerator {
    /// `None` for a non-GPU accelerator.
    #[must_use]
    pub fn gpu_card(&self) -> Option<Result<&'static GpuCard, GpuResolveError>> {
        (self.kind == AcceleratorKind::Gpu).then(|| resolve_gpu(&self.model, Some(self.memory)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceleratorKind {
    Gpu,
    Tpu,
    AppleSilicon,
    Cpu,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cpu {
    pub model: String,
    pub cores: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interconnect {
    Nvlink,
    Pcie,
    Infiniband,
    Ethernet,
}

/// Rental price metadata for a hardware profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub per_hour: Money,
}
