//! Cloud/external/secret adapters.

pub mod aws;
pub mod azure;
pub mod capability;
pub mod catalog;
#[cfg(test)]
pub(crate) mod conformance;
pub mod consumer;
pub mod cost;
pub mod fake;
pub mod gcp;
pub mod hetzner;
pub mod modal;
pub mod nebius;
pub mod ovhcloud;
pub mod provider;
pub(crate) mod rest_lifecycle;
pub mod runpod;
pub mod scaleway;
pub mod secret;
pub mod self_hosted;
pub mod ssh;
pub mod sync;
pub mod transport;

pub use aws::AwsProvider;
pub use azure::AzureProvider;
pub use catalog::resolve_residency;
pub use cost::{CostError, Usage, estimate_cost};
pub use fake::FakeProvider;
pub use gcp::GcpProvider;
pub use hetzner::HetznerProvider;
pub use modal::ModalProvider;
pub use nebius::NebiusProvider;
pub use ovhcloud::OvhCloudProvider;
pub use provider::{
    HANDLE_SCHEMA_VERSION, Provider, ProviderError, ProvisionRequest, ProvisioningHandle,
    ProvisioningState, ResourceIdentity, TeardownOutcome,
};
pub use runpod::RunPodProvider;
pub use scaleway::ScalewayProvider;
pub use self_hosted::{SelfHostedConfigError, SelfHostedProvider};
pub use ssh::{CommandOutput, RusshTransport, SshTarget, SshTransport, SshTransportError};
pub use sync::{Freshness, SyncReport, SyncedCapabilities, classify, snapshot, sync_all};
pub use transport::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, ReqwestTransport, TransportError,
};

#[cfg(test)]
mod gpu_catalog_tests {
    use amicu_core::resource::{AcceleratorKind, gpu_card};

    use crate::Provider;

    #[test]
    fn every_gpu_offering_is_a_canonical_catalog_card() {
        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(crate::AwsProvider::new()),
            Box::new(crate::AzureProvider::new()),
            Box::new(crate::GcpProvider::new()),
            Box::new(crate::HetznerProvider::new()),
            Box::new(crate::ModalProvider::new()),
            Box::new(crate::NebiusProvider::new()),
            Box::new(crate::OvhCloudProvider::new()),
            Box::new(crate::RunPodProvider::new()),
            Box::new(crate::ScalewayProvider::new()),
        ];
        for provider in providers {
            for offering in provider.capabilities().compute {
                let Some(acc) = offering.accelerator else {
                    continue;
                };
                if acc.kind != AcceleratorKind::Gpu {
                    continue;
                }
                let card = gpu_card(&acc.model).unwrap_or_else(|| {
                    panic!("{}: `{}` is not a catalog id", offering.id, acc.model)
                });
                assert!(card.memory_matches(acc.memory), "{}: VRAM", offering.id);
            }
        }
        for hw in crate::consumer::catalog() {
            let acc = hw.accelerator.unwrap();
            if acc.kind == AcceleratorKind::Gpu {
                assert!(gpu_card(&acc.model).is_some(), "{}", acc.model);
            }
        }
    }
}
