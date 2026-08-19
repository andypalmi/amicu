//! Compute offerings, storage, network, orchestration, provisioning and
//! lifecycle metadata.

use amicu_core::resource::Accelerator;
use amicu_core::units::ByteSize;
use serde::{Deserialize, Serialize};

use super::availability::Availability;
use super::pricing::Pricing;

/// A named compute shape a provider offers: an optional accelerator (GPU / TPU
/// / Apple Silicon), its VRAM and count, host resources, the regions it is
/// offered in, its pricing, and its availability.
///
/// The accelerator model, VRAM and count reuse [`Accelerator`] from
/// `amicu-core` so provider capabilities stay consistent with hardware
/// profiles rather than redefining the same concept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComputeOffering {
    /// Stable offering identifier, e.g. `"gpu.h100.1x"` or `"cpu.large"`.
    pub id: String,
    /// Accelerator (kind, model, count, VRAM); absent for CPU-only shapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accelerator: Option<Accelerator>,
    /// Host vCPU count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u32>,
    /// Host system memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<ByteSize>,
    /// Region codes (see [`super::Region`]) this offering is sold in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
    /// On-demand and spot pricing for this offering.
    pub pricing: Pricing,
    /// Current capacity state for this offering.
    pub availability: Availability,
}

/// A kind of storage a provider offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    /// Network block storage (e.g. attached volumes).
    Block,
    /// Object storage (e.g. S3-compatible buckets).
    Object,
    /// Local SSD attached to the instance.
    LocalSsd,
    /// Local NVMe attached to the instance.
    LocalNvme,
    /// Shared network filesystem.
    NetworkFs,
}

/// A storage option, with an optional capacity bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageOption {
    /// The storage kind.
    pub kind: StorageKind,
    /// Maximum capacity, when bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<ByteSize>,
}

impl StorageOption {
    /// A storage option of the given kind with no capacity bound.
    #[must_use]
    pub fn new(kind: StorageKind) -> Self {
        Self {
            kind,
            capacity: None,
        }
    }

    /// Set the capacity bound.
    #[must_use]
    pub fn with_capacity(mut self, capacity: ByteSize) -> Self {
        self.capacity = Some(capacity);
        self
    }
}

/// Network capabilities of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NetworkCapabilities {
    /// Advertised bandwidth in megabits per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_mbps: Option<u64>,
    /// Whether a private network / VPC is available.
    #[serde(default)]
    pub private_network: bool,
}

/// Cluster orchestration a provider supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OrchestrationSupport {
    /// Managed Kubernetes (EKS / GKE / AKS / Kapsule / k3s-as-a-service).
    #[serde(default)]
    pub managed_kubernetes: bool,
    /// Bare-metal / dedicated servers.
    #[serde(default)]
    pub bare_metal: bool,
}

/// How instances are provisioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisioningMethod {
    /// Provisioned through a provider API.
    Api,
    /// Provisioned over SSH to existing hosts.
    Ssh,
    /// Both an API and SSH provisioning are supported.
    ApiAndSsh,
}

/// The credential model a provider authenticates with.
///
/// This names the *kind* of credential only; no secret material is ever stored
/// in the descriptor. Values are resolved at apply time via the secret manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialsModel {
    /// An API token / bearer token.
    ApiToken,
    /// An API key pair.
    ApiKey,
    /// An SSH key.
    SshKey,
    /// A cloud service account / IAM role.
    ServiceAccount,
    /// No credentials required (e.g. local / self-hosted).
    None,
}

/// Startup and teardown behavior of provisioned instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LifecycleBehavior {
    /// Typical time from provision to ready, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_secs: Option<u32>,
    /// Typical teardown time, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown_secs: Option<u32>,
    /// Whether instances can be stopped / hibernated and resumed.
    #[serde(default)]
    pub supports_hibernation: bool,
}

/// How quickly model artifacts can be pulled onto provider hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelDownloadPerformance {
    /// Typical sustained download throughput, in megabits per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throughput_mbps: Option<u64>,
    /// Whether the provider offers a local registry mirror / model cache.
    #[serde(default)]
    pub cached_registry: bool,
}
