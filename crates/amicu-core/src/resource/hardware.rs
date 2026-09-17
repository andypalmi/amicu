use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accelerator {
    pub kind: AcceleratorKind,
    /// Accelerator model, e.g. `"h100"`, `"m3_max"`.
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
