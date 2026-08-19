//! The OVHcloud [`Provider`] adapter, with EU data-residency filtering.
//!
//! OVHcloud is a French sovereign-cloud provider (SecNumCloud-certified) that
//! offers GPU instances through its Public Cloud API and bare-metal dedicated
//! servers across data centers worldwide. Amicu targets EU workloads, so this
//! adapter is **residency filtering by construction**: [`OvhCloudProvider::new`]
//! admits only the EU data centers from [`datacenters`], and
//! [`provision`](OvhCloudProvider::provision) rejects any region outside that
//! admitted set with [`ProviderError::UnknownRegion`] — an instance can never be
//! placed in a non-compliant region (including the UK) through this adapter.
//! [`OvhCloudProvider::for_residencies`] widens the admitted set explicitly when
//! a caller genuinely needs other jurisdictions.
//!
//! The [`ProviderCapabilities`] descriptor carries OVHcloud's real EU data
//! centers and a representative slice of its single-GPU Public Cloud catalog with
//! on-demand pricing (OVH bills hourly/monthly with no public spot market, so the
//! catalog is on-demand-only). Live provisioning against OVH's REST API and
//! real-time capacity/pricing sync are out of scope here (the latter is
//! TASK-023); the provisioning surface simulates the instance lifecycle in
//! memory, mirroring [`FakeProvider`](crate::fake::FakeProvider), so the planner
//! and apply path can be exercised without network or credentials. Prices are
//! illustrative catalog values in EUR/hr, not a live quote.

use std::collections::HashMap;
use std::sync::Mutex;

use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_core::resource::{Accelerator, AcceleratorKind, Money};
use amicu_core::secret::{SecretRef, SecretResolver};
use async_trait::async_trait;

use crate::capability::{
    Availability, AvailabilityState, BillingGranularity, ComputeOffering, CredentialsModel,
    LifecycleBehavior, ModelDownloadPerformance, NetworkCapabilities, OrchestrationSupport,
    Pricing, ProviderCapabilities, ProvisioningMethod, Rate, RateUnit, Region, Residency,
    StorageKind, StorageOption,
};
use crate::provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};

/// The adapter identity OVHcloud registers under.
pub const PROVIDER_ID: &str = "ovhcloud";

/// An OVHcloud data center: its region code, residency jurisdiction, ISO country
/// code, and human-readable location. Both EU and non-EU entries are listed so
/// the EU filter has something to exclude.
struct DataCenter {
    /// OVHcloud region code, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
}

/// OVHcloud's data centers. EU entries are what [`OvhCloudProvider::new`] admits;
/// the UK and non-EU entries exist so the residency filter is a real exclusion,
/// not a hand-picked list. Residency uses a `fn` (not a `const`) because
/// [`Residency::Other`] owns a `String`.
fn datacenters() -> [DataCenter; 9] {
    [
        // EU.
        DataCenter {
            code: "GRA",
            residency: Residency::Eu,
            country: "FR",
            location: "Gravelines",
        },
        DataCenter {
            code: "SBG",
            residency: Residency::Eu,
            country: "FR",
            location: "Strasbourg",
        },
        DataCenter {
            code: "DE1",
            residency: Residency::Eu,
            country: "DE",
            location: "Frankfurt",
        },
        DataCenter {
            code: "WAW",
            residency: Residency::Eu,
            country: "PL",
            location: "Warsaw",
        },
        // UK — excluded by the EU filter (not EU/EEA).
        DataCenter {
            code: "UK1",
            residency: Residency::Uk,
            country: "GB",
            location: "London",
        },
        // Non-EU — excluded by the residency filter.
        DataCenter {
            code: "BHS",
            residency: Residency::Ca,
            country: "CA",
            location: "Beauharnois",
        },
        DataCenter {
            code: "US-EAST-VA",
            residency: Residency::Us,
            country: "US",
            location: "Virginia",
        },
        DataCenter {
            code: "SGP",
            residency: Residency::Apac,
            country: "SG",
            location: "Singapore",
        },
        DataCenter {
            code: "SYD",
            residency: Residency::Apac,
            country: "AU",
            location: "Sydney",
        },
    ]
}

/// One entry of OVHcloud's GPU catalog: an offering id, the accelerator model as
/// OVH names it, its VRAM in GiB, representative host resources, and an
/// illustrative on-demand rate in EUR per hour.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    vram_gib: u32,
    vcpus: u32,
    host_mem_gib: u32,
    on_demand: f64,
    availability: AvailabilityState,
}

/// A representative slice of OVHcloud's single-GPU Public Cloud catalog. Prices
/// are illustrative catalog values in EUR/hr, not a live quote (real-time pricing
/// is TASK-023). OVH offers no public spot market, so pricing is on-demand-only.
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "gpu.h100.1x",
        model: "h100",
        vram_gib: 80,
        vcpus: 30,
        host_mem_gib: 240,
        on_demand: 2.99,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "gpu.a100.1x",
        model: "a100",
        vram_gib: 80,
        vcpus: 15,
        host_mem_gib: 180,
        on_demand: 2.29,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.l40s.1x",
        model: "l40s",
        vram_gib: 48,
        vcpus: 15,
        host_mem_gib: 90,
        on_demand: 1.39,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.l4.1x",
        model: "l4",
        vram_gib: 24,
        vcpus: 8,
        host_mem_gib: 32,
        on_demand: 0.79,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.v100.1x",
        model: "v100",
        vram_gib: 32,
        vcpus: 13,
        host_mem_gib: 45,
        on_demand: 0.89,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.t4.1x",
        model: "t4",
        vram_gib: 16,
        vcpus: 8,
        host_mem_gib: 30,
        on_demand: 0.39,
        availability: AvailabilityState::Available,
    },
];

/// The OVHcloud provider adapter.
///
/// Holds the admitted (residency-filtered) region set and the derived capability
/// descriptor. Provisioning is simulated in memory (see the module docs); every
/// simulated instance and its transitions are recorded for observability.
pub struct OvhCloudProvider {
    id: AdapterId,
    capabilities: ProviderCapabilities,
    health: HealthStatus,
    state: Mutex<State>,
}

/// Mutable in-memory state behind the simulated provisioning surface.
#[derive(Default)]
struct State {
    counter: u64,
    instances: HashMap<String, ProvisioningHandle>,
    history: HashMap<String, Vec<ProvisioningState>>,
}

impl OvhCloudProvider {
    /// An OVHcloud provider admitting only EU data centers.
    ///
    /// This is the residency-safe default: no non-EU region (including the UK)
    /// reaches the descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// An OVHcloud provider admitting the data centers whose residency is in
    /// `allowed`. Use this to deliberately widen beyond the EU default.
    #[must_use]
    pub fn for_residencies(allowed: &[Residency]) -> Self {
        let id = AdapterId::new(PROVIDER_ID);
        let regions: Vec<Region> = datacenters()
            .into_iter()
            .filter(|dc| allowed.contains(&dc.residency))
            .map(|dc| {
                Region::new(dc.code, dc.residency)
                    .with_country(dc.country)
                    .with_display_name(dc.location)
            })
            .collect();
        let capabilities = Self::capabilities_for(&id, regions);
        Self {
            id,
            capabilities,
            health: HealthStatus::Healthy,
            state: Mutex::new(State::default()),
        }
    }

    /// Override the health status (e.g. for tests or a degraded probe result).
    #[must_use]
    pub fn with_health(mut self, health: HealthStatus) -> Self {
        self.capabilities.health = health.clone();
        self.health = health;
        self
    }

    /// The OVHcloud data-center codes admitted for `residency`.
    #[must_use]
    pub fn datacenter_codes(residency: &Residency) -> Vec<&'static str> {
        datacenters()
            .into_iter()
            .filter(|dc| &dc.residency == residency)
            .map(|dc| dc.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. Every
    /// offering is sold in every admitted region (OVH's GPU catalog is
    /// data-center agnostic at this granularity).
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let region_codes: Vec<String> = regions.iter().map(|r| r.code.clone()).collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::ApiAndSsh,
            CredentialsModel::ApiKey,
            BillingGranularity::PerHour,
            Availability::new(AvailabilityState::Available),
            HealthStatus::Healthy,
        );
        caps.regions = regions;
        caps.compute = GPUS
            .iter()
            .map(|g| ComputeOffering {
                id: g.id.into(),
                accelerator: Some(Accelerator {
                    kind: AcceleratorKind::Gpu,
                    model: g.model.into(),
                    count: 1,
                    memory: format!("{}GiB", g.vram_gib).parse().expect("valid size"),
                }),
                vcpus: Some(g.vcpus),
                memory: Some(
                    format!("{}GiB", g.host_mem_gib)
                        .parse()
                        .expect("valid size"),
                ),
                regions: region_codes.clone(),
                pricing: Pricing::on_demand(Rate::new(
                    Money::new(g.on_demand, "EUR"),
                    RateUnit::PerHour,
                )),
                availability: Availability::new(g.availability),
            })
            .collect();
        caps.storage = vec![
            StorageOption::new(StorageKind::Block),
            StorageOption::new(StorageKind::Object),
            StorageOption::new(StorageKind::NetworkFs),
        ];
        caps.network = NetworkCapabilities {
            bandwidth_mbps: Some(10_000),
            private_network: true,
        };
        caps.orchestration = OrchestrationSupport {
            managed_kubernetes: true,
            bare_metal: true,
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(120),
            teardown_secs: Some(30),
            supports_hibernation: false,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(3_000),
            cached_registry: false,
        };
        caps
    }

    /// The instances currently live (provisioned and non-terminal).
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
        self.state.lock().expect("ovhcloud provider mutex poisoned")
    }

    fn has_offering(&self, offering_id: &str) -> bool {
        self.capabilities
            .compute
            .iter()
            .any(|o| o.id == offering_id)
    }

    fn admits_region(&self, region: &str) -> bool {
        self.capabilities.regions.iter().any(|r| r.code == region)
    }

    /// Resolve and (pretend to) use the credential to authenticate the OVH API
    /// call. The [`SecretValue`](amicu_core::secret::SecretValue) lives only for
    /// this call and is dropped (zeroized) on return; it is never stored, placed
    /// on a handle, logged, or serialized. An empty credential is rejected as
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
                    detail: "empty OVHcloud API key".into(),
                },
            ));
        }
        let _signed = value.expose_secret().len();
        Ok(())
    }
}

impl Default for OvhCloudProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for OvhCloudProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for OvhCloudProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for OvhCloudProvider {
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
        // Residency filter: only admitted (EU by default) regions can host an
        // instance. A real OVH data center outside the admitted set — including
        // the UK — is rejected here.
        if !self.admits_region(&request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        self.authenticate(resolver, request.credentials.as_ref())
            .await?;

        let mut state = self.lock();
        state.counter += 1;
        let id = format!("ovhcloud-{}-{}", request.region, state.counter);

        let ready = ProvisioningState::Ready {
            detail: "instance ready".into(),
        };
        let transitions = vec![
            ProvisioningState::Requested,
            ProvisioningState::Provisioning {
                detail: "instance starting".into(),
            },
            ready.clone(),
        ];
        let handle = ProvisioningHandle {
            id: id.clone(),
            provider: self.id.clone(),
            region: request.region.clone(),
            offering_id: request.offering_id.clone(),
            credentials: request.credentials.clone(),
            state: ready,
        };
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
            detail: "instance terminated".into(),
        };
        let entry = state.history.entry(handle.id.clone()).or_default();
        entry.push(ProvisioningState::Stopping {
            detail: "instance stopping".into(),
        });
        entry.push(ProvisioningState::Terminated {
            detail: "instance terminated".into(),
        });
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

    fn no_creds() -> TestResolver {
        TestResolver::default()
    }

    #[test]
    fn new_admits_only_eu_regions() {
        let caps = OvhCloudProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("ovhcloud"));
        assert!(!caps.regions.is_empty());
        // Every admitted region is EU/EEA...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU data centers are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"GRA"));
        assert!(codes.contains(&"DE1"));
        // No UK or non-EU OVH data center leaks into the descriptor.
        assert!(!codes.contains(&"UK1"));
        assert!(!codes.contains(&"BHS"));
        assert!(!codes.contains(&"US-EAST-VA"));
        assert!(!codes.contains(&"SGP"));
    }

    #[test]
    fn filter_excludes_uk_and_non_eu_datacenters() {
        // The underlying catalog genuinely contains UK and non-EU data centers,
        // so the EU default is an exclusion, not a hand-picked list.
        assert!(OvhCloudProvider::datacenter_codes(&Residency::Uk).contains(&"UK1"));
        assert!(OvhCloudProvider::datacenter_codes(&Residency::Us).contains(&"US-EAST-VA"));
        let widened =
            OvhCloudProvider::for_residencies(&[Residency::Eu, Residency::Uk, Residency::Us]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        assert!(codes.iter().any(|c| c == "GRA"));
        assert!(codes.iter().any(|c| c == "UK1"));
        assert!(codes.iter().any(|c| c == "US-EAST-VA"));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = OvhCloudProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_on_demand_only_pricing() {
        let caps = OvhCloudProvider::new().capabilities();
        let h100 = caps
            .compute
            .iter()
            .find(|o| o.id == "gpu.h100.1x")
            .expect("h100 offering present");
        // OVH has no public spot market: pricing is on-demand-only.
        assert!(!h100.pricing.has_spot());
        assert_eq!(h100.pricing.on_demand_rate().price.currency, "EUR");
        // H100 capacity is constrained.
        assert!(!h100.availability.is_available());
        // Every offering is sold in every admitted (EU) region.
        assert_eq!(h100.regions.len(), caps.regions.len());
    }

    #[test]
    fn descriptor_models_ovh_operational_facts() {
        let caps = OvhCloudProvider::new().capabilities();
        assert_eq!(caps.provisioning, ProvisioningMethod::ApiAndSsh);
        assert_eq!(caps.credentials, CredentialsModel::ApiKey);
        assert_eq!(caps.billing_granularity, BillingGranularity::PerHour);
        // vRack private network.
        assert!(caps.network.private_network);
        assert!(caps.orchestration.managed_kubernetes);
        assert!(caps.orchestration.bare_metal);
        assert!(!caps.lifecycle.supports_hibernation);
        assert!(!caps.model_download.cached_registry);
        let kinds: Vec<StorageKind> = caps.storage.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StorageKind::Block));
        assert!(kinds.contains(&StorageKind::Object));
        assert!(kinds.contains(&StorageKind::NetworkFs));
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let p = OvhCloudProvider::new();
        let resolver = no_creds();
        let req = ProvisionRequest::new("gpu.a100.1x", "GRA");

        let handle = p.provision(&req, &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("ovhcloud"));
        assert!(handle.id.starts_with("ovhcloud-GRA-"));
        assert_eq!(p.live_instances().len(), 1);

        let history = p.instance_history(&handle.id);
        assert!(matches!(history[0], ProvisioningState::Requested));
        assert!(history.last().unwrap().is_ready());

        p.teardown(&handle, &resolver).await.unwrap();
        assert!(p.live_instances().is_empty());
        assert!(p.status(&handle, &resolver).await.unwrap().is_terminal());
    }

    #[tokio::test]
    async fn provision_in_uk_region_is_rejected() {
        // UK1 (London) is a real OVH data center, but the EU adapter does not
        // admit it: the UK is outside the EU/EEA residency boundary.
        let p = OvhCloudProvider::new();
        let err = p
            .provision(&ProvisionRequest::new("gpu.a100.1x", "UK1"), &no_creds())
            .await
            .expect_err("UK region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn provision_in_non_eu_region_is_rejected() {
        // BHS (Canada) is a real OVH data center outside the admitted EU set.
        let p = OvhCloudProvider::new();
        let err = p
            .provision(&ProvisionRequest::new("gpu.a100.1x", "BHS"), &no_creds())
            .await
            .expect_err("non-EU region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let err = OvhCloudProvider::new()
            .provision(&ProvisionRequest::new("nope", "GRA"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = OvhCloudProvider::new();
        let resolver = TestResolver::default().with("ovh_api_key", "");
        let req = ProvisionRequest::new("gpu.a100.1x", "GRA")
            .with_credentials(SecretRef::new("ovh_api_key"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let p = OvhCloudProvider::new();
        let secret_value = "leak-me-ovh-api-key";
        let resolver = TestResolver::default().with("ovh_api_key", secret_value);
        let req = ProvisionRequest::new("gpu.a100.1x", "GRA")
            .with_credentials(SecretRef::new("ovh_api_key"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("ovh_api_key"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("ovh_api_key"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"api_key\""));
    }

    #[tokio::test]
    async fn describe_never_serializes_uk_or_non_eu_regions() {
        let describe_json = serde_json::to_string(&OvhCloudProvider::new().describe()).unwrap();
        assert!(describe_json.contains("GRA"));
        assert!(!describe_json.contains("UK1"));
        assert!(!describe_json.contains("BHS"));
        assert!(!describe_json.contains("US-EAST-VA"));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(OvhCloudProvider::new()));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("ovhcloud")).unwrap();
        let handle = found
            .provision(&ProvisionRequest::new("gpu.t4.1x", "DE1"), &no_creds())
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = OvhCloudProvider::new().with_health(HealthStatus::Degraded {
            detail: "maintenance".into(),
        });
        assert!(matches!(
            p.health().await.unwrap(),
            HealthStatus::Degraded { .. }
        ));
        assert!(matches!(
            p.capabilities().health,
            HealthStatus::Degraded { .. }
        ));
    }
}
