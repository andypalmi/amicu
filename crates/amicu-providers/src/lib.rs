//! Cloud/external/secret adapters.

pub mod capability;
pub mod cost;
pub mod fake;
pub mod ovhcloud;
pub mod provider;
pub mod runpod;
pub mod scaleway;
pub mod secret;

pub use cost::{CostError, Usage, estimate_cost};
pub use fake::FakeProvider;
pub use ovhcloud::OvhCloudProvider;
pub use provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};
pub use runpod::RunPodProvider;
pub use scaleway::ScalewayProvider;
