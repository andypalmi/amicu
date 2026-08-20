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
use amicu_core::secret::{SecretRef, SecretResolver};
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
    /// Every instance ever provisioned, keyed by id, with its current state.
    instances: HashMap<String, ProvisioningHandle>,
    /// Observed state transitions per instance id, in order.
    history: HashMap<String, Vec<ProvisioningState>>,
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
    /// When set, the next `provision` drives to `Failed` with this detail.
    provision_failure: Option<String>,
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
            provision_failure: None,
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

    /// Make `provision` simulate a bring-up failure, ending in
    /// [`ProvisioningState::Failed`] with `detail`.
    #[must_use]
    pub fn with_provision_failure(mut self, detail: impl Into<String>) -> Self {
        self.provision_failure = Some(detail.into());
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

    /// The instances that are currently live (provisioned and non-terminal).
    #[must_use]
    pub fn live_instances(&self) -> Vec<ProvisioningHandle> {
        self.lock()
            .instances
            .values()
            .filter(|h| !h.state.is_terminal())
            .cloned()
            .collect()
    }

    /// The recorded state transitions for `id`, in order.
    #[must_use]
    pub fn instance_history(&self, id: &str) -> Vec<ProvisioningState> {
        self.lock().history.get(id).cloned().unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("fake provider mutex poisoned")
    }

    /// Resolve and (pretend to) use the credential to authenticate a backend
    /// call. The [`SecretValue`](amicu_core::secret::SecretValue) lives only for
    /// this call and is dropped (zeroized) on return; it is never stored.
    ///
    /// An empty resolved credential is rejected as
    /// [`ProviderError::CredentialsRejected`].
    async fn authenticate(
        &self,
        resolver: &dyn SecretResolver,
        credentials: Option<&SecretRef>,
    ) -> Result<(), AdapterError> {
        let Some(secret_ref) = credentials else {
            return Ok(());
        };
        let value = resolver.resolve(secret_ref).await?;
        if value.is_empty() {
            return Err(AdapterError::from_kind(
                ProviderError::CredentialsRejected {
                    detail: "empty credential".into(),
                },
            ));
        }
        // Pretend to sign the simulated backend request; the exposed bytes are
        // never copied out, logged, or serialized.
        let _signed = value.expose_secret().len();
        Ok(())
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
        resolver: &dyn SecretResolver,
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
        // Apply-time credential resolution: value is used to authenticate and
        // dropped inside `authenticate` — never stored on the handle.
        self.authenticate(resolver, request.credentials.as_ref())
            .await?;

        let mut state = self.lock();
        state.counter += 1;
        let id = format!("{}-instance-{}", self.id, state.counter);

        // Drive the bring-up lifecycle, recording each transition.
        let mut transitions = vec![
            ProvisioningState::Requested,
            ProvisioningState::Provisioning {
                detail: "fake instance starting".into(),
            },
        ];
        let final_state = match &self.provision_failure {
            Some(detail) => ProvisioningState::Failed {
                detail: detail.clone(),
            },
            None => ProvisioningState::Ready {
                detail: "fake instance ready".into(),
            },
        };
        transitions.push(final_state.clone());

        let handle = ProvisioningHandle {
            id: id.clone(),
            provider: self.id.clone(),
            region: request.region.clone(),
            zone: None,
            offering_id: request.offering_id.clone(),
            credentials: request.credentials.clone(),
            state: final_state,
        };
        state.provisions.push(request.clone());
        state.history.insert(id.clone(), transitions);
        state.instances.insert(id, handle.clone());
        Ok(handle)
    }

    async fn status(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningState, AdapterError> {
        self.authenticate(resolver, handle.credentials.as_ref())
            .await?;
        self.lock()
            .instances
            .get(&handle.id)
            .map(|h| h.state.clone())
            .ok_or_else(|| {
                AdapterError::from_kind(ProviderError::UnknownInstance {
                    id: handle.id.clone(),
                })
            })
    }

    async fn teardown(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<(), AdapterError> {
        self.authenticate(resolver, handle.credentials.as_ref())
            .await?;

        let mut state = self.lock();
        let Some(instance) = state.instances.get_mut(&handle.id) else {
            return Err(AdapterError::from_kind(ProviderError::UnknownInstance {
                id: handle.id.clone(),
            }));
        };
        instance.state = ProvisioningState::Terminated {
            detail: "fake instance torn down".into(),
        };
        let entry = state.history.entry(handle.id.clone()).or_default();
        entry.push(ProvisioningState::Stopping {
            detail: "fake instance stopping".into(),
        });
        entry.push(ProvisioningState::Terminated {
            detail: "fake instance torn down".into(),
        });
        state.teardowns.push(handle.id.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::adapter::Registry;
    use amicu_core::diagnostic::{Category as DiagCategory, DiagnosticCode};
    use amicu_core::secret::SecretValue;
    use std::sync::Arc;

    fn provider() -> FakeProvider {
        FakeProvider::new("fake-cloud")
    }

    /// A small in-memory [`SecretResolver`] for hermetic tests.
    #[derive(Default)]
    struct TestResolver {
        secrets: HashMap<String, SecretValue>,
    }

    impl TestResolver {
        fn with(mut self, name: &str, value: &str) -> Self {
            self.secrets
                .insert(name.into(), SecretValue::new(value.to_string()));
            self
        }
    }

    #[async_trait]
    impl SecretResolver for TestResolver {
        async fn resolve(&self, secret: &SecretRef) -> Result<SecretValue, AdapterError> {
            self.secrets.get(&secret.name).cloned().ok_or_else(|| {
                AdapterError::permanent(
                    DiagnosticCode::new(DiagCategory::Policy, 10),
                    format!("secret '{}' unavailable", secret.name),
                )
            })
        }
    }

    /// A resolver seeded with nothing — for requests that carry no credential.
    fn no_creds() -> TestResolver {
        TestResolver::default()
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
        let resolver = no_creds();
        let req = ProvisionRequest::new("gpu.h100.1x", "eu-central-1");

        let handle = p.provision(&req, &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("fake-cloud"));
        assert_eq!(handle.offering_id, "gpu.h100.1x");
        assert_eq!(p.provision_calls(), vec![req]);
        assert_eq!(p.live_instances().len(), 1);

        p.teardown(&handle, &resolver).await.unwrap();
        assert_eq!(p.teardown_calls(), vec![handle.id.clone()]);
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn provision_drives_lifecycle_and_status_observes_ready() {
        let p = provider();
        let resolver = no_creds();
        let handle = p
            .provision(
                &ProvisionRequest::new("gpu.h100.1x", "eu-central-1"),
                &resolver,
            )
            .await
            .unwrap();

        // Bring-up transitions are recorded and observable.
        let history = p.instance_history(&handle.id);
        assert!(matches!(history[0], ProvisioningState::Requested));
        assert!(matches!(history[1], ProvisioningState::Provisioning { .. }));
        assert!(history.last().unwrap().is_ready());

        // `status` observes the live Ready state.
        let observed = p.status(&handle, &resolver).await.unwrap();
        assert!(observed.is_ready());
        assert!(!observed.is_terminal());
    }

    #[tokio::test]
    async fn teardown_transitions_to_terminated() {
        let p = provider();
        let resolver = no_creds();
        let handle = p
            .provision(
                &ProvisionRequest::new("gpu.h100.1x", "eu-central-1"),
                &resolver,
            )
            .await
            .unwrap();

        p.teardown(&handle, &resolver).await.unwrap();

        let observed = p.status(&handle, &resolver).await.unwrap();
        assert!(matches!(observed, ProvisioningState::Terminated { .. }));
        assert!(observed.is_terminal());

        let history = p.instance_history(&handle.id);
        assert!(matches!(
            history[history.len() - 2],
            ProvisioningState::Stopping { .. }
        ));
        assert!(matches!(
            history[history.len() - 1],
            ProvisioningState::Terminated { .. }
        ));
    }

    #[tokio::test]
    async fn simulated_failure_yields_failed_state() {
        let p = provider().with_provision_failure("out of capacity in simulation");
        let resolver = no_creds();
        let handle = p
            .provision(
                &ProvisionRequest::new("gpu.h100.1x", "eu-central-1"),
                &resolver,
            )
            .await
            .unwrap();

        assert!(handle.state.is_failed());
        assert!(handle.state.is_terminal());

        let observed = p.status(&handle, &resolver).await.unwrap();
        assert!(observed.is_failed());
        // A failed instance is terminal, so it is not counted as live.
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn predicate_accessors_classify_states() {
        assert!(ProvisioningState::Ready { detail: "x".into() }.is_ready());
        assert!(ProvisioningState::Stopped { detail: "x".into() }.is_stopped());
        assert!(ProvisioningState::Failed { detail: "x".into() }.is_failed());
        assert!(ProvisioningState::Failed { detail: "x".into() }.is_terminal());
        assert!(ProvisioningState::Terminated { detail: "x".into() }.is_terminal());
        assert!(!ProvisioningState::Requested.is_terminal());
        assert!(!ProvisioningState::Provisioning { detail: "x".into() }.is_ready());
    }

    #[tokio::test]
    async fn provision_resolves_credentials_at_apply_time() {
        let p = provider();
        let resolver = TestResolver::default().with("provider_token", "s3cr3t-token");
        let req = ProvisionRequest::new("gpu.h100.1x", "eu-central-1")
            .with_credentials(SecretRef::new("provider_token"));

        let handle = p.provision(&req, &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        // The handle carries the reference, never the resolved value.
        assert_eq!(handle.credentials, Some(SecretRef::new("provider_token")));
    }

    #[tokio::test]
    async fn provision_missing_secret_is_error_not_panic() {
        let p = provider();
        let resolver = no_creds(); // seeded with nothing
        let req = ProvisionRequest::new("gpu.h100.1x", "eu-central-1")
            .with_credentials(SecretRef::new("provider_token"));

        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("missing secret must surface as an error");
        assert_eq!(err.code().category(), DiagCategory::Policy);
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = provider();
        let resolver = TestResolver::default().with("provider_token", "");
        let req = ProvisionRequest::new("gpu.h100.1x", "eu-central-1")
            .with_credentials(SecretRef::new("provider_token"));

        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let p = provider();
        let secret_value = "leak-me-token";
        let resolver = TestResolver::default().with("provider_token", secret_value);
        let req = ProvisionRequest::new("gpu.h100.1x", "eu-central-1")
            .with_credentials(SecretRef::new("provider_token"));

        // Requests are serialized into plans: only the ref name, never the value.
        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("provider_token"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();

        // Handles are persisted; they carry the ref, never the value.
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("provider_token"));
        assert!(!handle_json.contains(secret_value));

        // describe() (planning telemetry) carries only the credential kind.
        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_a_permanent_provider_error() {
        let err = provider()
            .provision(&ProvisionRequest::new("nope", "eu-central-1"), &no_creds())
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
            .provision(&ProvisionRequest::new("gpu.h100.1x", "mars-1"), &no_creds())
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
            zone: None,
            offering_id: "gpu.h100.1x".into(),
            credentials: None,
            state: ProvisioningState::Ready {
                detail: "n/a".into(),
            },
        };
        let err = p
            .teardown(&phantom, &no_creds())
            .await
            .expect_err("unknown instance");
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
            .provision(
                &ProvisionRequest::new("gpu.h100.1x", "eu-central-1"),
                &no_creds(),
            )
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
