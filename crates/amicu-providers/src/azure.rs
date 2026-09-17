//! The Azure [`Provider`] adapter, with EU data-residency filtering.
//!
//! Azure is a global hyperscaler offering GPU virtual machines and managed
//! Kubernetes (AKS) across regions worldwide - EU, UK, and non-EU alike. Amicu
//! targets EU workloads, so this adapter is **residency filtering by
//! construction**: [`AzureProvider::new`] admits only the EU regions from
//! [`regions`], and [`provision`](AzureProvider::provision) rejects any region
//! outside that admitted set with [`ProviderError::UnknownRegion`] - an instance
//! can never be placed in a non-compliant region (including the UK) through this
//! adapter. [`AzureProvider::for_residencies`] widens the admitted set explicitly
//! when a caller genuinely needs other jurisdictions.
//!
//! The [`ProviderCapabilities`] descriptor carries Azure's real EU regions and a
//! representative slice of its GPU VM catalog. Azure runs a Spot VM market, so
//! offerings carry both an on-demand and a spot rate. Live provisioning against
//! the Azure Resource Manager API and real-time capacity/pricing sync are out of
//! scope here (the latter is amicu-planning#23); the provisioning surface simulates the
//! instance lifecycle in memory, mirroring [`FakeProvider`](crate::fake::FakeProvider),
//! so the planner and apply path can be exercised without network or credentials.
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

/// The adapter identity Azure registers under.
pub const PROVIDER_ID: &str = "azure";

/// An Azure region: its region code, residency jurisdiction, ISO country code,
/// and human-readable location. Both EU and non-EU (incl. UK) entries are listed
/// so the EU filter has something to exclude.
struct AzRegion {
    /// Azure region code, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
}

/// Azure's regions. EU entries are what [`AzureProvider::new`] admits; the UK and
/// non-EU entries exist so the residency filter is a real exclusion, not a
/// hand-picked list. Uses a `fn` (not a `const`) because [`Residency::Other`]
/// owns a `String`.
fn regions() -> [AzRegion; 8] {
    [
        // EU.
        AzRegion {
            code: "westeurope",
            residency: Residency::Eu,
            country: "NL",
            location: "Amsterdam",
        },
        AzRegion {
            code: "northeurope",
            residency: Residency::Eu,
            country: "IE",
            location: "Dublin",
        },
        AzRegion {
            code: "germanywestcentral",
            residency: Residency::Eu,
            country: "DE",
            location: "Frankfurt",
        },
        AzRegion {
            code: "swedencentral",
            residency: Residency::Eu,
            country: "SE",
            location: "Gävle",
        },
        AzRegion {
            code: "francecentral",
            residency: Residency::Eu,
            country: "FR",
            location: "Paris",
        },
        // UK - excluded by the EU filter (not EU/EEA).
        AzRegion {
            code: "uksouth",
            residency: Residency::Uk,
            country: "GB",
            location: "London",
        },
        // Non-EU - excluded by the residency filter.
        AzRegion {
            code: "eastus",
            residency: Residency::Us,
            country: "US",
            location: "Virginia",
        },
        AzRegion {
            code: "southeastasia",
            residency: Residency::Apac,
            country: "SG",
            location: "Singapore",
        },
    ]
}

/// One entry of Azure's GPU VM catalog: an offering id (the VM size), the
/// accelerator model, its count and per-accelerator VRAM in GiB, representative
/// host resources, and illustrative on-demand and spot rates in USD per hour.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    count: u32,
    vram_gib: u32,
    vcpus: u32,
    host_mem_gib: u32,
    on_demand: f64,
    spot: f64,
    availability: AvailabilityState,
}

/// A representative slice of Azure's GPU VM catalog. Prices are illustrative
/// catalog values in USD/hr, not a live quote (real-time pricing is amicu-planning#23).
/// Every VM carries a Spot rate - Azure runs a public Spot VM market.
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "Standard_ND96isr_H100_v5",
        model: "h100",
        count: 8,
        vram_gib: 80,
        vcpus: 96,
        host_mem_gib: 1900,
        on_demand: 98.32,
        spot: 39.33,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "Standard_NC24ads_A100_v4",
        model: "a100",
        count: 1,
        vram_gib: 80,
        vcpus: 24,
        host_mem_gib: 220,
        on_demand: 3.67,
        spot: 1.10,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "Standard_NV36ads_A10_v5",
        model: "a10",
        count: 1,
        vram_gib: 24,
        vcpus: 36,
        host_mem_gib: 440,
        on_demand: 3.20,
        spot: 0.96,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "Standard_NC4as_T4_v3",
        model: "t4",
        count: 1,
        vram_gib: 16,
        vcpus: 4,
        host_mem_gib: 28,
        on_demand: 0.53,
        spot: 0.16,
        availability: AvailabilityState::Available,
    },
];

/// The Azure provider adapter.
///
/// Holds the admitted (residency-filtered) region set and the derived capability
/// descriptor. Provisioning is simulated in memory (see the module docs); every
/// simulated instance and its transitions are recorded for observability.
pub struct AzureProvider {
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

impl AzureProvider {
    /// An Azure provider admitting only EU regions.
    ///
    /// This is the residency-safe default: no non-EU region (including the UK)
    /// reaches the descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// An Azure provider admitting the regions whose residency is in `allowed`.
    /// Use this to deliberately widen beyond the EU default.
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

    /// The Azure region codes admitted for `residency`.
    #[must_use]
    pub fn region_codes(residency: &Residency) -> Vec<&'static str> {
        regions()
            .into_iter()
            .filter(|r| &r.residency == residency)
            .map(|r| r.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. Every
    /// offering is sold in every admitted region (Azure's GPU VM catalog is
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
                // On-demand plus a Spot rate: Azure runs a public Spot VM market.
                pricing: Pricing::on_demand(Rate::new(
                    Money::new(g.on_demand, "USD"),
                    RateUnit::PerHour,
                ))
                .with_spot(Rate::new(Money::new(g.spot, "USD"), RateUnit::PerHour)),
                availability: Availability::new(g.availability),
            })
            .collect();
        caps.storage = vec![
            StorageOption::new(StorageKind::Block),  // Managed Disks
            StorageOption::new(StorageKind::Object), // Blob Storage
        ];
        caps.network = NetworkCapabilities {
            bandwidth_mbps: Some(40_000),
            private_network: true, // Virtual Network
        };
        caps.orchestration = OrchestrationSupport {
            managed_kubernetes: true, // AKS
            bare_metal: false,
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(90),
            teardown_secs: Some(30),
            supports_hibernation: true,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(8_000),
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
        self.state.lock().expect("azure provider mutex poisoned")
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

    /// Resolve and (pretend to) use the credential to authenticate the Azure API
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
                    detail: "empty Azure service principal secret".into(),
                },
            ));
        }
        let _signed = value.expose_secret().len();
        Ok(())
    }
}

impl Default for AzureProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for AzureProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for AzureProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for AzureProvider {
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
        // instance. A real Azure region outside the admitted set - including the
        // UK - is rejected here.
        if !self.admits_region(&request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        self.authenticate(resolver, request.credentials.as_ref())
            .await?;

        let mut state = self.lock();
        state.counter += 1;
        let id = format!("azure-{}-{}", request.region, state.counter);

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
            zone: None,
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
        let caps = AzureProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("azure"));
        assert!(!caps.regions.is_empty());
        // Every admitted region is EU/EEA...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU regions are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"westeurope"));
        assert!(codes.contains(&"germanywestcentral"));
        assert!(codes.contains(&"francecentral"));
        // No UK or non-EU Azure region leaks into the descriptor.
        assert!(!codes.contains(&"uksouth"));
        assert!(!codes.contains(&"eastus"));
        assert!(!codes.contains(&"southeastasia"));
    }

    #[test]
    fn filter_excludes_uk_and_non_eu_regions() {
        // The underlying catalog genuinely contains UK and non-EU regions, so the
        // EU default is an exclusion, not a hand-picked list. Widening the filter
        // to include the UK admits uksouth.
        assert!(AzureProvider::region_codes(&Residency::Uk).contains(&"uksouth"));
        assert!(AzureProvider::region_codes(&Residency::Us).contains(&"eastus"));
        let widened = AzureProvider::for_residencies(&[Residency::Eu, Residency::Uk]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        assert!(codes.iter().any(|c| c == "westeurope"));
        assert!(codes.iter().any(|c| c == "uksouth"));
        // Widening only to the UK does not admit non-EU regions.
        assert!(!codes.iter().any(|c| c == "eastus"));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = AzureProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_on_demand_and_spot_pricing() {
        let caps = AzureProvider::new().capabilities();
        let h100 = caps
            .compute
            .iter()
            .find(|o| o.id == "Standard_ND96isr_H100_v5")
            .expect("h100 VM offering present");
        // Azure runs a Spot VM market: both rates are exposed.
        assert!(h100.pricing.has_spot());
        assert_eq!(h100.pricing.on_demand_rate().price.currency, "USD");
        assert_eq!(h100.pricing.spot_rate().unwrap().price.currency, "USD");
        // The H100 VM is an 8-GPU node.
        assert_eq!(h100.accelerator.as_ref().unwrap().count, 8);
        // H100 capacity is constrained.
        assert!(!h100.availability.is_available());
        // Every offering is sold in every admitted (EU) region.
        assert_eq!(h100.regions.len(), caps.regions.len());
    }

    #[test]
    fn descriptor_models_azure_operational_facts() {
        let caps = AzureProvider::new().capabilities();
        assert_eq!(caps.provisioning, ProvisioningMethod::ApiAndSsh);
        assert_eq!(caps.credentials, CredentialsModel::ServiceAccount);
        assert_eq!(caps.billing_granularity, BillingGranularity::PerSecond);
        assert!(caps.network.private_network);
        assert!(caps.orchestration.managed_kubernetes); // AKS
        assert!(!caps.orchestration.bare_metal);
        let kinds: Vec<StorageKind> = caps.storage.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StorageKind::Block));
        assert!(kinds.contains(&StorageKind::Object));
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let p = AzureProvider::new();
        let resolver = no_creds();
        let req = ProvisionRequest::new("Standard_NC24ads_A100_v4", "westeurope");

        let handle = p.provision(&req, &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("azure"));
        assert!(handle.id.starts_with("azure-westeurope-"));
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
        // uksouth (London) is a real Azure region, but the EU adapter does not
        // admit it: the UK is outside the EU/EEA residency boundary.
        let p = AzureProvider::new();
        let err = p
            .provision(
                &ProvisionRequest::new("Standard_NC24ads_A100_v4", "uksouth"),
                &no_creds(),
            )
            .await
            .expect_err("UK region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn provision_in_non_eu_region_is_rejected() {
        // eastus (Virginia) is a real Azure region outside the admitted EU set.
        let p = AzureProvider::new();
        let err = p
            .provision(
                &ProvisionRequest::new("Standard_NC24ads_A100_v4", "eastus"),
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
        let err = AzureProvider::new()
            .provision(&ProvisionRequest::new("nope", "westeurope"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = AzureProvider::new();
        let resolver = TestResolver::default().with("azure_client_secret", "");
        let req = ProvisionRequest::new("Standard_NC24ads_A100_v4", "westeurope")
            .with_credentials(SecretRef::new("azure_client_secret"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let p = AzureProvider::new();
        let secret_value = "leak-me-azure-client-secret";
        let resolver = TestResolver::default().with("azure_client_secret", secret_value);
        let req = ProvisionRequest::new("Standard_NC24ads_A100_v4", "westeurope")
            .with_credentials(SecretRef::new("azure_client_secret"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("azure_client_secret"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("azure_client_secret"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"service_account\""));
    }

    #[tokio::test]
    async fn describe_never_serializes_uk_or_non_eu_regions() {
        let describe_json = serde_json::to_string(&AzureProvider::new().describe()).unwrap();
        assert!(describe_json.contains("westeurope"));
        assert!(!describe_json.contains("uksouth"));
        assert!(!describe_json.contains("eastus"));
        assert!(!describe_json.contains("southeastasia"));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(AzureProvider::new()));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("azure")).unwrap();
        let handle = found
            .provision(
                &ProvisionRequest::new("Standard_NC4as_T4_v3", "northeurope"),
                &no_creds(),
            )
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = AzureProvider::new().with_health(HealthStatus::Degraded {
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
