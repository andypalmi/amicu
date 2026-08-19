//! Cloud/external/secret adapters.

pub mod azure;
pub mod capability;
pub mod cost;
pub mod fake;
pub mod hetzner;
pub mod ovhcloud;
pub mod provider;
pub mod runpod;
pub mod scaleway;
pub mod secret;
pub mod self_hosted;
pub mod sync;

pub use azure::AzureProvider;
pub use cost::{CostError, Usage, estimate_cost};
pub use fake::FakeProvider;
pub use hetzner::HetznerProvider;
pub use ovhcloud::OvhCloudProvider;
pub use provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};
pub use runpod::RunPodProvider;
pub use scaleway::ScalewayProvider;
pub use self_hosted::SelfHostedProvider;
pub use sync::{Freshness, SyncReport, SyncedCapabilities, classify, snapshot, sync_all};
