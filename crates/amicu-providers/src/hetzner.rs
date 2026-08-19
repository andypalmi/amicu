//! The Hetzner [`Provider`] adapter, with EU data-residency filtering.
//!
//! Hetzner is a German provider with a strong EU-sovereignty posture. Its GPU
//! hardware is **dedicated / bare-metal** (the GEX line), not a GPU cloud:
//! servers are provisioned through the hcloud API and driven over SSH, and
//! dedicated capacity bills **monthly** in EUR (no spot). Amicu targets EU
//! workloads, so this adapter is **residency filtering by construction**:
//! [`HetznerProvider::new`] admits only the EU data centers from
//! [`datacenters`], and [`provision`](HetznerProvider::provision) rejects any
//! region outside that admitted set with [`ProviderError::UnknownRegion`] — a
//! server can never be placed in a non-compliant region through this adapter.
//! [`HetznerProvider::for_residencies`] widens the admitted set explicitly when
//! a caller genuinely needs other jurisdictions.
//!
//! Hetzner's GPU hardware lives only in its German and Finnish data centers, so
//! the GPU offerings are sold in the EU data centers alone. The
//! [`ProviderCapabilities`] descriptor carries Hetzner's real data centers and a
//! representative slice of the GEX dedicated-server catalog with monthly
//! on-demand pricing. Live provisioning against the hcloud REST API and
//! real-time capacity/pricing sync are out of scope here (the latter is
//! TASK-023); the provisioning surface simulates the server lifecycle in
//! memory, mirroring [`FakeProvider`](crate::fake::FakeProvider), so the planner
//! and apply path can be exercised without network or credentials. Prices are
//! illustrative catalog values, not a live quote.

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

/// The adapter identity Hetzner registers under.
pub const PROVIDER_ID: &str = "hetzner";

/// A Hetzner data center: its location code, residency jurisdiction, ISO
/// country code, human-readable location, and whether it hosts GPU (GEX)
/// hardware. Both EU and non-EU entries are listed so the EU filter has
/// something to exclude.
struct DataCenter {
    /// Hetzner location code, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
    /// Whether GPU (GEX dedicated) hardware is available here. Hetzner's GPU
    /// fleet lives only in its German and Finnish sites.
    gpu: bool,
}

/// Hetzner's data centers. The EU entries are what [`HetznerProvider::new`]
/// admits; the non-EU entries exist so the residency filter is a real
/// exclusion, not a hand-picked list. Residency uses a `fn` (not a `const`)
/// because [`Residency::Other`] owns a `String`.
fn datacenters() -> [DataCenter; 6] {
    [
        // EU — GPU hardware lives here (DE / FI).
        DataCenter {
            code: "nbg1",
            residency: Residency::Eu,
            country: "DE",
            location: "Nuremberg",
            gpu: true,
        },
        DataCenter {
            code: "fsn1",
            residency: Residency::Eu,
            country: "DE",
            location: "Falkenstein",
            gpu: true,
        },
        DataCenter {
            code: "hel1",
            residency: Residency::Eu,
            country: "FI",
            location: "Helsinki",
            gpu: true,
        },
        // Non-EU — excluded by the residency filter, and no GPU hardware.
        DataCenter {
            code: "ash",
            residency: Residency::Us,
            country: "US",
            location: "Ashburn, Virginia",
            gpu: false,
        },
        DataCenter {
            code: "hil",
            residency: Residency::Us,
            country: "US",
            location: "Hillsboro, Oregon",
            gpu: false,
        },
        DataCenter {
            code: "sin",
            residency: Residency::Apac,
            country: "SG",
            location: "Singapore",
            gpu: false,
        },
    ]
}

/// One entry of Hetzner's GEX dedicated-server catalog: an offering id, the GPU
/// model as Hetzner names it, its VRAM in GiB, the (generous) host resources of
/// the dedicated box, and an illustrative monthly on-demand rate in EUR.
/// Dedicated servers bill monthly with no spot market.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    vram_gib: u32,
    vcpus: u32,
    host_mem_gib: u32,
    /// Illustrative on-demand rate in EUR **per month**.
    on_demand_monthly: f64,
    availability: AvailabilityState,
}

/// A representative slice of Hetzner's GEX GPU dedicated-server line. These are
/// large bare-metal boxes, not cloud slices. Prices are illustrative catalog
/// values in EUR/month, not a live quote (real-time pricing is TASK-023).
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "gpu.rtx-4000-ada.1x",
        model: "rtx-4000-ada",
        vram_gib: 20,
        vcpus: 24,
        host_mem_gib: 64,
        on_demand_monthly: 184.0,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.rtx-6000-ada.1x",
        model: "rtx-6000-ada",
        vram_gib: 48,
        vcpus: 40,
        host_mem_gib: 128,
        on_demand_monthly: 819.0,
        availability: AvailabilityState::Available,
    },
];

/// The Hetzner provider adapter.
///
/// Holds the admitted (residency-filtered) region set and the derived
/// capability descriptor. Provisioning is simulated in memory (see the module
/// docs); every simulated server and its transitions are recorded for
/// observability.
pub struct HetznerProvider {
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

impl HetznerProvider {
    /// A Hetzner provider admitting only EU data centers.
    ///
    /// This is the residency-safe default: no non-EU region reaches the
    /// descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// A Hetzner provider admitting the data centers whose residency is in
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

    /// The Hetzner data-center codes admitted for `residency`.
    #[must_use]
    pub fn datacenter_codes(residency: &Residency) -> Vec<&'static str> {
        datacenters()
            .into_iter()
            .filter(|dc| &dc.residency == residency)
            .map(|dc| dc.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. The GEX
    /// offerings are sold only in the GPU-capable (German / Finnish) data
    /// centers among the admitted set.
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let gpu_codes: Vec<String> = datacenters()
            .into_iter()
            .filter(|dc| dc.gpu)
            .map(|dc| dc.code.to_string())
            .collect();
        let gpu_region_codes: Vec<String> = regions
            .iter()
            .map(|r| r.code.clone())
            .filter(|code| gpu_codes.contains(code))
            .collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::ApiAndSsh,
            CredentialsModel::ApiToken,
            BillingGranularity::PerMonth,
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
                regions: gpu_region_codes.clone(),
                // Dedicated servers bill monthly on-demand; no spot market.
                pricing: Pricing::on_demand(Rate::new(
                    Money::new(g.on_demand_monthly, "EUR"),
                    RateUnit::PerMonth,
                )),
                availability: Availability::new(g.availability),
            })
            .collect();
        caps.storage = vec![
            StorageOption::new(StorageKind::LocalNvme),
            StorageOption::new(StorageKind::Block),
            StorageOption::new(StorageKind::Object),
        ];
        caps.network = NetworkCapabilities {
            bandwidth_mbps: Some(1_000),
            private_network: true,
        };
        caps.orchestration = OrchestrationSupport {
            managed_kubernetes: false,
            bare_metal: true,
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(1_200),
            teardown_secs: Some(120),
            supports_hibernation: false,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(1_000),
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
        self.state.lock().expect("hetzner provider mutex poisoned")
    }

    fn has_offering(&self, offering_id: &str) -> bool {
        self.capabilities
            .compute
            .iter()
            .any(|o| o.id == offering_id)
    }

    /// Whether `offering_id` is sold in `region`. An offering's region set is
    /// the intersection of admitted (EU by default) residency and the
    /// GPU-capable data centers, so this rejects both non-admitted regions and
    /// admitted regions that do not host the offering's GPU line.
    fn offering_serves_region(&self, offering_id: &str, region: &str) -> bool {
        self.capabilities
            .compute
            .iter()
            .find(|o| o.id == offering_id)
            .is_some_and(|o| o.regions.iter().any(|r| r == region))
    }

    /// Resolve and (pretend to) use the credential to authenticate the hcloud
    /// API call. The [`SecretValue`](amicu_core::secret::SecretValue) lives only
    /// for this call and is dropped (zeroized) on return; it is never stored,
    /// placed on a handle, logged, or serialized. An empty credential is
    /// rejected as [`ProviderError::CredentialsRejected`].
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
                    detail: "empty Hetzner API token".into(),
                },
            ));
        }
        let _signed = value.expose_secret().len();
        Ok(())
    }
}

impl Default for HetznerProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for HetznerProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for HetznerProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for HetznerProvider {
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
        // Residency + availability filter: the offering must be sold in the
        // requested region. A non-EU data center, or an EU one that does not
        // host this GPU line, is rejected here.
        if !self.offering_serves_region(&request.offering_id, &request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        self.authenticate(resolver, request.credentials.as_ref())
            .await?;

        let mut state = self.lock();
        state.counter += 1;
        let id = format!("hetzner-{}-{}", request.region, state.counter);

        let ready = ProvisioningState::Ready {
            detail: "server ready".into(),
        };
        let transitions = vec![
            ProvisioningState::Requested,
            ProvisioningState::Provisioning {
                detail: "server starting".into(),
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
            detail: "server terminated".into(),
        };
        let entry = state.history.entry(handle.id.clone()).or_default();
        entry.push(ProvisioningState::Stopping {
            detail: "server stopping".into(),
        });
        entry.push(ProvisioningState::Terminated {
            detail: "server terminated".into(),
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
        let caps = HetznerProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("hetzner"));
        assert!(!caps.regions.is_empty());
        // Every admitted region is EU...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU data centers are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"nbg1"));
        assert!(codes.contains(&"fsn1"));
        assert!(codes.contains(&"hel1"));
        // No non-EU Hetzner data center leaks into the descriptor.
        assert!(!codes.contains(&"ash"));
        assert!(!codes.contains(&"hil"));
        assert!(!codes.contains(&"sin"));
    }

    #[test]
    fn filter_excludes_non_eu_datacenters() {
        // The underlying catalog genuinely contains US and APAC data centers, so
        // the EU default is an exclusion, not a hand-picked list.
        assert!(HetznerProvider::datacenter_codes(&Residency::Us).contains(&"ash"));
        assert!(HetznerProvider::datacenter_codes(&Residency::Apac).contains(&"sin"));
        let widened = HetznerProvider::for_residencies(&[Residency::Eu, Residency::Us]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        assert!(codes.iter().any(|c| c == "nbg1"));
        assert!(codes.iter().any(|c| c == "ash"));
        assert!(!codes.iter().any(|c| c == "sin"));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = HetznerProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_monthly_on_demand_pricing() {
        let caps = HetznerProvider::new().capabilities();
        assert_eq!(caps.billing_granularity, BillingGranularity::PerMonth);
        let gex = caps
            .compute
            .iter()
            .find(|o| o.id == "gpu.rtx-6000-ada.1x")
            .expect("rtx-6000-ada offering present");
        // Dedicated servers bill monthly in EUR with no spot market.
        assert!(!gex.pricing.has_spot());
        let rate = gex.pricing.on_demand_rate();
        assert_eq!(rate.price.currency, "EUR");
        assert_eq!(rate.unit, RateUnit::PerMonth);
        // The GEX line is sold in every admitted (EU) region.
        assert_eq!(gex.regions.len(), caps.regions.len());
    }

    #[test]
    fn gpu_offerings_are_sold_only_in_eu_datacenters() {
        // Widen to US: the US data centers are admitted as regions, but Hetzner's
        // GPU hardware lives only in the EU, so the GEX offerings are not sold
        // there.
        let widened = HetznerProvider::for_residencies(&[Residency::Eu, Residency::Us]);
        let caps = widened.capabilities();
        assert!(caps.regions.iter().any(|r| r.code == "ash"));
        for offering in &caps.compute {
            assert!(offering.regions.iter().all(|c| c != "ash" && c != "hil"));
            assert!(offering.regions.iter().any(|c| c == "nbg1"));
        }
    }

    #[test]
    fn descriptor_reports_bare_metal_provisioning() {
        let caps = HetznerProvider::new().capabilities();
        assert_eq!(caps.provisioning, ProvisioningMethod::ApiAndSsh);
        assert!(caps.orchestration.bare_metal);
        assert!(!caps.orchestration.managed_kubernetes);
        assert!(caps.network.private_network);
        assert!(!caps.lifecycle.supports_hibernation);
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let p = HetznerProvider::new();
        let resolver = no_creds();
        let req = ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1");

        let handle = p.provision(&req, &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("hetzner"));
        assert!(handle.id.starts_with("hetzner-nbg1-"));
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
        // ash (US) and sin (APAC) are real Hetzner data centers, but the EU
        // adapter admits neither: the residency filter restricts servers to
        // compliant regions.
        let p = HetznerProvider::new();
        for region in ["ash", "hil", "sin"] {
            let err = p
                .provision(
                    &ProvisionRequest::new("gpu.rtx-4000-ada.1x", region),
                    &no_creds(),
                )
                .await
                .expect_err("non-EU region must be rejected");
            assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
            assert!(!err.is_retryable());
        }
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let err = HetznerProvider::new()
            .provision(&ProvisionRequest::new("nope", "nbg1"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn gpu_offering_in_admitted_non_gpu_region_is_rejected() {
        // Widening residency admits US data centers, but the GEX line is sold
        // only in the GPU-capable EU sites: provisioning it in an admitted-yet-
        // non-GPU region must still be rejected, not silently placed there.
        let p = HetznerProvider::for_residencies(&[Residency::Eu, Residency::Us]);
        assert!(p.capabilities().regions.iter().any(|r| r.code == "ash"));
        let err = p
            .provision(
                &ProvisionRequest::new("gpu.rtx-4000-ada.1x", "ash"),
                &no_creds(),
            )
            .await
            .expect_err("GPU offering not sold in this region");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(p.live_instances().is_empty());
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = HetznerProvider::new();
        let resolver = TestResolver::default().with("hetzner_token", "");
        let req = ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1")
            .with_credentials(SecretRef::new("hetzner_token"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let p = HetznerProvider::new();
        let secret_value = "leak-me-hetzner-token";
        let resolver = TestResolver::default().with("hetzner_token", secret_value);
        let req = ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1")
            .with_credentials(SecretRef::new("hetzner_token"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("hetzner_token"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("hetzner_token"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"api_token\""));
    }

    #[tokio::test]
    async fn describe_never_serializes_non_eu_regions() {
        let describe_json = serde_json::to_string(&HetznerProvider::new().describe()).unwrap();
        // Quoted forms so a code is not matched as a substring of a location
        // name (e.g. "sin" inside "Helsinki").
        assert!(describe_json.contains("\"nbg1\""));
        assert!(!describe_json.contains("\"ash\""));
        assert!(!describe_json.contains("\"hil\""));
        assert!(!describe_json.contains("\"sin\""));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(HetznerProvider::new()));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("hetzner")).unwrap();
        let handle = found
            .provision(
                &ProvisionRequest::new("gpu.rtx-6000-ada.1x", "fsn1"),
                &no_creds(),
            )
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = HetznerProvider::new().with_health(HealthStatus::Degraded {
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
