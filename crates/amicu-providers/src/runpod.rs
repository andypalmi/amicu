//! The RunPod [`Provider`] adapter, with EU data-residency filtering.
//!
//! RunPod offers provider-managed GPU pods (`POST /pods`) across data centers
//! worldwide. Amicu targets EU workloads, so this adapter is **residency
//! filtering by construction**: [`RunPodProvider::new`] admits only the EU/EEA
//! data centers from [`DATACENTERS`], and [`provision`](RunPodProvider::provision)
//! rejects any region outside that admitted set with
//! [`ProviderError::UnknownRegion`] — a pod can never be placed in a non-compliant
//! region through this adapter. [`RunPodProvider::for_residencies`] widens the
//! admitted set explicitly when a caller genuinely needs other jurisdictions.
//!
//! The [`ProviderCapabilities`] descriptor carries RunPod's real EU data centers
//! and a representative slice of its GPU catalog with on-demand and interruptible
//! (spot) pricing. Live provisioning against RunPod's REST API and real-time
//! capacity/pricing sync are out of scope here (the latter is TASK-023); the
//! provisioning surface simulates the pod lifecycle in memory, mirroring
//! [`FakeProvider`](crate::fake::FakeProvider), so the planner and apply path can
//! be exercised without network or credentials. Prices are illustrative catalog
//! values, not a live quote.

use std::collections::HashMap;
use std::sync::Mutex;

use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_core::resource::{Accelerator, AcceleratorKind, Money};
use amicu_core::secret::{SecretRef, SecretResolver};
use async_trait::async_trait;

use crate::capability::{
    Availability, AvailabilityState, BillingGranularity, ComputeOffering, CredentialsModel,
    LifecycleBehavior, ModelDownloadPerformance, NetworkCapabilities, Pricing,
    ProviderCapabilities, ProvisioningMethod, Rate, RateUnit, Region, Residency, StorageKind,
    StorageOption,
};
use crate::provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};

/// The adapter identity RunPod registers under.
pub const PROVIDER_ID: &str = "runpod";

/// A RunPod data center: its `dataCenterId`, residency jurisdiction, ISO country
/// code, and human-readable location. Both EU/EEA and non-EU entries are listed
/// so the EU filter has something to exclude.
struct DataCenter {
    /// RunPod `dataCenterId`, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
}

/// RunPod's data centers. EU/EEA entries are what [`RunPodProvider::new`] admits;
/// the non-EU entries exist so the residency filter is a real exclusion, not a
/// hand-picked list. Residency uses a `fn` (not a `const`) because
/// [`Residency::Other`] owns a `String`.
fn datacenters() -> [DataCenter; 12] {
    [
        // EU / EEA.
        DataCenter {
            code: "EU-RO-1",
            residency: Residency::Eu,
            country: "RO",
            location: "Romania",
        },
        DataCenter {
            code: "EU-SE-1",
            residency: Residency::Eu,
            country: "SE",
            location: "Sweden",
        },
        DataCenter {
            code: "EU-NL-1",
            residency: Residency::Eu,
            country: "NL",
            location: "Netherlands",
        },
        DataCenter {
            code: "EU-CZ-1",
            residency: Residency::Eu,
            country: "CZ",
            location: "Czechia",
        },
        DataCenter {
            code: "EUR-IS-1",
            residency: Residency::Eu,
            country: "IS",
            location: "Iceland",
        },
        DataCenter {
            code: "EUR-NO-1",
            residency: Residency::Eu,
            country: "NO",
            location: "Norway",
        },
        // Non-EU — excluded by the residency filter.
        DataCenter {
            code: "US-CA-1",
            residency: Residency::Us,
            country: "US",
            location: "California",
        },
        DataCenter {
            code: "US-GA-1",
            residency: Residency::Us,
            country: "US",
            location: "Georgia",
        },
        DataCenter {
            code: "US-KS-2",
            residency: Residency::Us,
            country: "US",
            location: "Kansas",
        },
        DataCenter {
            code: "CA-MTL-1",
            residency: Residency::Ca,
            country: "CA",
            location: "Montreal",
        },
        DataCenter {
            code: "AP-JP-1",
            residency: Residency::Apac,
            country: "JP",
            location: "Japan",
        },
        DataCenter {
            code: "OC-AU-1",
            residency: Residency::Apac,
            country: "AU",
            location: "Australia",
        },
    ]
}

/// One entry of RunPod's GPU catalog: an offering id, the accelerator model as
/// RunPod names it, its VRAM in GiB, representative host resources, and
/// illustrative on-demand / spot rates in USD per hour.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    vram_gib: u32,
    vcpus: u32,
    host_mem_gib: u32,
    on_demand: f64,
    spot: f64,
    availability: AvailabilityState,
}

/// A representative slice of RunPod's single-GPU catalog. Prices are illustrative
/// catalog values in USD/hr, not a live quote (real-time pricing is TASK-023).
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "gpu.h100-80gb.1x",
        model: "h100",
        vram_gib: 80,
        vcpus: 20,
        host_mem_gib: 251,
        on_demand: 2.99,
        spot: 1.99,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "gpu.a100-80gb.1x",
        model: "a100",
        vram_gib: 80,
        vcpus: 16,
        host_mem_gib: 125,
        on_demand: 1.89,
        spot: 1.19,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.l40s.1x",
        model: "l40s",
        vram_gib: 48,
        vcpus: 16,
        host_mem_gib: 94,
        on_demand: 0.99,
        spot: 0.79,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.rtx-a6000.1x",
        model: "rtx-a6000",
        vram_gib: 48,
        vcpus: 8,
        host_mem_gib: 50,
        on_demand: 0.79,
        spot: 0.49,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.rtx-4090.1x",
        model: "rtx-4090",
        vram_gib: 24,
        vcpus: 8,
        host_mem_gib: 41,
        on_demand: 0.69,
        spot: 0.34,
        availability: AvailabilityState::Available,
    },
];

/// The RunPod provider adapter.
///
/// Holds the admitted (residency-filtered) region set and the derived capability
/// descriptor. Provisioning is simulated in memory (see the module docs); every
/// simulated instance and its transitions are recorded for observability.
pub struct RunPodProvider {
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

impl RunPodProvider {
    /// A RunPod provider admitting only EU/EEA data centers.
    ///
    /// This is the residency-safe default: no non-EU region reaches the
    /// descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// A RunPod provider admitting the data centers whose residency is in
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

    /// The RunPod data-center codes admitted for `residency`.
    #[must_use]
    pub fn datacenter_codes(residency: &Residency) -> Vec<&'static str> {
        datacenters()
            .into_iter()
            .filter(|dc| &dc.residency == residency)
            .map(|dc| dc.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. Every
    /// offering is sold in every admitted region (RunPod's GPU catalog is
    /// data-center agnostic at this granularity).
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let region_codes: Vec<String> = regions.iter().map(|r| r.code.clone()).collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::Api,
            CredentialsModel::ApiToken,
            BillingGranularity::PerSecond,
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
                    Money::new(g.on_demand, "USD"),
                    RateUnit::PerHour,
                ))
                .with_spot(Rate::new(Money::new(g.spot, "USD"), RateUnit::PerHour)),
                availability: Availability::new(g.availability),
            })
            .collect();
        caps.storage = vec![
            StorageOption::new(StorageKind::LocalNvme),
            StorageOption::new(StorageKind::NetworkFs),
        ];
        caps.network = NetworkCapabilities {
            bandwidth_mbps: Some(10_000),
            private_network: false,
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(30),
            teardown_secs: Some(10),
            supports_hibernation: true,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(5_000),
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
        self.state.lock().expect("runpod provider mutex poisoned")
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

    /// Resolve and (pretend to) use the credential to authenticate the RunPod
    /// API call. The [`SecretValue`](amicu_core::secret::SecretValue) lives only
    /// for this call and is dropped (zeroized) on return; it is never stored,
    /// placed on a handle, logged, or serialized. An empty credential is rejected
    /// as [`ProviderError::CredentialsRejected`].
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
                    detail: "empty RunPod API token".into(),
                },
            ));
        }
        let _signed = value.expose_secret().len();
        Ok(())
    }
}

impl Default for RunPodProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for RunPodProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for RunPodProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for RunPodProvider {
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
        // Residency filter: only admitted (EU by default) regions can host a pod.
        // A real RunPod data center outside the admitted set is rejected here.
        if !self.admits_region(&request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        self.authenticate(resolver, request.credentials.as_ref())
            .await?;

        let mut state = self.lock();
        state.counter += 1;
        let id = format!("runpod-{}-{}", request.region, state.counter);

        let ready = ProvisioningState::Ready {
            detail: "pod ready".into(),
        };
        let transitions = vec![
            ProvisioningState::Requested,
            ProvisioningState::Provisioning {
                detail: "pod starting".into(),
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
            detail: "pod terminated".into(),
        };
        let entry = state.history.entry(handle.id.clone()).or_default();
        entry.push(ProvisioningState::Stopping {
            detail: "pod stopping".into(),
        });
        entry.push(ProvisioningState::Terminated {
            detail: "pod terminated".into(),
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
        let caps = RunPodProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("runpod"));
        assert!(!caps.regions.is_empty());
        // Every admitted region is EU/EEA...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU data centers are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"EU-RO-1"));
        assert!(codes.contains(&"EUR-IS-1"));
        // No non-EU RunPod data center leaks into the descriptor.
        assert!(!codes.contains(&"US-CA-1"));
        assert!(!codes.contains(&"AP-JP-1"));
    }

    #[test]
    fn filter_excludes_non_eu_datacenters() {
        // The underlying catalog genuinely contains non-EU data centers, so the
        // EU default is an exclusion, not a hand-picked list.
        assert!(RunPodProvider::datacenter_codes(&Residency::Us).contains(&"US-CA-1"));
        let widened = RunPodProvider::for_residencies(&[Residency::Eu, Residency::Us]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        assert!(codes.iter().any(|c| c == "EU-RO-1"));
        assert!(codes.iter().any(|c| c == "US-CA-1"));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = RunPodProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_on_demand_and_spot_pricing() {
        let caps = RunPodProvider::new().capabilities();
        let h100 = caps
            .compute
            .iter()
            .find(|o| o.id == "gpu.h100-80gb.1x")
            .expect("h100 offering present");
        assert!(h100.pricing.has_spot());
        assert_eq!(h100.pricing.on_demand_rate().price.currency, "USD");
        // Every offering is sold in every admitted (EU) region.
        assert_eq!(h100.regions.len(), caps.regions.len());
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let p = RunPodProvider::new();
        let resolver = no_creds();
        let req = ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1");

        let handle = p.provision(&req, &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("runpod"));
        assert!(handle.id.starts_with("runpod-EU-RO-1-"));
        assert_eq!(p.live_instances().len(), 1);

        let history = p.instance_history(&handle.id);
        assert!(matches!(history[0], ProvisioningState::Requested));
        assert!(history.last().unwrap().is_ready());

        p.teardown(&handle, &resolver).await.unwrap();
        assert!(p.live_instances().is_empty());
        assert!(p.status(&handle, &resolver).await.unwrap().is_terminal());
    }

    #[tokio::test]
    async fn provision_in_non_eu_region_is_rejected() {
        // US-CA-1 is a real RunPod data center, but the EU adapter does not admit
        // it: the residency filter restricts pods to compliant regions.
        let p = RunPodProvider::new();
        let err = p
            .provision(
                &ProvisionRequest::new("gpu.a100-80gb.1x", "US-CA-1"),
                &no_creds(),
            )
            .await
            .expect_err("non-EU region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let err = RunPodProvider::new()
            .provision(&ProvisionRequest::new("nope", "EU-RO-1"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = RunPodProvider::new();
        let resolver = TestResolver::default().with("runpod_token", "");
        let req = ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1")
            .with_credentials(SecretRef::new("runpod_token"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let p = RunPodProvider::new();
        let secret_value = "leak-me-runpod-token";
        let resolver = TestResolver::default().with("runpod_token", secret_value);
        let req = ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1")
            .with_credentials(SecretRef::new("runpod_token"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("runpod_token"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("runpod_token"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"api_token\""));
    }

    #[tokio::test]
    async fn describe_never_serializes_non_eu_regions() {
        let describe_json = serde_json::to_string(&RunPodProvider::new().describe()).unwrap();
        assert!(describe_json.contains("EU-RO-1"));
        assert!(!describe_json.contains("US-CA-1"));
        assert!(!describe_json.contains("AP-JP-1"));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(RunPodProvider::new()));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("runpod")).unwrap();
        let handle = found
            .provision(
                &ProvisionRequest::new("gpu.rtx-4090.1x", "EU-NL-1"),
                &no_creds(),
            )
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = RunPodProvider::new().with_health(HealthStatus::Degraded {
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
