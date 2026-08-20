//! Cloud/external/secret adapters.

pub mod aws;
pub mod azure;
pub mod capability;
pub mod catalog;
pub mod cost;
pub mod fake;
pub mod gcp;
pub mod hetzner;
pub mod nebius;
pub mod ovhcloud;
pub mod provider;
pub mod runpod;
pub mod scaleway;
pub mod secret;
pub mod self_hosted;
pub mod sync;
pub mod transport;

pub use aws::AwsProvider;
pub use azure::AzureProvider;
pub use catalog::resolve_residency;
pub use cost::{CostError, Usage, estimate_cost};
pub use fake::FakeProvider;
pub use gcp::GcpProvider;
pub use hetzner::HetznerProvider;
pub use nebius::NebiusProvider;
pub use ovhcloud::OvhCloudProvider;
pub use provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};
pub use runpod::RunPodProvider;
pub use scaleway::ScalewayProvider;
pub use self_hosted::{SelfHostedConfigError, SelfHostedProvider};
pub use sync::{Freshness, SyncReport, SyncedCapabilities, classify, snapshot, sync_all};
pub use transport::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, ReqwestTransport, TransportError,
};
