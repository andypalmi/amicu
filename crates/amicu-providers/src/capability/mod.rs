//! The normalized provider capability descriptor.
//!
//! Every provider adapter exposes a [`ProviderCapabilities`]: a plain-data,
//! serde-serializable snapshot the planner and recommendation engine consume to
//! filter, rank, and validate stacks against residency, budget, availability,
//! and governance constraints. It carries no credentials — only the credential
//! *model* ([`CredentialsModel`]); secret values are resolved at apply time.
//!
//! This module is data models only (TASK-013): no provider trait or adapter
//! logic. It reuses `amicu-core` types where the concept already exists
//! ([`Accelerator`], [`ByteSize`], [`Money`](amicu_core::resource::Money),
//! [`AdapterId`], [`HealthStatus`]) and adds typed [`Region`] / [`Residency`]
//! and pricing / availability values that core did not yet model.

mod availability;
mod compute;
mod pricing;
mod region;

pub use availability::{Availability, AvailabilityState};
pub use compute::{
    ComputeOffering, CredentialsModel, LifecycleBehavior, ModelDownloadPerformance,
    NetworkCapabilities, OrchestrationSupport, ProvisioningMethod, StorageKind, StorageOption,
};
pub use pricing::{BillingGranularity, Pricing, Rate, RateUnit};
pub use region::{Region, Residency};

use amicu_core::adapter::{AdapterId, HealthStatus};
use serde::{Deserialize, Serialize};

/// A provider's normalized capability descriptor.
///
/// Models every attribute the PRD "Provider Capabilities Model" lists: GPU
/// models / VRAM / count and their availability ([`compute`](Self::compute)),
/// regions and data residency ([`regions`](Self::regions)), on-demand vs spot
/// pricing (per offering), storage, network bandwidth, Kubernetes and
/// bare-metal support, provisioning method, credential model, startup /
/// teardown behavior, billing granularity, capacity availability, model
/// download performance, and provider health / incident state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// The provider adapter's identity, e.g. `"hetzner"`.
    pub provider: AdapterId,
    /// Regions the provider offers, each tagged with its residency jurisdiction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<Region>,
    /// Compute shapes (accelerator model / VRAM / count, pricing, availability).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compute: Vec<ComputeOffering>,
    /// Storage options.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<StorageOption>,
    /// Network bandwidth and private-network support.
    #[serde(default)]
    pub network: NetworkCapabilities,
    /// Kubernetes and bare-metal support.
    #[serde(default)]
    pub orchestration: OrchestrationSupport,
    /// How instances are provisioned (API / SSH).
    pub provisioning: ProvisioningMethod,
    /// The credential model (kind only, never values).
    pub credentials: CredentialsModel,
    /// Startup / teardown behavior.
    #[serde(default)]
    pub lifecycle: LifecycleBehavior,
    /// The smallest increment usage is billed in.
    pub billing_granularity: BillingGranularity,
    /// Provider-wide capacity availability.
    pub availability: Availability,
    /// Model download performance.
    #[serde(default)]
    pub model_download: ModelDownloadPerformance,
    /// Provider health and incident state; reuses core [`HealthStatus`].
    pub health: HealthStatus,
}

impl ProviderCapabilities {
    /// A descriptor with the required scalar fields set and every collection
    /// empty. Populate the public fields to build up the full descriptor.
    #[must_use]
    pub fn new(
        provider: AdapterId,
        provisioning: ProvisioningMethod,
        credentials: CredentialsModel,
        billing_granularity: BillingGranularity,
        availability: Availability,
        health: HealthStatus,
    ) -> Self {
        Self {
            provider,
            regions: Vec::new(),
            compute: Vec::new(),
            storage: Vec::new(),
            network: NetworkCapabilities::default(),
            orchestration: OrchestrationSupport::default(),
            provisioning,
            credentials,
            lifecycle: LifecycleBehavior::default(),
            billing_granularity,
            availability,
            model_download: ModelDownloadPerformance::default(),
            health,
        }
    }

    /// Regions residing in the European Union.
    pub fn eu_regions(&self) -> impl Iterator<Item = &Region> {
        self.regions.iter().filter(|r| r.is_eu())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::resource::{Accelerator, AcceleratorKind, Money};

    fn sample() -> ProviderCapabilities {
        let mut caps = ProviderCapabilities::new(
            AdapterId::new("hetzner"),
            ProvisioningMethod::ApiAndSsh,
            CredentialsModel::ApiToken,
            BillingGranularity::PerHour,
            Availability::new(AvailabilityState::Available),
            HealthStatus::Healthy,
        );
        caps.regions = vec![
            Region::new("eu-central-1", Residency::Eu)
                .with_display_name("Falkenstein")
                .with_country("DE"),
            Region::new("us-east-1", Residency::Us),
        ];
        caps.compute = vec![ComputeOffering {
            id: "gpu.h100.1x".into(),
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::Gpu,
                model: "h100".into(),
                count: 1,
                memory: "80GiB".parse().unwrap(),
            }),
            vcpus: Some(32),
            memory: Some("128GiB".parse().unwrap()),
            regions: vec!["eu-central-1".into()],
            pricing: Pricing::on_demand(Rate::new(Money::new(2.5, "EUR"), RateUnit::PerHour))
                .with_spot(Rate::new(Money::new(0.9, "EUR"), RateUnit::PerHour)),
            availability: Availability::new(AvailabilityState::Limited).with_detail("spot only"),
        }];
        caps.storage = vec![
            StorageOption::new(StorageKind::LocalNvme).with_capacity("2TB".parse().unwrap()),
            StorageOption::new(StorageKind::Object),
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
            startup_secs: Some(60),
            teardown_secs: Some(30),
            supports_hibernation: false,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(5_000),
            cached_registry: true,
        };
        caps
    }

    #[test]
    fn descriptor_serde_round_trips() {
        let caps = sample();
        let json = serde_json::to_string_pretty(&caps).unwrap();
        let back: ProviderCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);
    }

    #[test]
    fn descriptor_never_serializes_secret_material() {
        // The credential model names a kind only; no values are present.
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(json.contains("\"credentials\":\"api_token\""));
    }

    #[test]
    fn region_and_residency_are_typed() {
        let region = Region::new("eu-central-1", Residency::Eu);
        assert!(region.is_eu());
        assert!(Residency::Eu.is_eu());
        assert!(!Residency::Us.is_eu());

        // Residency round-trips, including the escape-hatch variant.
        assert_eq!(serde_json::to_string(&Residency::Eu).unwrap(), "\"eu\"");
        let other = Residency::Other("antarctica".into());
        let json = serde_json::to_string(&other).unwrap();
        assert_eq!(serde_json::from_str::<Residency>(&json).unwrap(), other);

        let eu: Vec<_> = sample().eu_regions().map(|r| r.code.clone()).collect();
        assert_eq!(eu, ["eu-central-1"]);
    }

    #[test]
    fn pricing_exposes_on_demand_and_spot() {
        let caps = sample();
        let pricing = &caps.compute[0].pricing;
        assert!(pricing.has_spot());
        assert_eq!(pricing.on_demand_rate().price, Money::new(2.5, "EUR"));
        assert_eq!(pricing.on_demand_rate().unit, RateUnit::PerHour);
        assert_eq!(pricing.spot_rate().unwrap().price, Money::new(0.9, "EUR"));
        assert_eq!(pricing.spot_rate().unwrap().unit, RateUnit::PerHour);

        // On-demand-only pricing exposes no spot rate.
        let on_demand_only =
            Pricing::on_demand(Rate::new(Money::new(1.0, "USD"), RateUnit::PerHour));
        assert!(!on_demand_only.has_spot());
        assert!(on_demand_only.spot_rate().is_none());
    }

    #[test]
    fn availability_state_is_typed() {
        assert!(Availability::new(AvailabilityState::Available).is_available());
        let limited = Availability::new(AvailabilityState::Limited).with_detail("spot only");
        assert!(!limited.is_available());
        assert_eq!(limited.detail.as_deref(), Some("spot only"));
    }
}
