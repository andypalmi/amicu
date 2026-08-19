//! Cloud/external/secret adapters.

pub mod capability;
pub mod fake;
pub mod provider;
pub mod secret;

pub use fake::FakeProvider;
pub use provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};
