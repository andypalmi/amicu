//! An in-memory [`Provider`] for tests and local development.
//!
//! [`FakeProvider`] returns canned [`ProviderCapabilities`] and
//! [`HealthStatus`], and simulates provisioning/teardown in memory: it records
//! every call, hands back a synthetic [`ProvisioningHandle`], and tracks live
//! instances so tests can drive and observe the surface without any IO. It is
//! the reference implementation later provider adapters and the planner tests
//! build against.

use std::collections::HashMap;
use std::sync::Mutex;

use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_core::resource::{Accelerator, AcceleratorKind, Money};
use async_trait::async_trait;

use crate::capability::{
    Availability, AvailabilityState, BillingGranularity, ComputeOffering, CredentialsModel,
    Pricing, ProviderCapabilities, ProvisioningMethod, Rate, RateUnit, Region, Residency,
};
use crate::provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};

/// Mutable in-memory state behind the fake.
#[derive(Default)]
struct State {
    /// Monotonic counter backing synthetic instance ids.
    counter: u64,
    /// Live (not yet torn down) instances, keyed by id.
    live: HashMap<String, ProvisioningHandle>,
    /// Every provision request seen, in order.
    provisions: Vec<ProvisionRequest>,
    /// Every torn-down instance id, in order.
    teardowns: Vec<String>,
}

/// An in-memory provider that records calls and simulates provisioning.
pub struct FakeProvider {
    id: AdapterId,
    capabilities: ProviderCapabilities,
    health: HealthStatus,
    state: Mutex<State>,
}

impl FakeProvider {
    /// A fake provider with the given id, canned [`sample_capabilities`] and a
    /// [`HealthStatus::Healthy`] health.
    ///
    /// [`sample_capabilities`]: FakeProvider::sample_capabilities
    #[must_use]
    pub fn new(id: &str) -> Self {
        let aid = AdapterId::new(id);
        Self {
            capabilities: Self::sample_capabilities(&aid),
            id: aid,
            health: HealthStatus::Healthy,
            state: Mutex::new(State::default()),
        }
    }

    /// Override the canned capabilities.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: ProviderCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Override the canned health status.
    #[must_use]
    pub fn with_health(mut self, health: HealthStatus) -> Self {
        self.health = health;
        self
    }

    /// A representative descriptor: one EU region and one on-demand H100
    /// offering, provisioned by API with an API token.
    #[must_use]
    pub fn sample_capabilities(id: &AdapterId) -> ProviderCapabilities {
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::Api,
            CredentialsModel::ApiToken,
            BillingGranularity::PerHour,
            Availability::new(AvailabilityState::Available),
            HealthStatus::Healthy,
        );
        caps.regions = vec![Region::new("eu-central-1", Residency::Eu).with_country("DE")];
        caps.compute = vec![ComputeOffering {
            id: "gpu.h100.1x".into(),
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::Gpu,
                model: "h100".into(),
                count: 1,
                memory: "80GiB".parse().expect("valid size"),
            }),
            vcpus: Some(32),
            memory: Some("128GiB".parse().expect("valid size")),
            regions: vec!["eu-central-1".into()],
            pricing: Pricing::on_demand(Rate::new(Money::new(2.5, "EUR"), RateUnit::PerHour)),
            availability: Availability::new(AvailabilityState::Available),
        }];
        caps
    }

    /// Every provision request the fake has seen, in order.
    #[must_use]
    pub fn provision_calls(&self) -> Vec<ProvisionRequest> {
        self.lock().provisions.clone()
    }

    /// Every instance id the fake has torn down, in order.
    #[must_use]
    pub fn teardown_calls(&self) -> Vec<String> {
        self.lock().teardowns.clone()
    }

    /// The instances that are currently live (provisioned, not torn down).
    #[must_use]
    pub fn live_instances(&self) -> Vec<ProvisioningHandle> {
        self.lock().live.values().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("fake provider mutex poisoned")
    }

    fn has_offering(&self, offering_id: &str) -> bool {
        self.capabilities
            .compute
            .iter()
            .any(|o| o.id == offering_id)
    }

    fn has_region(&self, region: &str) -> bool {
        self.capabilities.regions.iter().any(|r| r.code == region)
    }
}

#[async_trait]
impl Adapter for FakeProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for FakeProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    async fn provision(
        &self,
        request: &ProvisionRequest,
    ) -> Result<ProvisioningHandle, AdapterError> {
        if !self.has_offering(&request.offering_id) {
            return Err(AdapterError::from_kind(ProviderError::UnknownOffering {
                offering: request.offering_id.clone(),
            }));
        }
        if !self.has_region(&request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }

        let mut state = self.lock();
        state.counter += 1;
        let handle = ProvisioningHandle {
            id: format!("{}-instance-{}", self.id, state.counter),
            provider: self.id.clone(),
            region: request.region.clone(),
            offering_id: request.offering_id.clone(),
            state: ProvisioningState::Ready {
                detail: "fake instance ready".into(),
            },
        };
        state.provisions.push(request.clone());
        state.live.insert(handle.id.clone(), handle.clone());
        Ok(handle)
    }

    async fn teardown(&self, handle: &ProvisioningHandle) -> Result<(), AdapterError> {
        let mut state = self.lock();
        if state.live.remove(&handle.id).is_none() {
            return Err(AdapterError::from_kind(ProviderError::UnknownInstance {
                id: handle.id.clone(),
            }));
        }
        state.teardowns.push(handle.id.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::adapter::Registry;
    use amicu_core::diagnostic::Category as DiagCategory;
    use std::sync::Arc;

    fn provider() -> FakeProvider {
        FakeProvider::new("fake-cloud")
    }

    #[tokio::test]
    async fn id_and_health_return_canned_data() {
        let p = provider().with_health(HealthStatus::Degraded {
            detail: "maintenance".into(),
        });
        assert_eq!(p.id(), AdapterId::new("fake-cloud"));
        assert_eq!(
            p.health().await.unwrap(),
            HealthStatus::Degraded {
                detail: "maintenance".into()
            }
        );
    }

    #[tokio::test]
    async fn capabilities_return_canned_descriptor() {
        let caps = provider().capabilities();
        assert_eq!(caps.provider, AdapterId::new("fake-cloud"));
        assert_eq!(caps.compute.len(), 1);
        assert_eq!(caps.compute[0].id, "gpu.h100.1x");
        assert!(caps.regions.iter().any(Region::is_eu));
    }

    #[tokio::test]
    async fn describe_yields_capability_json() {
        let json = provider().describe();
        assert_eq!(
            json.get("provider").and_then(|v| v.as_str()),
            Some("fake-cloud")
        );
        assert!(json.get("compute").is_some());
    }

    #[tokio::test]
    async fn provision_then_teardown_is_observable() {
        let p = provider();
        let req = ProvisionRequest::new("gpu.h100.1x", "eu-central-1");

        let handle = p.provision(&req).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("fake-cloud"));
        assert_eq!(handle.offering_id, "gpu.h100.1x");
        assert_eq!(p.provision_calls(), vec![req]);
        assert_eq!(p.live_instances().len(), 1);

        p.teardown(&handle).await.unwrap();
        assert_eq!(p.teardown_calls(), vec![handle.id.clone()]);
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_a_permanent_provider_error() {
        let err = provider()
            .provision(&ProvisionRequest::new("nope", "eu-central-1"))
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
        assert_eq!(err.code().category(), DiagCategory::Provider);
        assert!(!err.is_retryable());
        assert!(std::error::Error::source(&err).is_some());
    }

    #[tokio::test]
    async fn provision_unknown_region_is_rejected() {
        let err = provider()
            .provision(&ProvisionRequest::new("gpu.h100.1x", "mars-1"))
            .await
            .expect_err("unknown region");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
    }

    #[tokio::test]
    async fn teardown_unknown_instance_is_rejected() {
        let p = provider();
        let phantom = ProvisioningHandle {
            id: "ghost".into(),
            provider: AdapterId::new("fake-cloud"),
            region: "eu-central-1".into(),
            offering_id: "gpu.h100.1x".into(),
            state: ProvisioningState::Ready {
                detail: "n/a".into(),
            },
        };
        let err = p.teardown(&phantom).await.expect_err("unknown instance");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-012");
    }

    #[tokio::test]
    async fn usable_as_trait_object_and_registry_discoverable() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(provider()));
        registry.register(Arc::new(FakeProvider::new("other-cloud")));
        assert_eq!(registry.len(), 2);

        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("fake-cloud")).unwrap();
        let handle = found
            .provision(&ProvisionRequest::new("gpu.h100.1x", "eu-central-1"))
            .await
            .unwrap();
        assert!(handle.state.is_ready());

        let discovered = registry.discover();
        assert_eq!(discovered.len(), 2);
        for entry in discovered {
            assert!(entry.capabilities.get("compute").is_some());
        }
    }
}
