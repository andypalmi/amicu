//! The Nebius [`Provider`] adapter, with EU data-residency filtering.
//!
//! Nebius is a European GPU cloud (managed Kubernetes and bare-metal GPU
//! instances) with its flagship region in Finland. Amicu targets EU workloads,
//! so this adapter is **residency filtering by construction**:
//! [`NebiusProvider::new`] admits only the EU regions from [`REGIONS`], and
//! [`provision`](NebiusProvider::provision) rejects any region outside that
//! admitted set with [`ProviderError::UnknownRegion`] - an instance can never be
//! placed in a non-compliant region through this adapter.
//! [`NebiusProvider::for_residencies`] widens the admitted set explicitly when a
//! caller genuinely needs other jurisdictions.
//!
//! The [`ProviderCapabilities`] descriptor carries Nebius's real EU regions and a
//! representative slice of its high-end GPU catalog with on-demand and
//! interruptible (spot) pricing. Live provisioning against the Nebius API and
//! real-time capacity/pricing sync are out of scope here; the provisioning
//! surface simulates the instance lifecycle in memory, mirroring
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
    ResourceIdentity, TeardownOutcome,
};

/// The adapter identity Nebius registers under.
pub const PROVIDER_ID: &str = "nebius";

/// A Nebius region: its id, residency jurisdiction, ISO country code, and
/// human-readable location. Both EU and non-EU entries are listed so the EU
/// filter has something to exclude.
struct NebiusRegion {
    /// Nebius region id, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
}

/// Nebius's regions. EU entries are what [`NebiusProvider::new`] admits; the
/// non-EU entry exists so the residency filter is a real exclusion, not a
/// hand-picked list. Uses a `fn` (not a `const`) because [`Residency::Other`]
/// owns a `String`.
fn regions() -> [NebiusRegion; 3] {
    [
        // EU.
        NebiusRegion {
            code: "eu-north1",
            residency: Residency::Eu,
            country: "FI",
            location: "Finland",
        },
        NebiusRegion {
            code: "eu-west1",
            residency: Residency::Eu,
            country: "FR",
            location: "Paris",
        },
        // Non-EU - excluded by the residency filter.
        NebiusRegion {
            code: "us-central1",
            residency: Residency::Us,
            country: "US",
            location: "Kansas City",
        },
    ]
}

/// One entry of Nebius's GPU catalog: an offering id, the accelerator model as
/// Nebius names it, its VRAM in GiB, representative host resources, and
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

/// A representative slice of Nebius's high-end single-GPU catalog. Prices are
/// illustrative catalog values in USD/hr, not a live quote.
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "gpu.h200-141gb.1x",
        model: "h200-sxm-141gb",
        vram_gib: 141,
        vcpus: 16,
        host_mem_gib: 200,
        on_demand: 3.50,
        spot: 2.30,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "gpu.h100-80gb.1x",
        model: "h100-sxm-80gb",
        vram_gib: 80,
        vcpus: 16,
        host_mem_gib: 200,
        on_demand: 2.95,
        spot: 1.95,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.l40s-48gb.1x",
        model: "l40s-48gb",
        vram_gib: 48,
        vcpus: 8,
        host_mem_gib: 96,
        on_demand: 1.55,
        spot: 0.99,
        availability: AvailabilityState::Available,
    },
];

/// The Nebius provider adapter.
///
/// Holds the admitted (residency-filtered) region set and the derived capability
/// descriptor. Provisioning is simulated in memory (see the module docs); every
/// simulated instance and its transitions are recorded for observability.
pub struct NebiusProvider {
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

impl NebiusProvider {
    /// A Nebius provider admitting only EU regions.
    ///
    /// This is the residency-safe default: no non-EU region reaches the
    /// descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// A Nebius provider admitting the regions whose residency is in `allowed`.
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

    /// The Nebius region codes admitted for `residency`.
    #[must_use]
    pub fn region_codes(residency: &Residency) -> Vec<&'static str> {
        regions()
            .into_iter()
            .filter(|r| &r.residency == residency)
            .map(|r| r.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. Every
    /// offering is sold in every admitted region (Nebius's GPU catalog is
    /// region-agnostic at this granularity).
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let region_codes: Vec<String> = regions.iter().map(|r| r.code.clone()).collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::Api,
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
                    count: 1,
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
            bandwidth_mbps: Some(100_000),
            private_network: true,
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(60),
            teardown_secs: Some(20),
            supports_hibernation: false,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(10_000),
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
        self.state.lock().expect("nebius provider mutex poisoned")
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

    /// Resolve and (pretend to) use the credential to authenticate the Nebius
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
                    detail: "empty Nebius service-account key".into(),
                },
            ));
        }
        let _signed = value.expose_secret().len();
        Ok(())
    }
}

impl Default for NebiusProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for NebiusProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for NebiusProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for NebiusProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn prepare(
        &self,
        request: &ProvisionRequest,
        identity: ResourceIdentity,
    ) -> Result<ProvisioningHandle, AdapterError> {
        if !self.has_offering(&request.offering_id) {
            return Err(AdapterError::from_kind(ProviderError::UnknownOffering {
                offering: request.offering_id.clone(),
            }));
        }
        // Residency filter: only admitted (EU by default) regions can host an
        // instance. A real Nebius region outside the admitted set is rejected here.
        if !self.admits_region(&request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        Ok(ProvisioningHandle::prepared(
            self.id.clone(),
            request,
            identity,
        ))
    }

    async fn provision(
        &self,
        prepared: ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningHandle, AdapterError> {
        self.authenticate(resolver, prepared.credentials.as_ref())
            .await?;

        let mut state = self.lock();
        // Idempotent: a retried provision adopts the instance already created
        // under this identity instead of duplicating it.
        if let Some(existing) = crate::rest_lifecycle::adopt_in_memory(&state.instances, &prepared)?
        {
            return Ok(existing);
        }
        state.counter += 1;
        let id = format!("nebius-{}-{}", prepared.region, state.counter);

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
        let handle = prepared.completed("instance_id", id.clone(), ready);
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
    ) -> Result<TeardownOutcome, AdapterError> {
        self.authenticate(resolver, handle.credentials.as_ref())
            .await?;

        let mut state = self.lock();
        let keys = crate::rest_lifecycle::matching_keys(&state.instances, handle);
        // An in-memory simulator that has never seen the record cannot vouch
        // that the resource is gone (its memory may simply have been lost).
        if keys.is_empty() {
            return Ok(TeardownOutcome::Unconfirmed {
                reason: "instance unknown to this simulator; absence cannot be confirmed".into(),
            });
        }
        for key in keys {
            let instance = state.instances.get_mut(&key).expect("key just matched");
            if matches!(instance.state, ProvisioningState::Terminated { .. }) {
                continue;
            }
            instance.state = ProvisioningState::Terminated {
                detail: "instance terminated".into(),
            };
            let entry = state.history.entry(key.clone()).or_default();
            entry.push(ProvisioningState::Stopping {
                detail: "instance stopping".into(),
            });
            entry.push(ProvisioningState::Terminated {
                detail: "instance terminated".into(),
            });
        }
        Ok(TeardownOutcome::ConfirmedGone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProvisionCompat;
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
        let caps = NebiusProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("nebius"));
        assert_ne!(caps.regions, Vec::new());
        // Every admitted region is EU...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU regions are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"eu-north1"));
        assert!(codes.contains(&"eu-west1"));
        // No non-EU Nebius region leaks into the descriptor.
        assert!(!codes.contains(&"us-central1"));
    }

    #[test]
    fn filter_excludes_non_eu_regions() {
        // The underlying catalog genuinely contains a non-EU region, so the EU
        // default is an exclusion, not a hand-picked list.
        assert!(NebiusProvider::region_codes(&Residency::Us).contains(&"us-central1"));
        let widened = NebiusProvider::for_residencies(&[Residency::Eu, Residency::Us]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        assert!(codes.iter().any(|c| c == "eu-north1"));
        assert!(codes.iter().any(|c| c == "us-central1"));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = NebiusProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_high_end_gpu_with_on_demand_and_spot_pricing() {
        let caps = NebiusProvider::new().capabilities();
        let h200 = caps
            .compute
            .iter()
            .find(|o| o.id == "gpu.h200-141gb.1x")
            .expect("h200 offering present");
        assert!(h200.pricing.has_spot());
        assert_eq!(h200.pricing.on_demand_rate().price.currency, "USD");
        // Every offering is sold in every admitted (EU) region.
        assert_eq!(h200.regions.len(), caps.regions.len());
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let p = NebiusProvider::new();
        let resolver = no_creds();
        let req = ProvisionRequest::new("gpu.h100-80gb.1x", "eu-north1");

        let handle = p.provision_request(&req, &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("nebius"));
        assert!(handle.id.starts_with("nebius-eu-north1-"));
        assert_eq!(p.live_instances().len(), 1);

        let history = p.instance_history(&handle.id);
        assert!(matches!(history[0], ProvisioningState::Requested));
        assert!(history.last().unwrap().is_ready());

        assert!(
            p.teardown(&handle, &resolver)
                .await
                .unwrap()
                .is_confirmed_gone()
        );
        assert_eq!(p.live_instances(), Vec::new());
        assert!(p.status(&handle, &resolver).await.unwrap().is_terminal());
    }

    #[tokio::test]
    async fn provision_in_non_eu_region_is_rejected() {
        // us-central1 is a real Nebius region, but the EU adapter does not admit
        // it: the residency filter restricts instances to compliant regions.
        let p = NebiusProvider::new();
        let err = p
            .provision_request(
                &ProvisionRequest::new("gpu.h100-80gb.1x", "us-central1"),
                &no_creds(),
            )
            .await
            .expect_err("non-EU region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
        assert_eq!(p.live_instances(), Vec::new());
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let err = NebiusProvider::new()
            .provision_request(&ProvisionRequest::new("nope", "eu-north1"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = NebiusProvider::new();
        let resolver = TestResolver::default().with("nebius_key", "");
        let req = ProvisionRequest::new("gpu.h100-80gb.1x", "eu-north1")
            .with_credentials(SecretRef::new("nebius_key"));
        let err = p
            .provision_request(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let p = NebiusProvider::new();
        let secret_value = "leak-me-nebius-key";
        let resolver = TestResolver::default().with("nebius_key", secret_value);
        let req = ProvisionRequest::new("gpu.h100-80gb.1x", "eu-north1")
            .with_credentials(SecretRef::new("nebius_key"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("nebius_key"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision_request(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("nebius_key"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"service_account\""));
    }

    #[tokio::test]
    async fn describe_never_serializes_non_eu_regions() {
        let describe_json = serde_json::to_string(&NebiusProvider::new().describe()).unwrap();
        assert!(describe_json.contains("eu-north1"));
        assert!(!describe_json.contains("us-central1"));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(NebiusProvider::new()));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("nebius")).unwrap();
        let handle = found
            .provision_request(
                &ProvisionRequest::new("gpu.l40s-48gb.1x", "eu-west1"),
                &no_creds(),
            )
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = NebiusProvider::new().with_health(HealthStatus::Degraded {
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
