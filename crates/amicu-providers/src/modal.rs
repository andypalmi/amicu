//! The Modal [`Provider`] adapter: a **catalog entry only**.
//!
//! Modal (modal.com) has no REST API; the only supported way in is the
//! Sandbox API over gRPC, which is out of scope here.
//! This adapter therefore carries a real [`ProviderCapabilities`] GPU catalog,
//! so the wizard/recommend engine can offer Modal as a candidate and cost
//! estimation can quote it, but [`provision`](ModalProvider::provision),
//! [`status`](ModalProvider::status) and [`teardown`](ModalProvider::teardown)
//! all reject with [`ProviderError::Backend`]: there is no transport behind
//! them yet, fake or otherwise. Modal deployments do not go through this
//! trait's provisioning surface: `amicu apply` routes them through
//! `amicu-modal`'s gRPC Sandbox supervisor (`ModalSupervisor`) directly.
//!
//! Modal *is* regionalized: a caller can pin a Function/Sandbox to a broad
//! region (`"us"`, `"eu"`, `"ap"`) or a narrower one (`"us-west"`,
//! `"eu-north"`, `"uk"`, `"jp"`, ...) via the `region`/`routing_region`
//! argument - it is a placement preference with a price multiplier, not a
//! capacity gate (every GPU is assumed available in every region; Modal does
//! not document per-region GPU availability). [`regions`] models the region
//! table the same way [`RunPodProvider`](crate::RunPodProvider)'s internal
//! `datacenters` does: a real list of codes tagged with their
//! [`Residency`], read from
//! <https://modal.com/docs/guide/region-selection> on 2026-09-18. This
//! means `"eu"` (and every EU narrow region) resolves to [`Residency::Eu`]
//! through [`catalog::resolve_residency`](crate::catalog::resolve_residency),
//! so a `data_class` requiring EU residency is satisfiable against Modal
//! when the deployment is pinned there, and fails closed only for a region
//! code Modal genuinely does not serve.
//!
//! Pinning to a region multiplies Modal's base on-demand rate: broad regions
//! by 1.15x, narrow regions by 1.75x (same source, checked 2026-09-18:
//! "Broad (e.g. `us`) | 1.15x", "Narrow (e.g. `us-west`) | 1.75x"; the
//! smaller multiplier applies when multiple regions spanning both
//! categories are given). The stored `on_demand` rate is Modal's *unpinned* base
//! price; every offering also carries the per-region multipliers in
//! [`Pricing::region_multipliers`] (see [`region_multiplier`]), which
//! [`estimate_cost`](crate::estimate_cost) applies when the usage pins a
//! region.
//!
//! GPU models, VRAM and on-demand USD/hr prices were read from
//! <https://modal.com/pricing> on 2026-09-18 (Modal bills per second; the
//! catalog's [`RateUnit::PerHour`] rates are that per-second price times
//! 3600, matching [`BillingGranularity::PerSecond`] on the descriptor).
//! Modal runs no spot/interruptible market, so every offering carries only an
//! on-demand rate. `memory_bandwidth_gbps` is filled only where a single,
//! unambiguous per-GPU figure could be confirmed on an nvidia.com product
//! page; it is `None` for A100 (nvidia.com publishes different PCIe/SXM
//! figures and Modal does not document which form factor it runs) and B200
//! (nvidia.com publishes only aggregate 8-GPU HGX system figures, not a
//! per-GPU number). Prices are illustrative catalog values, not a live quote,
//! same convention as every other adapter here.

use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_core::resource::{Accelerator, AcceleratorKind, Money};
use amicu_core::secret::SecretResolver;
use async_trait::async_trait;

use crate::capability::{
    Availability, AvailabilityState, BillingGranularity, ComputeOffering, CredentialsModel,
    Pricing, ProviderCapabilities, ProvisioningMethod, Rate, RateUnit, Region, Residency,
    StorageKind, StorageOption,
};
use crate::provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
    ResourceIdentity, TeardownOutcome,
};

/// The adapter identity Modal registers under.
pub const PROVIDER_ID: &str = "modal";

/// One region Modal's `region`/`routing_region` argument accepts: its code,
/// residency jurisdiction, and a human-readable location.
struct ModalRegion {
    /// Region code, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the region falls under.
    residency: Residency,
    /// Human-readable location.
    location: &'static str,
}

/// A compact residency tag for [`RAW_REGIONS`], expanded to a real
/// [`Residency`] in [`regions`]. Kept separate from `Residency` itself
/// because [`Residency::Other`] owns a `String`, which cannot appear in a
/// `const` table row; `Other` here holds the `'static` jurisdiction token
/// instead.
#[derive(Clone, Copy)]
enum ResidencyTag {
    Us,
    Eu,
    Apac,
    Uk,
    Ca,
    Other(&'static str),
}

/// Every region Modal's `region=`/`routing_region=` argument accepts: three
/// broad regions (`"us"`, `"eu"`, `"ap"`, 1.15x price multiplier) and the
/// narrow regions each broad region contains (1.75x multiplier), as
/// `(code, residency, location)`. Source:
/// <https://modal.com/docs/guide/region-selection>, checked 2026-09-18.
/// `"me"`/`"sa"`/`"af"`/`"mx"` (Middle East / South America / Africa /
/// Mexico) have no matching named [`Residency`] variant, so they use
/// [`ResidencyTag::Other`] with the region code as the jurisdiction token.
const RAW_REGIONS: [(&str, ResidencyTag, &str); 22] = [
    // Broad regions (1.15x multiplier).
    ("us", ResidencyTag::Us, "United States (broad)"),
    ("eu", ResidencyTag::Eu, "European Economic Area (broad)"),
    ("ap", ResidencyTag::Apac, "Asia-Pacific (broad)"),
    // Narrow regions (1.75x multiplier).
    ("us-east", ResidencyTag::Us, "United States - Virginia"),
    ("us-central", ResidencyTag::Us, "United States - Central"),
    ("us-south", ResidencyTag::Us, "United States - South"),
    ("us-west", ResidencyTag::Us, "United States - Oregon"),
    (
        "eu-west",
        ResidencyTag::Eu,
        "European Economic Area - Dublin",
    ),
    (
        "eu-north",
        ResidencyTag::Eu,
        "European Economic Area - North",
    ),
    (
        "eu-south",
        ResidencyTag::Eu,
        "European Economic Area - South",
    ),
    ("uk", ResidencyTag::Uk, "United Kingdom"),
    ("ca", ResidencyTag::Ca, "Canada"),
    (
        "ap-northeast",
        ResidencyTag::Apac,
        "Asia-Pacific - Northeast",
    ),
    (
        "ap-southeast",
        ResidencyTag::Apac,
        "Asia-Pacific - Southeast",
    ),
    ("ap-south", ResidencyTag::Apac, "Asia-Pacific - Mumbai"),
    (
        "ap-melbourne",
        ResidencyTag::Apac,
        "Asia-Pacific - Melbourne",
    ),
    ("jp", ResidencyTag::Apac, "Japan"),
    ("au", ResidencyTag::Apac, "Australia"),
    ("me", ResidencyTag::Other("me"), "Middle East"),
    ("sa", ResidencyTag::Other("sa"), "South America"),
    ("af", ResidencyTag::Other("af"), "Africa"),
    ("mx", ResidencyTag::Other("mx"), "Mexico"),
];

/// Expand [`RAW_REGIONS`] into typed [`ModalRegion`]s. A `fn` (not a
/// `const`) because [`Residency::Other`] owns a `String` - same reason
/// [`RunPodProvider`](crate::RunPodProvider)'s internal `datacenters` is a
/// `fn`.
fn regions() -> [ModalRegion; 22] {
    RAW_REGIONS.map(|(code, tag, location)| ModalRegion {
        code,
        residency: match tag {
            ResidencyTag::Us => Residency::Us,
            ResidencyTag::Eu => Residency::Eu,
            ResidencyTag::Apac => Residency::Apac,
            ResidencyTag::Uk => Residency::Uk,
            ResidencyTag::Ca => Residency::Ca,
            ResidencyTag::Other(token) => Residency::Other(token.to_string()),
        },
        location,
    })
}

/// The proxy-region fallback: Modal's own default for `@app.server`
/// (`routing_region="us-east"`, `modal/app.py`), used when a deployment pins no
/// region or one with no verified proxy equivalent.
pub const DEFAULT_PROXY_REGION: &str = "us-east";

/// The proxy ("routing") regions Modal documents for `routing_region=`, which
/// `@app.server` forwards verbatim as `HttpConfig.proxy_regions=[routing_region]`
/// (`modal-client` `py/modal/app.py`): `us-east` (Virginia), `us-west`
/// (Oregon), `eu-west` (Dublin) and `ap-south` (Mumbai). Source:
/// <https://modal.com/docs/guide/region-selection#regional-routing>, checked
/// 2026-10-10. This is a *smaller* set than the compute regions in
/// [`regions`]; a compute region code is not necessarily a valid proxy region.
pub const PROXY_REGIONS: [&str; 4] = ["us-east", "us-west", "eu-west", "ap-south"];

/// Resolves the proxy region for a deployment pinned to compute region
/// `region` (a [`regions`] code), or unpinned (`None`).
///
/// - `us-west` / `eu-west` / `ap-south` map to themselves.
/// - Every other EU region (`eu`, `eu-north`, `eu-south`) maps to `eu-west`,
///   the only EU proxy region, so an EU-pinned deployment is not fronted by a
///   US proxy.
/// - Other `ap` regions (`ap`, `ap-northeast`, `ap-southeast`,
///   `ap-melbourne`, `jp`, `au`) map to `ap-south`, the only APAC proxy region
///   (a latency/locality choice, not an exact location match).
/// - `uk` maps to `eu-west` (Dublin), the nearest proxy region, so a
///   UK-pinned deployment is not fronted by a US proxy.
/// - Everything else falls back to [`DEFAULT_PROXY_REGION`] (`us-east`): `us`
///   and the other US regions, plus `ca`, `me`, `sa`, `af`, `mx`, for
///   which Modal documents no proxy region, and unknown or unpinned values.
///   No proxy region name is invented for those.
///
/// The result is always one of [`PROXY_REGIONS`].
#[must_use]
pub fn modal_proxy_region(region: Option<&str>) -> &'static str {
    match region {
        Some("us-west") => "us-west",
        Some("eu" | "eu-west" | "eu-north" | "eu-south" | "uk") => "eu-west",
        Some(
            "ap" | "ap-south" | "ap-northeast" | "ap-southeast" | "ap-melbourne" | "jp" | "au",
        ) => "ap-south",
        _ => DEFAULT_PROXY_REGION,
    }
}

/// One entry of Modal's GPU catalog: the offering-id slug, the exact string
/// Modal's `gpu=` argument expects (verbatim - PR 2's sandbox supervisor
/// passes it through, optionally suffixed `:<count>`), per-accelerator VRAM,
/// the maximum GPU count Modal allows in one container, the illustrative
/// on-demand USD/hr rate for a single GPU, and a verified memory-bandwidth
/// figure when one is unambiguously available.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    vram_gib: u32,
    /// Maximum GPUs of this kind per Modal container (`gpu="<model>:<max_count>"`).
    max_count: u32,
    per_gpu_hourly: f64,
    memory_bandwidth_gbps: Option<u32>,
    availability: AvailabilityState,
}

/// Modal's on-demand GPU catalog. Prices are USD/hr, derived from the
/// per-second rate at <https://modal.com/pricing> (checked 2026-09-18) times
/// 3600:
///
/// | GPU          | $/s (2026-09-18) | $/hr    |
/// |--------------|-------------------|---------|
/// | T4           | 0.000164          | 0.5904  |
/// | L4           | 0.000222          | 0.7992  |
/// | A10          | 0.000306          | 1.1016  |
/// | L40S         | 0.000542          | 1.9512  |
/// | A100-40GB    | 0.000583          | 2.0988  |
/// | A100-80GB    | 0.000694          | 2.4984  |
/// | H100 SXM5    | 0.001097          | 3.9492  |
/// | H200 SXM     | 0.001261          | 4.5396  |
/// | B200         | 0.001736          | 6.2496  |
/// | RTX PRO 6000 | 0.000842          | 3.0312  |
///
/// GPU set, VRAM and max per-container count from Modal's GPU guide
/// (`modal.com/docs/guide/gpu`, via `ctx7`, checked 2026-09-18): T4, L4, A10,
/// L40S, A100 (40GB/80GB), RTX-PRO-6000, H100, H200 and B200 are all offered;
/// B300, B200, H200, H100, A100, L4, T4 and L40S support up to 8 GPUs per
/// container, A10 up to 4, and RTX-PRO-6000 is single-GPU only (not listed
/// among the multi-GPU models). B300 is out of scope (not requested).
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "t4",
        model: "T4",
        vram_gib: 16,
        max_count: 8,
        per_gpu_hourly: 0.5904,
        // NVIDIA Tesla T4 product page: 320+ GB/s.
        memory_bandwidth_gbps: Some(320),
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "l4",
        model: "L4",
        vram_gib: 24,
        max_count: 8,
        per_gpu_hourly: 0.7992,
        // NVIDIA L4 product page: 300 GB/s.
        memory_bandwidth_gbps: Some(300),
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "a10",
        model: "A10",
        vram_gib: 24,
        max_count: 4,
        per_gpu_hourly: 1.1016,
        // NVIDIA A10 product page: 600 GB/s.
        memory_bandwidth_gbps: Some(600),
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "l40s",
        model: "L40S",
        vram_gib: 48,
        max_count: 8,
        per_gpu_hourly: 1.9512,
        // NVIDIA L40S product page: 864 GB/s.
        memory_bandwidth_gbps: Some(864),
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "a100-40gb",
        model: "A100-40GB",
        vram_gib: 40,
        max_count: 8,
        per_gpu_hourly: 2.0988,
        // Not filled: nvidia.com's A100 page details only the 80GB variants.
        memory_bandwidth_gbps: None,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "a100-80gb",
        model: "A100-80GB",
        vram_gib: 80,
        max_count: 8,
        per_gpu_hourly: 2.4984,
        // Not filled: nvidia.com lists 1,935 GB/s (PCIe) and 2,039 GB/s
        // (SXM) for the 80GB card; Modal does not document which form
        // factor backs this SKU, so no single figure is verifiable.
        memory_bandwidth_gbps: None,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "h100",
        model: "H100",
        vram_gib: 80,
        max_count: 8,
        per_gpu_hourly: 3.9492,
        // NVIDIA H100 SXM product page: 3.35 TB/s.
        memory_bandwidth_gbps: Some(3_350),
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "h200",
        model: "H200",
        vram_gib: 141,
        max_count: 8,
        per_gpu_hourly: 4.5396,
        // NVIDIA H200 product page: 4.8 TB/s.
        memory_bandwidth_gbps: Some(4_800),
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "b200",
        model: "B200",
        vram_gib: 180,
        max_count: 8,
        per_gpu_hourly: 6.2496,
        // Not filled: nvidia.com publishes only aggregate 8-GPU HGX B200
        // system figures, not a per-GPU bandwidth number.
        memory_bandwidth_gbps: None,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "rtx-pro-6000",
        model: "RTX-PRO-6000",
        vram_gib: 96,
        max_count: 1,
        per_gpu_hourly: 3.0312,
        // NVIDIA RTX PRO 6000 Blackwell Workstation Edition product page:
        // 1792 GB/s.
        memory_bandwidth_gbps: Some(1_792),
        availability: AvailabilityState::Available,
    },
];

/// Modal's price multiplier for a broad region (`us`, `eu`, `ap`).
const BROAD_REGION_MULTIPLIER: f64 = 1.15;
/// Modal's price multiplier for a narrow region (every other code in
/// [`RAW_REGIONS`]).
const NARROW_REGION_MULTIPLIER: f64 = 1.75;

/// The multiplier Modal charges over its base rate when a function is pinned
/// to `code`: 1.15x broad, 1.75x narrow. Source:
/// <https://modal.com/docs/guide/region-selection>, checked 2026-09-18.
fn region_multiplier(code: &str) -> f64 {
    if matches!(code, "us" | "eu" | "ap") {
        BROAD_REGION_MULTIPLIER
    } else {
        NARROW_REGION_MULTIPLIER
    }
}

/// Build a [`ComputeOffering`] for `g` at `count` GPUs, sold in every region
/// in `region_codes` (Modal does not document per-region GPU availability,
/// so every GPU is assumed available everywhere - same simplifying
/// assumption as [`RunPodProvider`](crate::RunPodProvider) and
/// [`AwsProvider`](crate::AwsProvider) make for their own region-agnostic
/// catalogs). The stored price is Modal's unpinned base rate; the region
/// price multipliers ride along in [`Pricing::region_multipliers`].
fn offering(g: &GpuSpec, count: u32, region_codes: &[String]) -> ComputeOffering {
    ComputeOffering {
        id: format!("gpu.{}.{count}x", g.id),
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: g.model.into(),
            count,
            memory: format!("{}GiB", g.vram_gib).parse().expect("valid size"),
            memory_bandwidth_gbps: g.memory_bandwidth_gbps,
        }),
        // Modal sandboxes size CPU/host memory independently of the GPU
        // request rather than pairing them into a fixed instance type, so
        // there is no representative host shape to report here.
        vcpus: None,
        memory: None,
        regions: region_codes.to_vec(),
        pricing: region_codes.iter().fold(
            Pricing::on_demand(Rate::new(
                Money::new(g.per_gpu_hourly * f64::from(count), "USD"),
                RateUnit::PerHour,
            )),
            |pricing, code| pricing.with_region_multiplier(code.clone(), region_multiplier(code)),
        ),
        availability: Availability::new(g.availability),
    }
}

/// Every apply-time method rejects with this: PR 1 is catalog-only, with no
/// transport behind `provision`/`status`/`teardown` yet. This is permanent
/// (not [`ProviderError::Backend`]'s transient "retry later"): no retry
/// makes an unimplemented operation succeed.
fn not_yet_provisionable() -> AdapterError {
    AdapterError::from_kind(ProviderError::Unsupported {
        detail: "Modal provisioning is not implemented by this catalog-only adapter; \
                 use the Modal sandbox target"
            .into(),
    })
}

/// The Modal provider adapter.
///
/// Holds the derived capability descriptor only - there is no transport, no
/// credential handling and no in-memory simulation, since apply-time methods
/// are unconditional stubs (see the module docs).
pub struct ModalProvider {
    id: AdapterId,
    capabilities: ProviderCapabilities,
    health: HealthStatus,
}

impl ModalProvider {
    /// A Modal provider with the on-demand GPU catalog above, sold in every
    /// region from [`regions`].
    #[must_use]
    pub fn new() -> Self {
        let id = AdapterId::new(PROVIDER_ID);
        let capabilities = Self::capabilities_for(&id);
        Self {
            id,
            capabilities,
            health: HealthStatus::Healthy,
        }
    }

    /// Override the health status (e.g. for tests or a degraded probe result).
    #[must_use]
    pub fn with_health(mut self, health: HealthStatus) -> Self {
        self.capabilities.health = health.clone();
        self.health = health;
        self
    }

    /// Build the capability descriptor across every Modal region. Every GPU
    /// carries a `1x` offering plus, when Modal allows more than one GPU per
    /// container, a second offering at that maximum count; each offering is
    /// sold in every region (see [`offering`]).
    fn capabilities_for(id: &AdapterId) -> ProviderCapabilities {
        let region_list: Vec<Region> = regions()
            .into_iter()
            .map(|r| Region::new(r.code, r.residency).with_display_name(r.location))
            .collect();
        let region_codes: Vec<String> = region_list.iter().map(|r| r.code.clone()).collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::Api,
            // Modal authenticates gRPC calls with a token id + token secret
            // pair (`MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET`), a key pair
            // rather than a single bearer token.
            CredentialsModel::ApiKey,
            BillingGranularity::PerSecond,
            Availability::new(AvailabilityState::Available),
            HealthStatus::Healthy,
        );
        caps.regions = region_list;
        caps.compute = GPUS
            .iter()
            .flat_map(|g| {
                let mut offerings = vec![offering(g, 1, &region_codes)];
                if g.max_count > 1 {
                    offerings.push(offering(g, g.max_count, &region_codes));
                }
                offerings
            })
            .collect();
        caps.storage = vec![StorageOption::new(StorageKind::NetworkFs)];
        caps
    }
}

impl Default for ModalProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for ModalProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for ModalProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for ModalProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn prepare(
        &self,
        _request: &ProvisionRequest,
        _identity: ResourceIdentity,
    ) -> Result<ProvisioningHandle, AdapterError> {
        Err(not_yet_provisionable())
    }

    async fn provision(
        &self,
        _prepared: ProvisioningHandle,
        _resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningHandle, AdapterError> {
        Err(not_yet_provisionable())
    }

    async fn status(
        &self,
        _handle: &ProvisioningHandle,
        _resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningState, AdapterError> {
        Err(not_yet_provisionable())
    }

    async fn teardown(
        &self,
        _handle: &ProvisioningHandle,
        _resolver: &dyn SecretResolver,
    ) -> Result<TeardownOutcome, AdapterError> {
        Err(not_yet_provisionable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProvisionCompat;
    use amicu_core::adapter::Registry;
    use amicu_core::diagnostic::{Category as DiagCategory, DiagnosticCode};
    use amicu_core::secret::{SecretRef, SecretValue};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// A resolver seeded with nothing; errors on any lookup so tests can
    /// prove the stub apply-time methods never touch it.
    #[derive(Default)]
    struct TestResolver {
        secrets: HashMap<String, SecretValue>,
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

    #[test]
    fn proxy_region_is_always_a_documented_proxy_region() {
        for region in regions() {
            let proxy = modal_proxy_region(Some(region.code));
            assert!(PROXY_REGIONS.contains(&proxy), "{} -> {proxy}", region.code);
        }
        assert_eq!(modal_proxy_region(None), "us-east");
        assert_eq!(modal_proxy_region(Some("nowhere")), "us-east");
    }

    #[test]
    fn eu_residency_regions_get_an_eu_proxy() {
        for region in regions().iter().filter(|r| r.residency == Residency::Eu) {
            assert_eq!(modal_proxy_region(Some(region.code)), "eu-west");
        }
        assert_eq!(modal_proxy_region(Some("us-west")), "us-west");
        assert_eq!(modal_proxy_region(Some("jp")), "ap-south");
        assert_eq!(modal_proxy_region(Some("us")), "us-east");
        assert_eq!(modal_proxy_region(Some("uk")), "eu-west");
        assert_eq!(modal_proxy_region(Some("ca")), "us-east");
    }

    #[test]
    fn provider_id_matches_capabilities() {
        assert_eq!(PROVIDER_ID, "modal");
        let caps = ModalProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("modal"));
    }

    #[test]
    fn catalog_is_non_empty_with_unique_offering_ids() {
        let caps = ModalProvider::new().capabilities();
        assert_ne!(caps.compute, Vec::new());
        let mut ids: Vec<&str> = caps.compute.iter().map(|o| o.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), caps.compute.len(), "offering ids must be unique");
    }

    #[test]
    fn every_offering_has_a_positive_usd_on_demand_hourly_rate() {
        let caps = ModalProvider::new().capabilities();
        for offering in &caps.compute {
            let rate = offering.pricing.on_demand_rate();
            assert!(
                rate.price.amount > 0.0,
                "{} must have a positive price",
                offering.id
            );
            assert_eq!(rate.price.currency, "USD");
            assert_eq!(rate.unit, RateUnit::PerHour);
            // Modal runs no interruptible/spot market.
            assert!(!offering.pricing.has_spot());
        }
    }

    #[test]
    fn billing_granularity_is_per_second() {
        assert_eq!(
            ModalProvider::new().capabilities().billing_granularity,
            BillingGranularity::PerSecond
        );
    }

    #[test]
    fn catalog_includes_broad_and_narrow_regions_with_correct_residency() {
        let caps = ModalProvider::new().capabilities();
        assert_eq!(caps.regions.len(), 22);
        let residency_of = |code: &str| -> Residency {
            caps.regions
                .iter()
                .find(|r| r.code == code)
                .unwrap_or_else(|| panic!("region {code} present"))
                .residency
                .clone()
        };
        // Broad regions.
        assert_eq!(residency_of("us"), Residency::Us);
        assert_eq!(residency_of("eu"), Residency::Eu);
        assert_eq!(residency_of("ap"), Residency::Apac);
        // A sample of narrow regions, including ones with no named
        // Residency variant.
        assert_eq!(residency_of("us-west"), Residency::Us);
        assert_eq!(residency_of("eu-west"), Residency::Eu);
        assert_eq!(residency_of("eu-north"), Residency::Eu);
        assert_eq!(residency_of("uk"), Residency::Uk);
        assert_eq!(residency_of("ca"), Residency::Ca);
        assert_eq!(residency_of("jp"), Residency::Apac);
        assert_eq!(residency_of("au"), Residency::Apac);
        assert_eq!(residency_of("mx"), Residency::Other("mx".to_string()));
        // At least one EU-resident region is present, unlike a provider
        // with no EU presence.
        assert!(caps.regions.iter().any(Region::is_eu));
    }

    #[test]
    fn every_offering_is_sold_in_every_region() {
        let caps = ModalProvider::new().capabilities();
        let region_codes: Vec<String> = caps.regions.iter().map(|r| r.code.clone()).collect();
        assert_ne!(region_codes.len(), 0);
        for offering in &caps.compute {
            assert_eq!(offering.regions, region_codes);
        }
    }

    #[test]
    fn multi_gpu_offerings_exist_up_to_vendor_limits() {
        let caps = ModalProvider::new().capabilities();
        let count_of = |id: &str| -> u32 {
            caps.compute
                .iter()
                .find(|o| o.id == id)
                .and_then(|o| o.accelerator.as_ref())
                .expect("offering present")
                .count
        };
        assert_eq!(count_of("gpu.h100.1x"), 1);
        assert_eq!(count_of("gpu.h100.8x"), 8);
        assert_eq!(count_of("gpu.a10.1x"), 1);
        assert_eq!(count_of("gpu.a10.4x"), 4);
        // RTX PRO 6000 is single-GPU only: no multi-GPU offering exists.
        assert!(!caps.compute.iter().any(|o| o.id == "gpu.rtx-pro-6000.4x"));
        assert!(!caps.compute.iter().any(|o| o.id == "gpu.rtx-pro-6000.8x"));
        // A multi-GPU offering's price scales linearly with count.
        let h100_1x = caps.compute.iter().find(|o| o.id == "gpu.h100.1x").unwrap();
        let h100_8x = caps.compute.iter().find(|o| o.id == "gpu.h100.8x").unwrap();
        assert!(
            (h100_8x.pricing.on_demand_rate().price.amount
                - h100_1x.pricing.on_demand_rate().price.amount * 8.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[tokio::test]
    async fn provision_status_teardown_all_reject_as_permanent_unsupported_errors() {
        let p = ModalProvider::new();
        let resolver = TestResolver::default();
        let req = ProvisionRequest::new("gpu.t4.1x", "us");

        let err = p
            .provision_request(&req, &resolver)
            .await
            .expect_err("catalog-only adapter must not provision");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-015");
        // Permanently unimplemented, not a transient backend fault: no
        // retry makes this succeed.
        assert!(!err.is_retryable());

        let mut handle = ProvisioningHandle::prepared(
            AdapterId::new("modal"),
            &ProvisionRequest::new("gpu.t4.1x", "us"),
            ResourceIdentity::new("d", "p"),
        );
        handle.id = "sandbox-1".into();
        let err = p
            .status(&handle, &resolver)
            .await
            .expect_err("catalog-only adapter must not report status");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-015");
        assert!(!err.is_retryable());

        let err = p
            .teardown(&handle, &resolver)
            .await
            .expect_err("catalog-only adapter must not tear down");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-015");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn apply_time_stubs_never_resolve_credentials() {
        // The resolver is seeded with nothing and would return a Policy error
        // if consulted; the returned error is the stub's own Provider/Backend
        // error, proving the resolver was never touched.
        let p = ModalProvider::new();
        let resolver = TestResolver::default();
        let req = ProvisionRequest::new("gpu.t4.1x", "us")
            .with_credentials(SecretRef::new("modal_token"));
        let err = p.provision_request(&req, &resolver).await.unwrap_err();
        assert_eq!(err.code().category(), DiagCategory::Provider);
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(ModalProvider::new()));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("modal")).unwrap();
        assert!(
            found
                .capabilities()
                .compute
                .iter()
                .any(|o| o.id == "gpu.h100.1x")
        );
        assert!(found.health().await.unwrap().is_healthy());
    }

    #[test]
    fn health_override_is_reported_and_in_descriptor() {
        let p = ModalProvider::new().with_health(HealthStatus::Degraded {
            detail: "maintenance".into(),
        });
        assert!(matches!(
            p.capabilities().health,
            HealthStatus::Degraded { .. }
        ));
    }

    #[test]
    fn describe_never_serializes_secret_material() {
        let describe_json = serde_json::to_string(&ModalProvider::new().describe()).unwrap();
        assert!(describe_json.contains("\"credentials\":\"api_key\""));
        assert!(!describe_json.contains("MODAL_TOKEN"));
    }

    #[test]
    fn pinned_region_estimates_apply_the_documented_multipliers() {
        use crate::{Usage, estimate_cost};
        let caps = ModalProvider::new().capabilities();
        let h100 = caps
            .compute
            .iter()
            .find(|o| o.id == "gpu.h100.1x")
            .expect("h100 1x offering");
        let base = h100.pricing.on_demand_rate().price.amount;
        let cost = |usage: Usage| estimate_cost(&h100.pricing, &usage).unwrap().high().amount;
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;

        // Unpinned: the base rate. Narrow `eu-west`: 1.75x. Broad `eu`: 1.15x.
        assert!(close(cost(Usage::for_hours(1.0)), base));
        assert!(close(
            cost(Usage::for_hours(1.0).in_region("eu-west")),
            base * 1.75
        ));
        assert!(close(
            cost(Usage::for_hours(1.0).in_region("eu")),
            base * 1.15
        ));
        // A region Modal does not list is charged the base rate.
        assert!(close(
            cost(Usage::for_hours(1.0).in_region("nowhere")),
            base
        ));
    }

    #[test]
    fn every_modal_region_has_a_multiplier() {
        for o in ModalProvider::new().capabilities().compute {
            for r in &o.regions {
                let m = o.pricing.region_multiplier(Some(r));
                assert!(
                    (m - 1.15).abs() < 1e-9 || (m - 1.75).abs() < 1e-9,
                    "{r}: {m}"
                );
            }
        }
    }
}
