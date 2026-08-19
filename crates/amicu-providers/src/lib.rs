//! Cloud/external/secret adapters.

pub mod capability;
pub mod cost;
pub mod fake;
pub mod provider;
pub mod secret;

pub use cost::{CostError, Usage, estimate_cost};
pub use fake::FakeProvider;
pub use provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};
