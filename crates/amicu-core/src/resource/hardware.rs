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
    /// `provider = "gcp"` only: where and how the deployment runs there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gcp: Option<GcpSettings>,
}

impl HardwareProfile {
    /// The RunPod mode this profile selects (`Serverless` unless a `runpod`
    /// block says otherwise).
    #[must_use]
    pub fn runpod_mode(&self) -> RunpodMode {
        self.runpod.as_ref().map(|r| r.mode).unwrap_or_default()
    }
}

/// The `gcp {}` block of a hardware profile. Every field but `project` is
/// optional; a missing `project` is reported as a diagnostic, not a parse error.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GcpSettings {
    /// Google Cloud project id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub backend: GcpBackend,
    /// `None`: the first catalog zone of the profile's region that offers the GPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// `None`: the smallest catalog machine type for the GPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_type: Option<String>,
    #[serde(default)]
    pub provisioning: GcpProvisioning,
    /// The VM's lifetime backstop, in seconds; `None` means 8 hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_duration_secs: Option<u32>,
    /// Shut the VM down after this long without a request (opt-in); `None`
    /// keeps it until `max_run_duration`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_shutdown_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_disk_gb: Option<u32>,
    /// Runtime service account email for the VM. It reads the VM's secrets
    /// from Secret Manager at boot, so it is required unless
    /// [`Self::insecure_metadata_secrets`] is set; `None` then attaches none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account: Option<String>,
    /// INSECURE opt-in: deliver the API key, Hugging Face token and TLS key as
    /// instance metadata instead of through Secret Manager. Anyone who can
    /// read the instance can read them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure_metadata_secrets: bool,
    /// Who may reach the serving port. Empty: the caller's public IP, `/32`,
    /// detected at apply (never `0.0.0.0/0` unless listed here).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_source_cidrs: Vec<String>,
    /// Service account amicu impersonates for its own API calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impersonate_service_account: Option<String>,
    /// Cloud Run only: who may call the service. `None` is
    /// [`GcpAccess::Public`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<GcpAccess>,
}

impl GcpSettings {
    /// [`Self::max_run_duration_secs`] or the 8 hour default.
    #[must_use]
    pub fn max_run_secs(&self) -> u32 {
        self.max_run_duration_secs.unwrap_or(8 * 3600)
    }
}

/// Which Google Cloud service runs the deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcpBackend {
    /// A Compute Engine VM.
    #[default]
    Compute,
    /// A Cloud Run service with one GPU per instance, scaled to zero.
    CloudRun,
}

/// Who may call a Cloud Run service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcpAccess {
    /// Anyone on the internet; the deployment's `api_key` is the only guard.
    #[default]
    Public,
    /// Cloud Run's IAM check stays on: callers also send a Google identity
    /// token with `roles/run.invoker`.
    Iam,
}

/// How a Compute Engine VM is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcpProvisioning {
    #[default]
    Spot,
    FlexStart,
    Standard,
}

/// The `runpod {}` block of a hardware profile.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RunpodSettings {
    #[serde(default)]
    pub mode: RunpodMode,
    /// Pod mode only; `None` means [`RunpodCloud::Secure`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<RunpodCloud>,
    /// Id of an existing RunPod network volume that holds the model cache.
    /// amicu never creates or deletes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_volume: Option<String>,
    /// Serverless mode only; `None` means off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flashboot: Option<bool>,
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
