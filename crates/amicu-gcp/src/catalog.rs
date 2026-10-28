//! The GCP GPU catalog: accelerator models, GPU machine types with their
//! consumption options, the zones and regions that offer them, and Cloud Run
//! GPU regions and prices. The backends and the HCL checks ask it "is GPU X
//! offered in zone Y", "which provisioning modes does this shape allow" and
//! "what is the smallest machine for GPU X".
//!
//! The data is Rust tables in `catalog/data.rs`, like the GPU cards in
//! `amicu-core`. Models use Google's accelerator type names
//! (`nvidia-h100-80gb`); [`resolve_model`] also takes the short names
//! (`h100`, `a100-80`, `l4`) and the `amicu-core` card ids (`l4-24gb`).
//!
//! Compute Engine prices are not in the catalog: Google ships them only
//! through the Cloud Billing Catalog API, and nothing is guessed. Cloud Run
//! prices are ([`GcpCatalog::cloud_run_price`]), with their source and date.
//!
//! Availability here is what the docs list, not entitlement: a project can
//! still lack quota or approval (see [`Zone::limited_series`]).
//!
//! # Regenerating the catalog
//!
//! The tables were transcribed on [`GENERATED`] from these pages: the GPU
//! locations page (`gpu-regions-zones`, zone by GPU series), the GPU models
//! page (`gpus`), the accelerator-optimized machines page (machine types,
//! local SSD, consumption options), the regions and zones page (second source
//! for zone series), the Cloud Run GPU page (regions) and the Cloud Run
//! pricing page (tier-1 prices). To refresh, re-read those pages, edit
//! `catalog/data.rs`, and run the drift check against a project that can list
//! Compute Engine zones:
//!
//! ```text
//! GCP_PROJECT=my-project cargo run -p amicu-gcp --example verify-catalog
//! ```
//!
//! It prints each zone and GPU (or machine type) the API lists that the
//! catalog lacks, and the reverse; see [`drift`]. Where the pages disagree
//! the tables follow these choices:
//!
//! - `us-central1-c` lists G4 per the GPU locations page, which the regions
//!   and zones page omits.
//! - `europe-west1-b` offers only A3 Mega (limited), not A3 High, per the zone
//!   table.
//! - `europe-west4-ai1a` is an AI zone, seen only on the GPU locations page
//!   and possibly hidden from the API ([`Zone::ai_zone`]).
//! - Consumption follows the per-series pages, not the "all GPU zones" claim
//!   on the locations page. A4X, A4X Max, A4 and A3 Ultra have no plain
//!   on-demand option. N1 GPU consumption is the standard set, unconfirmed.
//! - "reservation" covers on-demand and plain future reservations.
//! - P100 is not documented anywhere and is left out.

use std::collections::BTreeSet;

pub mod drift;

mod data;

use data::{
    CLOUD_RUN_GPUS, CLOUD_RUN_OFFERS, CLOUD_RUN_PRICES, CLOUD_RUN_PRICES_SOURCE, GPU_MODELS,
    MACHINE_TYPES, N1_GPUS, REGIONS, ZONES,
};

/// The date the tables were transcribed from Google's documentation.
pub const GENERATED: &str = data::GENERATED;

/// The page the Cloud Run prices come from.
pub const CLOUD_RUN_PRICES_URL: &str = CLOUD_RUN_PRICES_SOURCE;

/// The zone series that fractional G4 machine types need.
const G4_FRACTIONAL_SERIES: &str = "G4 (Fractional GPU)";

/// Short names and `amicu-core` card ids, all lowercase, to accelerator types.
const ALIASES: &[(&str, &str)] = &[
    ("gb300", "nvidia-gb300"),
    ("gb200", "nvidia-gb200"),
    ("b200", "nvidia-b200"),
    ("b200-180gb", "nvidia-b200"),
    ("h200", "nvidia-h200-141gb"),
    ("h200-sxm-141gb", "nvidia-h200-141gb"),
    ("h100-mega", "nvidia-h100-mega-80gb"),
    ("h100", "nvidia-h100-80gb"),
    ("h100-sxm-80gb", "nvidia-h100-80gb"),
    ("a100-80", "nvidia-a100-80gb"),
    ("a100-80gb", "nvidia-a100-80gb"),
    ("a100-sxm4-80gb", "nvidia-a100-80gb"),
    ("a100-40", "nvidia-tesla-a100"),
    ("a100-40gb", "nvidia-tesla-a100"),
    ("a100-sxm4-40gb", "nvidia-tesla-a100"),
    ("rtx-pro-6000", "nvidia-rtx-pro-6000"),
    ("rtx-pro-6000-96gb", "nvidia-rtx-pro-6000"),
    ("rtx-pro-6000-server-96gb", "nvidia-rtx-pro-6000"),
    ("l4", "nvidia-l4"),
    ("l4-24gb", "nvidia-l4"),
    ("t4", "nvidia-tesla-t4"),
    ("t4-16gb", "nvidia-tesla-t4"),
    ("p4", "nvidia-tesla-p4"),
    ("v100", "nvidia-tesla-v100"),
    ("v100-16gb", "nvidia-tesla-v100"),
];

/// How a VM (or a reservation for it) is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Provisioning {
    OnDemand,
    Spot,
    /// Flex-start (Dynamic Workload Scheduler): queued, time-bounded capacity.
    FlexStart,
    /// An on-demand or plain future reservation.
    Reservation,
    /// A calendar-mode future reservation.
    Calendar,
    /// A capacity-block future reservation.
    CapacityBlock,
}

impl Provisioning {
    /// The snake_case token, as in the catalog source data.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnDemand => "on_demand",
            Self::Spot => "spot",
            Self::FlexStart => "flex_start",
            Self::Reservation => "reservation",
            Self::Calendar => "calendar",
            Self::CapacityBlock => "capacity_block",
        }
    }

    /// The inverse of [`as_str`](Self::as_str).
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        [
            Self::OnDemand,
            Self::Spot,
            Self::FlexStart,
            Self::Reservation,
            Self::Calendar,
            Self::CapacityBlock,
        ]
        .into_iter()
        .find(|p| p.as_str() == token)
    }
}

/// One accelerator type, by Google's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuModel {
    /// The Compute Engine accelerator type, e.g. `nvidia-h100-80gb`.
    pub name: &'static str,
    pub display: &'static str,
    /// Memory per GPU, in GB.
    pub memory_gb: u32,
    pub architecture: &'static str,
    /// The `-vws` (virtual workstation) accelerator type, if there is one.
    pub vws: Option<&'static str>,
    /// When Google ends support (`YYYY-MM-DD`), if announced.
    pub end_of_support: Option<&'static str>,
}

/// Local SSD on a machine type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSsd {
    None,
    /// Always attached, in GB.
    Bundled(u32),
    /// Optional, up to this many GB (`None` when Google gives no size).
    Optional(Option<u32>),
}

/// A machine type that comes with its GPUs (the A and G series).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineType {
    pub name: &'static str,
    /// E.g. `A3 High`, `G2`.
    pub series: &'static str,
    /// The accelerator type, see [`GpuModel::name`].
    pub gpu: &'static str,
    /// GPUs per VM in thousandths: 1000 is one GPU, 125 an eighth (fractional G4).
    pub gpu_milli: u32,
    pub vcpus: u32,
    pub memory_gb: u32,
    /// Total GPU memory, in GB.
    pub gpu_memory_gb: u32,
    pub local_ssd: LocalSsd,
    pub consumption: &'static [Provisioning],
    pub bare_metal: bool,
}

impl MachineType {
    /// The GPU count, or `None` for a fractional GPU.
    #[must_use]
    pub fn gpu_count(&self) -> Option<u32> {
        self.gpu_milli
            .is_multiple_of(1000)
            .then_some(self.gpu_milli / 1000)
    }

    #[must_use]
    pub fn is_fractional(&self) -> bool {
        self.gpu_milli < 1000
    }

    #[must_use]
    pub fn supports(&self, provisioning: Provisioning) -> bool {
        self.consumption.contains(&provisioning)
    }
}

/// One GPU count an N1 VM can attach, and the largest shape allowed with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct N1Limit {
    pub gpus: u32,
    /// Total GPU memory, in GB.
    pub gpu_memory_gb: u32,
    pub max_vcpus: u32,
    pub max_memory_gb: u32,
}

/// A GPU that attaches to a general-purpose N1 machine type (any N1 shape
/// except the shared-core ones) at creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct N1Gpu {
    pub gpu: &'static str,
    pub limits: &'static [N1Limit],
    /// Assumed to be the standard set; the docs do not restate it per GPU.
    pub consumption: &'static [Provisioning],
}

impl N1Gpu {
    /// The allowed GPU counts, ascending.
    pub fn counts(&self) -> impl Iterator<Item = u32> + '_ {
        self.limits.iter().map(|l| l.gpus)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub name: &'static str,
    /// City and area, e.g. `Eemshaven, Netherlands, Europe`.
    pub location: &'static str,
    /// ISO 3166 country code.
    pub country: &'static str,
    /// In an EU member state (the UK and Switzerland are not).
    pub eu: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    pub name: &'static str,
    pub region: &'static str,
    /// An AI zone: needs per-project visibility and may be hidden from the API.
    pub ai_zone: bool,
    /// Accelerator types offered here.
    pub gpus: &'static [&'static str],
    /// Machine series offered, as the docs name them (`A3 High`, `N1+T4`).
    pub series: &'static [&'static str],
    /// Series that need account-team approval in this zone.
    pub limited_series: &'static [&'static str],
    pub a2_megagpu_16g: bool,
}

impl Zone {
    #[must_use]
    pub fn offers_gpu(&self, gpu: &str) -> bool {
        self.gpus.contains(&gpu)
    }

    /// Whether `machine_type` is offered in this zone.
    #[must_use]
    pub fn offers_machine_type(&self, machine_type: &MachineType) -> bool {
        let series = if machine_type.is_fractional() {
            G4_FRACTIONAL_SERIES
        } else {
            machine_type.series
        };
        self.series.contains(&series)
            && (self.a2_megagpu_16g || !machine_type.name.starts_with("a2-megagpu"))
    }

    /// Whether `machine_type` needs account-team approval in this zone.
    #[must_use]
    pub fn is_limited(&self, machine_type: &MachineType) -> bool {
        self.limited_series.contains(&machine_type.series)
    }
}

/// A Cloud Run GPU type and the smallest instance that can use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudRunGpu {
    /// Also the `--gpu-type` value.
    pub gpu: &'static str,
    pub min_vcpus: u32,
    pub min_memory_gib: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudRunOffer {
    pub gpu: &'static str,
    pub region: &'static str,
    pub invitation_only: bool,
    pub note: Option<&'static str>,
}

/// Cloud Run GPU price, tier 1, instance-based billing, GPU only (vCPU and
/// memory are billed on top). Source: [`CLOUD_RUN_PRICES_URL`], as of
/// [`GENERATED`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CloudRunPrice {
    pub gpu: &'static str,
    pub zonal_redundancy: bool,
    pub usd_per_gpu_second: f64,
    pub usd_per_gpu_hour: f64,
    pub regions: &'static [&'static str],
}

/// The accelerator type for a Google name, short name (`h100`, `a100-40`) or
/// `amicu-core` card id (`l4-24gb`), case-insensitive.
#[must_use]
pub fn resolve_model(name: &str) -> Option<&'static GpuModel> {
    let key = name.trim().to_ascii_lowercase();
    let canonical = ALIASES
        .iter()
        .find(|(alias, _)| *alias == key)
        .map_or(key.as_str(), |(_, model)| *model);
    GPU_MODELS.iter().find(|m| m.name == canonical)
}

/// The GCP GPU catalog. A zero-sized handle over the static tables, so the
/// backends can take it as a dependency.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcpCatalog {}

impl GcpCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn gpu_models(&self) -> &'static [GpuModel] {
        GPU_MODELS
    }

    /// See [`resolve_model`].
    #[must_use]
    pub fn resolve_model(&self, name: &str) -> Option<&'static GpuModel> {
        resolve_model(name)
    }

    #[must_use]
    pub fn machine_types(&self) -> &'static [MachineType] {
        MACHINE_TYPES
    }

    #[must_use]
    pub fn machine_type(&self, name: &str) -> Option<&'static MachineType> {
        MACHINE_TYPES.iter().find(|m| m.name == name)
    }

    /// The machine types with exactly `count` whole GPUs of `model`, smallest
    /// first. N1 shapes are not machine types of their own: see
    /// [`n1_gpu`](Self::n1_gpu).
    #[must_use]
    pub fn machine_types_for_gpu(&self, model: &str, count: u32) -> Vec<&'static MachineType> {
        let Some(model) = resolve_model(model) else {
            return Vec::new();
        };
        let mut found: Vec<_> = MACHINE_TYPES
            .iter()
            .filter(|m| m.gpu == model.name && m.gpu_count() == Some(count))
            .collect();
        found.sort_by_key(|m| (m.vcpus, m.memory_gb));
        found
    }

    /// The machine type with the fewest GPUs, at least `min_gpus` whole ones,
    /// then the fewest vCPUs, that can be obtained as `provisioning` (any when
    /// `None`). `None` for N1-only GPUs, which have no fixed machine type.
    #[must_use]
    pub fn smallest_machine_for_gpu(
        &self,
        model: &str,
        min_gpus: u32,
        provisioning: Option<Provisioning>,
    ) -> Option<&'static MachineType> {
        let model = resolve_model(model)?;
        MACHINE_TYPES
            .iter()
            .filter(|m| m.gpu == model.name && m.gpu_milli >= min_gpus.max(1) * 1000)
            .filter(|m| provisioning.is_none_or(|p| m.supports(p)))
            .min_by_key(|m| (m.gpu_milli, m.vcpus, m.memory_gb))
    }

    /// How `machine_type` can be obtained. `None` for an unknown machine type.
    #[must_use]
    pub fn consumption_options(&self, machine_type: &str) -> Option<&'static [Provisioning]> {
        self.machine_type(machine_type).map(|m| m.consumption)
    }

    /// Whether `machine_type` can be obtained as `provisioning`; `false` for an
    /// unknown machine type.
    #[must_use]
    pub fn supports(&self, machine_type: &str, provisioning: Provisioning) -> bool {
        self.machine_type(machine_type)
            .is_some_and(|m| m.supports(provisioning))
    }

    /// The N1 attachment rules for `model`, if it attaches to N1.
    #[must_use]
    pub fn n1_gpu(&self, model: &str) -> Option<&'static N1Gpu> {
        let model = resolve_model(model)?;
        N1_GPUS.iter().find(|n| n.gpu == model.name)
    }

    #[must_use]
    pub fn zones(&self) -> &'static [Zone] {
        ZONES
    }

    #[must_use]
    pub fn zone(&self, name: &str) -> Option<&'static Zone> {
        ZONES.iter().find(|z| z.name == name)
    }

    /// Whether `model` is offered in `zone`; `false` for an unknown zone or
    /// model.
    #[must_use]
    pub fn zone_offers_gpu(&self, zone: &str, model: &str) -> bool {
        match (self.zone(zone), resolve_model(model)) {
            (Some(z), Some(m)) => z.offers_gpu(m.name),
            _ => false,
        }
    }

    /// The zones offering `model`.
    #[must_use]
    pub fn zones_for_gpu(&self, model: &str) -> Vec<&'static Zone> {
        resolve_model(model).map_or_else(Vec::new, |m| {
            ZONES.iter().filter(|z| z.offers_gpu(m.name)).collect()
        })
    }

    /// The zones offering `machine_type`. For an N1 GPU shape use
    /// [`zones_for_gpu`](Self::zones_for_gpu).
    #[must_use]
    pub fn zones_offering(&self, machine_type: &str) -> Vec<&'static Zone> {
        self.machine_type(machine_type).map_or_else(Vec::new, |m| {
            ZONES.iter().filter(|z| z.offers_machine_type(m)).collect()
        })
    }

    #[must_use]
    pub fn regions(&self) -> &'static [Region] {
        REGIONS
    }

    #[must_use]
    pub fn region(&self, name: &str) -> Option<&'static Region> {
        REGIONS.iter().find(|r| r.name == name)
    }

    /// Whether `region` is in the EU. An unknown region is not: a residency
    /// check must not pass on a name the catalog has never seen.
    #[must_use]
    pub fn is_eu(&self, region: &str) -> bool {
        self.region(region).is_some_and(|r| r.eu)
    }

    /// The regions offering `model` on Cloud Run, sorted. Only L4 and RTX PRO
    /// 6000 are offered there.
    #[must_use]
    pub fn cloud_run_regions(&self, model: &str) -> Vec<&'static str> {
        let Some(model) = resolve_model(model) else {
            return Vec::new();
        };
        CLOUD_RUN_OFFERS
            .iter()
            .filter(|o| o.gpu == model.name)
            .map(|o| o.region)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[must_use]
    pub fn cloud_run_offers(&self) -> &'static [CloudRunOffer] {
        CLOUD_RUN_OFFERS
    }

    #[must_use]
    pub fn cloud_run_gpu(&self, model: &str) -> Option<&'static CloudRunGpu> {
        let model = resolve_model(model)?;
        CLOUD_RUN_GPUS.iter().find(|g| g.gpu == model.name)
    }

    /// The tier-1 price for `model` in `region`, or `None` where Google's
    /// prices were not obtained (tier-2 regions).
    #[must_use]
    pub fn cloud_run_price(
        &self,
        model: &str,
        region: &str,
        zonal_redundancy: bool,
    ) -> Option<&'static CloudRunPrice> {
        let model = resolve_model(model)?;
        CLOUD_RUN_PRICES.iter().find(|p| {
            p.gpu == model.name
                && p.zonal_redundancy == zonal_redundancy
                && p.regions.contains(&region)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn cat() -> GcpCatalog {
        GcpCatalog::new()
    }

    #[test]
    fn every_machine_type_gpu_exists_and_names_are_unique() {
        let mut seen = HashSet::new();
        for m in MACHINE_TYPES {
            assert!(
                GPU_MODELS.iter().any(|g| g.name == m.gpu),
                "{} uses unknown GPU {}",
                m.name,
                m.gpu
            );
            assert!(seen.insert(m.name), "duplicate machine type {}", m.name);
            assert!(m.vcpus > 0 && m.memory_gb > 0 && !m.consumption.is_empty());
        }
        for n in N1_GPUS {
            assert!(GPU_MODELS.iter().any(|g| g.name == n.gpu));
        }
    }

    #[test]
    fn gpu_memory_matches_the_model() {
        for m in MACHINE_TYPES {
            let model = resolve_model(m.gpu).unwrap();
            let expected = u64::from(model.memory_gb) * u64::from(m.gpu_milli) / 1000;
            // Google rounds per-GPU memory in a few rows; allow a small gap.
            let gap = u64::from(m.gpu_memory_gb).abs_diff(expected);
            assert!(
                gap * 20 <= expected,
                "{}: {} GB vs {expected}",
                m.name,
                m.gpu_memory_gb
            );
        }
    }

    #[test]
    fn every_zone_region_exists_and_zone_names_agree() {
        let mut seen = HashSet::new();
        for z in ZONES {
            assert!(seen.insert(z.name), "duplicate zone {}", z.name);
            assert!(cat().region(z.region).is_some(), "{} has no region", z.name);
            assert_eq!(crate::validate::zone_region(z.name).unwrap(), z.region);
            for g in z.gpus {
                assert!(resolve_model(g).is_some(), "{} offers unknown {g}", z.name);
            }
        }
        for r in REGIONS {
            assert!(
                ZONES.iter().any(|z| z.region == r.name),
                "{} has no zone",
                r.name
            );
        }
    }

    #[test]
    fn every_zone_series_has_a_known_shape() {
        // A series named in a zone must exist as machine types or as N1.
        for z in ZONES {
            for s in z.series.iter().chain(z.limited_series) {
                let known = MACHINE_TYPES.iter().any(|m| m.series == *s)
                    || *s == G4_FRACTIONAL_SERIES
                    || s.starts_with("N1+");
                assert!(known, "{}: unknown series {s}", z.name);
            }
        }
    }

    #[test]
    fn eu_flags() {
        let c = cat();
        for eu in [
            "europe-west1",
            "europe-west3",
            "europe-west4",
            "europe-north1",
            "europe-central2",
            "europe-west8",
            "europe-west9",
            "europe-west10",
        ] {
            assert!(c.is_eu(eu), "{eu}");
        }
        for not in ["europe-west2", "europe-west6", "us-central1", "me-west1"] {
            assert!(!c.is_eu(not), "{not}");
        }
        assert!(!c.is_eu("europe-nowhere9"));
        assert_eq!(REGIONS.len(), 34);
        assert_eq!(ZONES.len(), 92);
    }

    #[test]
    fn a3_high_small_shapes_are_spot_and_flex_start_only() {
        let c = cat();
        for name in ["a3-highgpu-1g", "a3-highgpu-2g", "a3-highgpu-4g"] {
            assert_eq!(
                c.consumption_options(name).unwrap(),
                [Provisioning::Spot, Provisioning::FlexStart],
                "{name}"
            );
            assert!(!c.supports(name, Provisioning::OnDemand));
            assert!(!c.supports(name, Provisioning::Reservation));
        }
        assert!(c.supports("a3-highgpu-8g", Provisioning::OnDemand));
        assert!(c.supports("a3-highgpu-8g", Provisioning::CapacityBlock));
        assert!(!c.supports("a4-highgpu-8g", Provisioning::OnDemand));
        assert_eq!(
            c.consumption_options("a4x-highgpu-4g").unwrap(),
            [Provisioning::CapacityBlock]
        );
        assert!(!c.supports("no-such-type", Provisioning::Spot));
    }

    #[test]
    fn short_names_resolve() {
        let c = cat();
        for (short, google) in [
            ("h100", "nvidia-h100-80gb"),
            ("H200", "nvidia-h200-141gb"),
            ("b200", "nvidia-b200"),
            ("a100-40", "nvidia-tesla-a100"),
            ("a100-80", "nvidia-a100-80gb"),
            ("l4", "nvidia-l4"),
            ("rtx-pro-6000", "nvidia-rtx-pro-6000"),
            ("t4", "nvidia-tesla-t4"),
            ("v100", "nvidia-tesla-v100"),
            ("p4", "nvidia-tesla-p4"),
            ("nvidia-l4", "nvidia-l4"),
            (" l4-24gb ", "nvidia-l4"),
        ] {
            assert_eq!(c.resolve_model(short).unwrap().name, google, "{short}");
        }
        assert!(c.resolve_model("a100").is_none(), "ambiguous");
        assert!(c.resolve_model("p100").is_none());
        assert!(c.resolve_model("").is_none());
    }

    #[test]
    fn aliases_agree_with_the_core_gpu_cards() {
        for (alias, google) in ALIASES {
            let model = GPU_MODELS.iter().find(|m| m.name == *google).unwrap();
            if let Some(card) = amicu_core::resource::gpu_card(alias) {
                assert_eq!(card.vram_gb, model.memory_gb, "{alias}");
            }
        }
        // The short names GCP shares with the core catalog are core families.
        for short in ["h100", "h200", "b200", "l4", "t4", "v100", "rtx-pro-6000"] {
            assert!(
                amicu_core::resource::gpu_cards()
                    .iter()
                    .any(|c| c.family == short),
                "{short}"
            );
        }
    }

    #[test]
    fn cloud_run_is_only_l4_and_rtx_pro_6000() {
        for o in CLOUD_RUN_OFFERS {
            assert!(["nvidia-l4", "nvidia-rtx-pro-6000"].contains(&o.gpu));
            assert!(cat().region(o.region).is_some(), "{}", o.region);
        }
        for p in CLOUD_RUN_PRICES {
            assert!(["nvidia-l4", "nvidia-rtx-pro-6000"].contains(&p.gpu));
            assert!(p.usd_per_gpu_hour > 0.0);
            for r in p.regions {
                assert!(
                    CLOUD_RUN_OFFERS
                        .iter()
                        .any(|o| o.gpu == p.gpu && o.region == *r)
                );
            }
        }
        let c = cat();
        assert_eq!(
            c.cloud_run_regions("l4"),
            [
                "asia-south1",
                "asia-southeast1",
                "europe-west1",
                "europe-west4",
                "us-central1",
                "us-east4"
            ]
        );
        assert_eq!(
            c.cloud_run_regions("rtx-pro-6000"),
            [
                "asia-south2",
                "asia-southeast1",
                "europe-west4",
                "us-central1"
            ]
        );
        assert_eq!(c.cloud_run_regions("h100"), Vec::<&str>::new());
        assert!(
            CLOUD_RUN_OFFERS
                .iter()
                .any(|o| o.region == "asia-south1" && o.invitation_only)
        );
        assert_eq!(c.cloud_run_gpu("l4").unwrap().min_vcpus, 4);
    }

    #[test]
    fn cloud_run_prices() {
        let c = cat();
        let p = c.cloud_run_price("l4", "europe-west4", false).unwrap();
        assert!((p.usd_per_gpu_hour - 0.67212).abs() < 1e-9);
        let p = c
            .cloud_run_price("rtx-pro-6000", "us-central1", true)
            .unwrap();
        assert!((p.usd_per_gpu_second - 0.000_569_13).abs() < 1e-12);
        // Tier-2 regions have no recorded price.
        assert!(c.cloud_run_price("l4", "asia-southeast1", false).is_none());
        assert!(CLOUD_RUN_PRICES_URL.starts_with("https://cloud.google.com/"));
    }

    #[test]
    fn gpu_is_offered_in_zone() {
        let c = cat();
        assert!(c.zone_offers_gpu("us-central1-a", "h100"));
        assert!(c.zone_offers_gpu("us-central1-a", "nvidia-h100-80gb"));
        assert!(!c.zone_offers_gpu("us-central1-a", "h200"));
        assert!(!c.zone_offers_gpu("nowhere-1-a", "h100"));
        assert!(!c.zone_offers_gpu("us-central1-a", "nonsense"));
        // Source discrepancies resolved in the data.
        assert!(c.zone_offers_gpu("us-central1-c", "rtx-pro-6000"));
        assert!(c.zone_offers_gpu("europe-west1-b", "nvidia-h100-mega-80gb"));
        assert!(c.zone_offers_gpu("europe-west1-b", "h100"));
        assert!(c.zone("europe-west4-ai1a").unwrap().ai_zone);
    }

    #[test]
    fn zones_offering_a_machine_type() {
        let c = cat();
        let names = |t: &str| -> Vec<&str> { c.zones_offering(t).iter().map(|z| z.name).collect() };
        // A2 Mega 16g is in a handful of zones only.
        assert_eq!(
            names("a2-megagpu-16g"),
            [
                "asia-southeast1-a",
                "asia-southeast1-c",
                "europe-west4-a",
                "europe-west4-b",
                "us-central1-a",
                "us-central1-b",
                "us-central1-c",
                "us-central1-f"
            ]
        );
        // Fractional G4 only in us-central1-b.
        assert_eq!(names("g4-standard-6"), ["us-central1-b"]);
        assert!(names("g4-standard-48").contains(&"europe-west4-ai1a"));
        assert!(names("a3-highgpu-1g").contains(&"europe-west9-c"));
        assert_eq!(c.zones_offering("nope"), Vec::<&Zone>::new());
        // Limited availability.
        let mega = c.machine_type("a3-megagpu-8g").unwrap();
        assert!(c.zone("us-central1-b").unwrap().is_limited(mega));
        assert!(!c.zone("us-central1-a").unwrap().is_limited(mega));
    }

    #[test]
    fn every_machine_type_is_offered_somewhere() {
        for m in MACHINE_TYPES {
            assert!(
                !cat().zones_offering(m.name).is_empty(),
                "{} is in no zone",
                m.name
            );
        }
    }

    #[test]
    fn machine_types_for_gpu_and_smallest() {
        let c = cat();
        let names = |model: &str, n: u32| -> Vec<&str> {
            c.machine_types_for_gpu(model, n)
                .iter()
                .map(|m| m.name)
                .collect()
        };
        assert_eq!(names("h100", 8), ["a3-highgpu-8g", "a3-edgegpu-8g"]);
        assert_eq!(names("l4", 1)[0], "g2-standard-4");
        assert_eq!(names("l4", 2), ["g2-standard-24"]);
        assert_eq!(names("rtx-pro-6000", 1), ["g4-standard-48"]);
        assert_eq!(names("l4", 3), Vec::<&str>::new());
        assert_eq!(
            names("t4", 1),
            Vec::<&str>::new(),
            "N1 has no fixed machine type"
        );

        let small = |model: &str, n: u32, p: Option<Provisioning>| {
            c.smallest_machine_for_gpu(model, n, p).map(|m| m.name)
        };
        assert_eq!(small("l4", 1, None), Some("g2-standard-4"));
        assert_eq!(small("h100", 1, None), Some("a3-highgpu-1g"));
        // 1 GPU of H100 on demand only exists as the 8-GPU shape.
        assert_eq!(
            small("h100", 1, Some(Provisioning::OnDemand)),
            Some("a3-highgpu-8g")
        );
        assert_eq!(
            small("h100", 1, Some(Provisioning::Spot)),
            Some("a3-highgpu-1g")
        );
        assert_eq!(small("a100-40", 2, None), Some("a2-highgpu-2g"));
        assert_eq!(small("rtx-pro-6000", 1, None), Some("g4-standard-48"));
        assert_eq!(small("b200", 1, None), Some("a4-highgpu-8g"));
        assert_eq!(small("l4", 9, None), None);
        assert_eq!(small("t4", 1, None), None);
    }

    #[test]
    fn n1_attachable_counts() {
        let c = cat();
        let counts = |m: &str| c.n1_gpu(m).unwrap().counts().collect::<Vec<_>>();
        assert_eq!(counts("t4"), [1, 2, 4]);
        assert_eq!(counts("p4"), [1, 2, 4]);
        assert_eq!(counts("v100"), [1, 2, 4, 8]);
        assert!(c.n1_gpu("l4").is_none());
        let v = &c.n1_gpu("v100").unwrap().limits[3];
        assert_eq!(
            (v.max_vcpus, v.max_memory_gb, v.gpu_memory_gb),
            (96, 624, 128)
        );
        assert_eq!(
            c.resolve_model("t4").unwrap().end_of_support,
            Some("2027-08-01")
        );
    }

    #[test]
    fn provisioning_tokens_round_trip() {
        for p in [
            Provisioning::OnDemand,
            Provisioning::Spot,
            Provisioning::FlexStart,
            Provisioning::Reservation,
            Provisioning::Calendar,
            Provisioning::CapacityBlock,
        ] {
            assert_eq!(Provisioning::parse(p.as_str()), Some(p));
        }
        assert_eq!(Provisioning::parse("preemptible"), None);
    }

    #[test]
    fn local_ssd_and_fractional_shapes() {
        let c = cat();
        let m = |n: &str| c.machine_type(n).unwrap();
        assert_eq!(m("a3-highgpu-8g").local_ssd, LocalSsd::Bundled(6000));
        assert_eq!(m("a2-highgpu-1g").local_ssd, LocalSsd::Optional(None));
        assert_eq!(m("g4-standard-6").local_ssd, LocalSsd::None);
        assert_eq!(m("g2-standard-4").local_ssd, LocalSsd::Optional(Some(375)));
        assert_eq!(m("g4-standard-6").gpu_count(), None);
        assert_eq!(m("g4-standard-12").gpu_milli, 250);
        assert_eq!(m("g4-standard-384").gpu_count(), Some(8));
        assert!(m("a4x-maxgpu-4g-metal").bare_metal);
    }
}
