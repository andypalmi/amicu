//! The AWS [`Provider`] adapter, with EU data-residency filtering.
//!
//! AWS is a global hyperscaler offering GPU EC2 instances, managed Kubernetes
//! (EKS) and bare-metal hosts across regions on every continent. It is a
//! secondary/global provider, not EU-first: it has both EU and non-EU regions.
//! Amicu targets EU workloads, so this adapter is **residency filtering by
//! construction**: [`AwsProvider::new`] admits only the EU regions from
//! [`regions`], and [`provision`](AwsProvider::provision) rejects any region
//! outside that admitted set with [`ProviderError::UnknownRegion`] - an instance
//! can never be placed in a non-EU region (`us-east-1`, `ap-southeast-1`, …)
//! through this adapter. [`AwsProvider::for_residencies`] widens the admitted set
//! explicitly when a caller genuinely needs other jurisdictions.
//!
//! The [`ProviderCapabilities`] descriptor carries AWS's EU regions and a
//! representative slice of its GPU EC2 catalog. AWS runs a real spot market, so
//! offerings are priced **on-demand and spot** ([`Pricing::with_spot`]). Live
//! provisioning against the EC2 API and real-time capacity/pricing sync are out
//! of scope here (the latter is amicu-planning#23); the provisioning surface simulates the
//! instance lifecycle in memory, mirroring
//! [`FakeProvider`](crate::fake::FakeProvider), so the planner and apply path can
//! be exercised without network or credentials. Prices are illustrative catalog
//! values in USD/hr, not a live quote.

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

/// The adapter identity AWS registers under.
pub const PROVIDER_ID: &str = "aws";

/// An AWS region: its region code, residency jurisdiction, ISO country code, and
/// human-readable location. Both EU and non-EU entries are listed so the EU
/// filter has something to exclude.
struct AwsRegion {
    /// AWS region code, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
}

/// AWS's regions. EU entries are what [`AwsProvider::new`] admits; the non-EU
/// entries exist so the residency filter is a real exclusion, not a hand-picked
/// list. Uses a `fn` (not a `const`) because [`Residency::Other`] owns a
/// `String`.
fn regions() -> [AwsRegion; 8] {
    [
        // EU.
        AwsRegion {
            code: "eu-west-1",
            residency: Residency::Eu,
            country: "IE",
            location: "Ireland",
        },
        AwsRegion {
            code: "eu-central-1",
            residency: Residency::Eu,
            country: "DE",
            location: "Frankfurt",
        },
        AwsRegion {
            code: "eu-north-1",
            residency: Residency::Eu,
            country: "SE",
            location: "Stockholm",
        },
        AwsRegion {
            code: "eu-south-1",
            residency: Residency::Eu,
            country: "IT",
            location: "Milan",
        },
        AwsRegion {
            code: "eu-west-3",
            residency: Residency::Eu,
            country: "FR",
            location: "Paris",
        },
        // Non-EU - excluded by the residency filter.
        AwsRegion {
            code: "us-east-1",
            residency: Residency::Us,
            country: "US",
            location: "N. Virginia",
        },
        AwsRegion {
            code: "us-west-2",
            residency: Residency::Us,
            country: "US",
            location: "Oregon",
        },
        AwsRegion {
            code: "ap-southeast-1",
            residency: Residency::Apac,
            country: "SG",
            location: "Singapore",
        },
    ]
}

/// One entry of AWS's GPU EC2 catalog: an offering id (the EC2 instance type),
/// the accelerator model, GPU count, per-accelerator VRAM in GiB, representative
/// host resources, and illustrative on-demand and spot rates in USD per hour.
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

/// A representative slice of AWS's GPU EC2 catalog. Prices are illustrative
/// catalog values in USD/hr, not a live quote (real-time pricing is amicu-planning#23).
/// Each offering carries both an on-demand and a spot rate - AWS runs a real spot
/// market.
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "p5.48xlarge",
        model: "h100",
        count: 8,
        vram_gib: 80,
        vcpus: 192,
        host_mem_gib: 2048,
        on_demand: 98.32,
        spot: 39.33,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "p4d.24xlarge",
        model: "a100",
        count: 8,
        vram_gib: 40,
        vcpus: 96,
        host_mem_gib: 1152,
        on_demand: 32.77,
        spot: 11.57,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "g6e.xlarge",
        model: "l40s",
        count: 1,
        vram_gib: 48,
        vcpus: 4,
        host_mem_gib: 32,
        on_demand: 1.86,
        spot: 0.74,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "g6.xlarge",
        model: "l4",
        count: 1,
        vram_gib: 24,
        vcpus: 4,
        host_mem_gib: 16,
        on_demand: 0.99,
        spot: 0.40,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "g5.xlarge",
        model: "a10g",
        count: 1,
        vram_gib: 24,
        vcpus: 4,
        host_mem_gib: 16,
        on_demand: 1.01,
        spot: 0.41,
        availability: AvailabilityState::Available,
    },
];

/// The AWS provider adapter.
///
/// Holds the admitted (residency-filtered) region set and the derived capability
/// descriptor. Provisioning is simulated in memory (see the module docs); every
/// simulated instance and its transitions are recorded for observability.
pub struct AwsProvider {
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

impl AwsProvider {
    /// An AWS provider admitting only EU regions.
    ///
    /// This is the residency-safe default: no non-EU region reaches the
    /// descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// An AWS provider admitting the regions whose residency is in `allowed`.
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

    /// The AWS region codes admitted for `residency`.
    #[must_use]
    pub fn region_codes(residency: &Residency) -> Vec<&'static str> {
        regions()
            .into_iter()
            .filter(|r| &r.residency == residency)
            .map(|r| r.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. Every
    /// offering is sold in every admitted region (AWS's GPU catalog is
    /// region-agnostic at this granularity).
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let region_codes: Vec<String> = regions.iter().map(|r| r.code.clone()).collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::ApiAndSsh,
            CredentialsModel::ApiKey,
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
                // On-demand plus a spot rate: AWS runs a real spot market.
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
            managed_kubernetes: true, // EKS
            bare_metal: true,         // .metal instances
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(90),
            teardown_secs: Some(30),
            supports_hibernation: true,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(20_000),
            cached_registry: true, // ECR
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
        self.state.lock().expect("aws provider mutex poisoned")
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

    /// Resolve and (pretend to) use the credential to authenticate the EC2 API
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
                    detail: "empty AWS access key".into(),
                },
            ));
        }
        let _signed = value.expose_secret().len();
        Ok(())
    }
}

impl Default for AwsProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for AwsProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for AwsProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for AwsProvider {
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
        // instance. A real AWS region outside the admitted set - us-east-1,
        // ap-southeast-1, … - is rejected here.
        if !self.admits_region(&request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        self.authenticate(resolver, request.credentials.as_ref())
            .await?;

        let mut state = self.lock();
        state.counter += 1;
        let id = format!("aws-{}-{}", request.region, state.counter);

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
        let caps = AwsProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("aws"));
        assert!(!caps.regions.is_empty());
        // Every admitted region is EU...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU regions are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"eu-west-1"));
        assert!(codes.contains(&"eu-central-1"));
        // No non-EU AWS region leaks into the descriptor.
        assert!(!codes.contains(&"us-east-1"));
        assert!(!codes.contains(&"ap-southeast-1"));
    }

    #[test]
    fn filter_excludes_non_eu_regions() {
        // The underlying catalog genuinely contains non-EU regions, so the EU
        // default is an exclusion, not a hand-picked list.
        assert!(AwsProvider::region_codes(&Residency::Us).contains(&"us-east-1"));
        assert!(AwsProvider::region_codes(&Residency::Apac).contains(&"ap-southeast-1"));
        let widened = AwsProvider::for_residencies(&[Residency::Us]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        // Widening to the US admits the US regions and nothing else.
        assert!(codes.iter().any(|c| c == "us-east-1"));
        assert!(codes.iter().any(|c| c == "us-west-2"));
        assert!(!codes.iter().any(|c| c == "eu-west-1"));
        assert!(!codes.iter().any(|c| c == "ap-southeast-1"));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = AwsProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_on_demand_and_spot_pricing() {
        let caps = AwsProvider::new().capabilities();
        let p5 = caps
            .compute
            .iter()
            .find(|o| o.id == "p5.48xlarge")
            .expect("p5 offering present");
        // AWS runs a spot market: both rates are exposed.
        assert!(p5.pricing.has_spot());
        assert_eq!(p5.pricing.on_demand_rate().price.currency, "USD");
        assert_eq!(p5.pricing.spot_rate().unwrap().price.currency, "USD");
        // Spot is cheaper than on-demand.
        assert!(
            p5.pricing.spot_rate().unwrap().price.amount < p5.pricing.on_demand_rate().price.amount
        );
        // H100 capacity is constrained.
        assert!(!p5.availability.is_available());
        // Every offering is sold in every admitted (EU) region.
        assert_eq!(p5.regions.len(), caps.regions.len());
    }

    #[test]
    fn multi_gpu_offering_exposes_count() {
        let caps = AwsProvider::new().capabilities();
        let p5 = caps
            .compute
            .iter()
            .find(|o| o.id == "p5.48xlarge")
            .expect("p5 offering present");
        let accel = p5.accelerator.as_ref().expect("accelerator present");
        assert_eq!(accel.count, 8);
        assert_eq!(accel.model, "h100");
        // Memory is per accelerator (80 GiB per H100), not the aggregate.
        assert_eq!(accel.memory, "80GiB".parse().unwrap());
    }

    #[test]
    fn descriptor_models_aws_operational_facts() {
        let caps = AwsProvider::new().capabilities();
        assert_eq!(caps.provisioning, ProvisioningMethod::ApiAndSsh);
        assert_eq!(caps.credentials, CredentialsModel::ApiKey);
        // EC2 bills per-second after a one-minute minimum.
        assert_eq!(caps.billing_granularity, BillingGranularity::PerSecond);
        assert!(caps.network.private_network);
        assert!(caps.orchestration.managed_kubernetes); // EKS
        assert!(caps.orchestration.bare_metal); // .metal instances
        let kinds: Vec<StorageKind> = caps.storage.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StorageKind::Block)); // EBS
        assert!(kinds.contains(&StorageKind::Object)); // S3
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let p = AwsProvider::new();
        let resolver = no_creds();
        let req = ProvisionRequest::new("g6.xlarge", "eu-central-1");

        let handle = p.provision(&req, &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("aws"));
        assert!(handle.id.starts_with("aws-eu-central-1-"));
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
        // us-east-1 is a real AWS region, but the EU adapter does not admit it.
        let p = AwsProvider::new();
        let err = p
            .provision(
                &ProvisionRequest::new("g6.xlarge", "us-east-1"),
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
        let err = AwsProvider::new()
            .provision(&ProvisionRequest::new("nope", "eu-west-1"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = AwsProvider::new();
        let resolver = TestResolver::default().with("aws_access_key", "");
        let req = ProvisionRequest::new("g6.xlarge", "eu-west-1")
            .with_credentials(SecretRef::new("aws_access_key"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let p = AwsProvider::new();
        let secret_value = "leak-me-aws-access-key";
        let resolver = TestResolver::default().with("aws_access_key", secret_value);
        let req = ProvisionRequest::new("g6.xlarge", "eu-west-1")
            .with_credentials(SecretRef::new("aws_access_key"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("aws_access_key"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("aws_access_key"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"api_key\""));
    }

    #[tokio::test]
    async fn describe_never_serializes_non_eu_regions() {
        let describe_json = serde_json::to_string(&AwsProvider::new().describe()).unwrap();
        assert!(describe_json.contains("eu-west-1"));
        assert!(!describe_json.contains("us-east-1"));
        assert!(!describe_json.contains("ap-southeast-1"));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(AwsProvider::new()));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("aws")).unwrap();
        let handle = found
            .provision(
                &ProvisionRequest::new("g5.xlarge", "eu-north-1"),
                &no_creds(),
            )
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = AwsProvider::new().with_health(HealthStatus::Degraded {
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
