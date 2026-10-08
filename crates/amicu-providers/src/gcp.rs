//! The Google Cloud (GCP) [`Provider`] adapter, with EU data-residency filtering.
//!
//! GCP is a global hyperscaler offering GPU Compute Engine instances and managed
//! Kubernetes (GKE) across regions worldwide. Amicu targets EU workloads, so this
//! adapter is **residency filtering by construction**: [`GcpProvider::new`] admits
//! only the EU regions from [`regions`], and [`provision`](GcpProvider::provision)
//! rejects any region outside that admitted set with
//! [`ProviderError::UnknownRegion`] - an instance can never be placed in a non-EU
//! region (e.g. `us-central1`) through this adapter.
//! [`GcpProvider::for_residencies`] widens the admitted set explicitly when a
//! caller genuinely needs other jurisdictions.
//!
//! The [`ProviderCapabilities`] descriptor carries GCP's real EU regions and a
//! representative slice of its GPU Compute Engine catalog. GCP offers Spot VMs, so
//! offerings are priced with **both on-demand and spot** rates. Live provisioning
//! against GCP's APIs and real-time capacity/pricing sync are out of scope here
//! (the latter is amicu-planning#23); the provisioning surface simulates the instance
//! lifecycle in memory, mirroring [`FakeProvider`](crate::fake::FakeProvider), so
//! the planner and apply path can be exercised without network or credentials.
//! Prices are illustrative catalog values in USD/hr, not a live quote.

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

/// The adapter identity GCP registers under.
pub const PROVIDER_ID: &str = "gcp";

/// A GCP region: its region code, residency jurisdiction, ISO country code, and
/// human-readable location. Both EU and non-EU entries are listed so the EU
/// filter has something to exclude.
struct GcpRegion {
    /// GCP region code, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
    /// Default zone (AZ) for zone-addressed calls, e.g. `europe-west1-b`.
    zone: &'static str,
}

/// GCP's regions. EU entries are what [`GcpProvider::new`] admits; the non-EU
/// entries exist so the residency filter is a real exclusion, not a hand-picked
/// list. Uses a `fn` (not a `const`) because [`Residency`] is not `Copy`.
fn regions() -> [GcpRegion; 8] {
    [
        // EU.
        GcpRegion {
            code: "europe-west1",
            residency: Residency::Eu,
            country: "BE",
            location: "Belgium",
            zone: "europe-west1-b",
        },
        GcpRegion {
            code: "europe-west4",
            residency: Residency::Eu,
            country: "NL",
            location: "Netherlands",
            zone: "europe-west4-a",
        },
        GcpRegion {
            code: "europe-north1",
            residency: Residency::Eu,
            country: "FI",
            location: "Finland",
            zone: "europe-north1-a",
        },
        GcpRegion {
            code: "europe-west3",
            residency: Residency::Eu,
            country: "DE",
            location: "Frankfurt",
            zone: "europe-west3-a",
        },
        GcpRegion {
            code: "europe-west9",
            residency: Residency::Eu,
            country: "FR",
            location: "Paris",
            zone: "europe-west9-a",
        },
        // Non-EU - excluded by the residency filter.
        GcpRegion {
            code: "us-central1",
            residency: Residency::Us,
            country: "US",
            location: "Iowa",
            zone: "us-central1-a",
        },
        GcpRegion {
            code: "us-east1",
            residency: Residency::Us,
            country: "US",
            location: "South Carolina",
            zone: "us-east1-b",
        },
        GcpRegion {
            code: "asia-east1",
            residency: Residency::Apac,
            country: "TW",
            location: "Taiwan",
            zone: "asia-east1-a",
        },
    ]
}

/// One entry of GCP's GPU Compute Engine catalog: an offering id (the machine
/// type), the accelerator model as GCP names it, its count and VRAM in GiB,
/// representative host resources, and illustrative on-demand and spot rates in USD
/// per hour.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    count: u32,
    /// VRAM per accelerator, in GiB.
    vram_gib: u32,
    vcpus: u32,
    host_mem_gib: u32,
    on_demand: f64,
    spot: f64,
    availability: AvailabilityState,
}

/// A representative slice of GCP's GPU Compute Engine catalog. Prices are
/// illustrative catalog values in USD/hr, not a live quote (real-time pricing is
/// amicu-planning#23). GCP offers Spot VMs, so every offering carries both an on-demand and
/// a spot rate.
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "a3-highgpu-8g",
        model: "nvidia-h100-80gb",
        count: 8,
        vram_gib: 80,
        vcpus: 208,
        host_mem_gib: 1872,
        on_demand: 88.49,
        spot: 30.97,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "a2-ultragpu-1g",
        model: "nvidia-a100-80gb",
        count: 1,
        vram_gib: 80,
        vcpus: 12,
        host_mem_gib: 170,
        on_demand: 5.07,
        spot: 1.77,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "g2-standard-8",
        model: "nvidia-l4",
        count: 1,
        vram_gib: 24,
        vcpus: 8,
        host_mem_gib: 32,
        on_demand: 0.85,
        spot: 0.28,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "n1-standard-8",
        model: "nvidia-tesla-t4",
        count: 1,
        vram_gib: 16,
        vcpus: 8,
        host_mem_gib: 30,
        on_demand: 0.73,
        spot: 0.22,
        availability: AvailabilityState::Available,
    },
];

/// The GCP provider adapter.
///
/// Holds the admitted (residency-filtered) region set and the derived capability
/// descriptor. Provisioning is simulated in memory (see the module docs); every
/// simulated instance and its transitions are recorded for observability.
pub struct GcpProvider {
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

impl GcpProvider {
    /// A GCP provider admitting only EU regions.
    ///
    /// This is the residency-safe default: no non-EU region reaches the
    /// descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// A GCP provider admitting the regions whose residency is in `allowed`. Use
    /// this to deliberately widen beyond the EU default.
    #[must_use]
    pub fn for_residencies(allowed: &[Residency]) -> Self {
        let id = AdapterId::new(PROVIDER_ID);
        let regions: Vec<Region> = regions()
            .into_iter()
            .filter(|r| allowed.contains(&r.residency))
            .map(|r| {
                Region::new(r.code, r.residency)
                    .with_country(r.country)
                    .with_display_name(r.location)
                    .with_default_zone(r.zone)
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

    /// The GCP region codes admitted for `residency`.
    #[must_use]
    pub fn region_codes(residency: &Residency) -> Vec<&'static str> {
        regions()
            .into_iter()
            .filter(|r| &r.residency == residency)
            .map(|r| r.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. Every
    /// offering is sold in every admitted region (GCP's GPU catalog is
    /// region-agnostic at this granularity).
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let region_codes: Vec<String> = regions.iter().map(|r| r.code.clone()).collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::ApiAndSsh,
            CredentialsModel::ServiceAccount,
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
                    count: g.count,
                    memory: format!("{}GiB", g.vram_gib).parse().expect("valid size"),
                    memory_bandwidth_gbps: None,
                }),
                vcpus: Some(g.vcpus),
                memory: Some(
                    format!("{}GiB", g.host_mem_gib)
                        .parse()
                        .expect("valid size"),
                ),
                regions: region_codes.clone(),
                // GCP offers Spot VMs: both an on-demand and a spot rate.
                pricing: Pricing::on_demand(Rate::new(
                    Money::new(g.on_demand, "USD"),
                    RateUnit::PerHour,
                ))
                .with_spot(Rate::new(Money::new(g.spot, "USD"), RateUnit::PerHour)),
                availability: Availability::new(g.availability),
            })
            .collect();
        caps.storage = vec![
            StorageOption::new(StorageKind::Block),
            StorageOption::new(StorageKind::Object),
        ];
        caps.network = NetworkCapabilities {
            bandwidth_mbps: Some(100_000),
            private_network: true,
        };
        caps.orchestration = OrchestrationSupport {
            managed_kubernetes: true, // GKE
            bare_metal: false,        // no general bare-metal GPU offering
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(60),
            teardown_secs: Some(30),
            supports_hibernation: true,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(20_000),
            cached_registry: true, // Artifact Registry
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
        self.state.lock().expect("gcp provider mutex poisoned")
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

    /// The zone a zone-addressed GCP call targets for `region`: the explicit
    /// `override_zone` when present, else the admitted region's default zone.
    /// Adapter-owned - the zone never reaches the residency filter.
    fn resolve_zone(&self, region: &str, override_zone: Option<&str>) -> Option<String> {
        self.capabilities
            .regions
            .iter()
            .find(|r| r.code == region)
            .and_then(|r| r.resolve_zone(override_zone))
            .or(override_zone)
            .map(str::to_string)
    }

    /// Resolve and (pretend to) use the credential to authenticate the GCP API
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
                    detail: "empty GCP service-account key".into(),
                },
            ));
        }
        let _signed = value.expose_secret().len();
        Ok(())
    }
}

impl Default for GcpProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for GcpProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for GcpProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for GcpProvider {
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
        // instance. A real GCP region outside the admitted set is rejected here.
        if !self.admits_region(&request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        self.authenticate(resolver, request.credentials.as_ref())
            .await?;

        // Zone-addressed placement detail the adapter owns: resolve the region's
        // default zone, honoring an explicit override when the request carries one.
        let zone = self.resolve_zone(&request.region, request.zone.as_deref());
        let mut state = self.lock();
        state.counter += 1;
        let id = format!("gcp-{}-{}", request.region, state.counter);

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
            zone,
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
        let caps = GcpProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("gcp"));
        assert_ne!(caps.regions, Vec::new());
        // Every admitted region is EU...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU regions are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"europe-west1"));
        assert!(codes.contains(&"europe-west3"));
        assert!(codes.contains(&"europe-west9"));
        // No non-EU GCP region leaks into the descriptor.
        assert!(!codes.contains(&"us-central1"));
        assert!(!codes.contains(&"us-east1"));
        assert!(!codes.contains(&"asia-east1"));
    }

    #[test]
    fn filter_excludes_non_eu_regions() {
        // The underlying catalog genuinely contains non-EU regions, so the EU
        // default is an exclusion, not a hand-picked list.
        assert!(GcpProvider::region_codes(&Residency::Us).contains(&"us-central1"));
        assert!(GcpProvider::region_codes(&Residency::Apac).contains(&"asia-east1"));
        let widened = GcpProvider::for_residencies(&[Residency::Us]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        // Widening to the US admits the US regions and nothing else.
        assert!(codes.iter().any(|c| c == "us-central1"));
        assert!(codes.iter().any(|c| c == "us-east1"));
        assert!(codes.iter().all(|c| c.starts_with("us-")));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = GcpProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_on_demand_and_spot_pricing() {
        let caps = GcpProvider::new().capabilities();
        let a100 = caps
            .compute
            .iter()
            .find(|o| o.id == "a2-ultragpu-1g")
            .expect("a100 offering present");
        // GCP offers Spot VMs: both an on-demand and a spot rate are exposed.
        assert!(a100.pricing.has_spot());
        assert_eq!(a100.pricing.on_demand_rate().price.currency, "USD");
        assert_eq!(a100.pricing.spot_rate().unwrap().price.currency, "USD");
        // Spot is cheaper than on-demand.
        assert!(
            a100.pricing.spot_rate().unwrap().price.amount
                < a100.pricing.on_demand_rate().price.amount
        );
        // Every offering is sold in every admitted (EU) region.
        assert_eq!(a100.regions.len(), caps.regions.len());
    }

    #[test]
    fn multi_gpu_offering_exposes_accelerator_count() {
        let caps = GcpProvider::new().capabilities();
        let a3 = caps
            .compute
            .iter()
            .find(|o| o.id == "a3-highgpu-8g")
            .expect("a3-highgpu-8g offering present");
        let accel = a3.accelerator.as_ref().expect("accelerator present");
        assert_eq!(accel.count, 8);
        assert_eq!(accel.model, "nvidia-h100-80gb");
        // Memory is per accelerator (80 GiB per H100), not the aggregate.
        assert_eq!(accel.memory, "80GiB".parse().unwrap());
    }

    #[test]
    fn descriptor_models_gcp_operational_facts() {
        let caps = GcpProvider::new().capabilities();
        assert_eq!(caps.provisioning, ProvisioningMethod::ApiAndSsh);
        assert_eq!(caps.credentials, CredentialsModel::ServiceAccount);
        assert_eq!(caps.billing_granularity, BillingGranularity::PerSecond);
        assert!(caps.network.private_network);
        assert!(caps.orchestration.managed_kubernetes); // GKE
        assert!(!caps.orchestration.bare_metal); // no general bare-metal GPU offering
        assert!(caps.lifecycle.supports_hibernation);
        assert!(caps.model_download.cached_registry); // Artifact Registry
        let kinds: Vec<StorageKind> = caps.storage.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StorageKind::Block));
        assert!(kinds.contains(&StorageKind::Object));
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let p = GcpProvider::new();
        let resolver = no_creds();
        let req = ProvisionRequest::new("a2-ultragpu-1g", "europe-west4");

        let handle = p.provision(&req, &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("gcp"));
        assert!(handle.id.starts_with("gcp-europe-west4-"));
        assert_eq!(p.live_instances().len(), 1);

        let history = p.instance_history(&handle.id);
        assert!(matches!(history[0], ProvisioningState::Requested));
        assert!(history.last().unwrap().is_ready());

        p.teardown(&handle, &resolver).await.unwrap();
        assert_eq!(p.live_instances(), Vec::new());
        assert!(p.status(&handle, &resolver).await.unwrap().is_terminal());
    }

    #[tokio::test]
    async fn provision_in_non_eu_region_is_rejected() {
        // us-central1 (Iowa) is a real GCP region outside the admitted EU set.
        let p = GcpProvider::new();
        let err = p
            .provision(
                &ProvisionRequest::new("a2-ultragpu-1g", "us-central1"),
                &no_creds(),
            )
            .await
            .expect_err("non-EU region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
        assert_eq!(p.live_instances(), Vec::new());
    }

    #[tokio::test]
    async fn non_eu_region_admitted_after_widening() {
        // Widening the residency filter to the US admits us-central1.
        let p = GcpProvider::for_residencies(&[Residency::Us]);
        let handle = p
            .provision(
                &ProvisionRequest::new("a2-ultragpu-1g", "us-central1"),
                &no_creds(),
            )
            .await
            .unwrap();
        assert!(handle.state.is_ready());
        assert!(handle.id.starts_with("gcp-us-central1-"));
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let err = GcpProvider::new()
            .provision(&ProvisionRequest::new("nope", "europe-west1"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = GcpProvider::new();
        let resolver = TestResolver::default().with("gcp_service_account_key", "");
        let req = ProvisionRequest::new("a2-ultragpu-1g", "europe-west1")
            .with_credentials(SecretRef::new("gcp_service_account_key"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let p = GcpProvider::new();
        let secret_value = "leak-me-gcp-service-account-key";
        let resolver = TestResolver::default().with("gcp_service_account_key", secret_value);
        let req = ProvisionRequest::new("a2-ultragpu-1g", "europe-west1")
            .with_credentials(SecretRef::new("gcp_service_account_key"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("gcp_service_account_key"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("gcp_service_account_key"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"service_account\""));
    }

    #[tokio::test]
    async fn describe_never_serializes_non_eu_regions() {
        let describe_json = serde_json::to_string(&GcpProvider::new().describe()).unwrap();
        assert!(describe_json.contains("europe-west1"));
        assert!(!describe_json.contains("us-central1"));
        assert!(!describe_json.contains("us-east1"));
        assert!(!describe_json.contains("asia-east1"));
        // GCP carries spot pricing; the descriptor must expose it.
        assert!(describe_json.contains("spot"));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(GcpProvider::new()));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("gcp")).unwrap();
        let handle = found
            .provision(
                &ProvisionRequest::new("g2-standard-8", "europe-north1"),
                &no_creds(),
            )
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = GcpProvider::new().with_health(HealthStatus::Degraded {
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

    #[test]
    fn descriptor_carries_region_default_zones() {
        let caps = GcpProvider::new().capabilities();
        let west1 = caps
            .regions
            .iter()
            .find(|r| r.code == "europe-west1")
            .unwrap();
        assert_eq!(west1.default_zone.as_deref(), Some("europe-west1-b"));
    }

    #[tokio::test]
    async fn provision_resolves_default_zone_onto_handle() {
        let p = GcpProvider::new();
        let handle = p
            .provision(
                &ProvisionRequest::new("a2-ultragpu-1g", "europe-west1"),
                &no_creds(),
            )
            .await
            .unwrap();
        // The region stays the planning-level input; the zone is the adapter's
        // resolved default.
        assert_eq!(handle.region, "europe-west1");
        assert_eq!(handle.zone.as_deref(), Some("europe-west1-b"));
    }

    #[tokio::test]
    async fn explicit_zone_override_is_honored() {
        let p = GcpProvider::new();
        let req =
            ProvisionRequest::new("a2-ultragpu-1g", "europe-west1").with_zone("europe-west1-c");
        let handle = p.provision(&req, &no_creds()).await.unwrap();
        assert_eq!(handle.zone.as_deref(), Some("europe-west1-c"));
        // The override is a placement detail only: the region is unchanged.
        assert_eq!(handle.region, "europe-west1");
    }
}
