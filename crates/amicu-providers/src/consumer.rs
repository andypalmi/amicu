//! Consumer / on-prem hardware catalog: named [`HardwareProfile`] constructors
//! for GPUs and Apple Silicon machines a user already owns.
//!
//! This is deliberately **not** a [`Provider`](crate::Provider) implementation.
//! A `Provider` provisions and tears down remote infrastructure through a
//! backend API; owned hardware is provisioned by nothing - amicu just launches
//! a local process (llama.cpp/vLLM) against a box that's already there. So
//! unlike `runpod`/`hetzner`, there is no adapter here, only catalog data: a
//! set of [`HardwareProfile`] values a user's HCL can reference (directly, or
//! copied as a starting point - see the `amicu-dsl` skill's
//! `hardware-examples.md`) and that the wizard/recommend engine can offer as
//! on-prem candidates.
//!
//! `provider` is `"on_prem"` on every entry - the string [`HardwareProfile`]'s
//! doc comment already anticipates - and `price` is always `None`, never
//! `Some(0)`: there is no rental rate for hardware the user already owns, and
//! `None` is the established convention for "no price" elsewhere in the
//! codebase (`amicu_recommend::preset`'s local-only builders, and
//! `amicu_engine::checks`). `amicu_recommend::rank` never divides by price, so
//! `None` here is safe to carry through ranking untouched.
//!
//! AMD's RX 7900 XTX is catalogued under [`AcceleratorKind::Gpu`]: the enum has
//! no AMD/ROCm-specific variant, and adding one would require a matching arm in
//! `amicu_registry::fit::device_pool`'s exhaustive match, which is out of scope
//! here. `Gpu` already covers non-NVIDIA discrete accelerators elsewhere in the
//! codebase (the `kind` is a memory-pool classifier, not a vendor tag), so this
//! is a faithful, low-risk fit.
//!
//! Every accelerator here also carries [`Accelerator::memory_bandwidth_gbps`]
//! from the vendor's published memory-bandwidth spec, for forward-looking
//! planning; it is not yet read by the fit check.

use amicu_core::resource::{Accelerator, AcceleratorKind, HardwareProfile, resolve_gpu};

/// A typical enthusiast/workstation desktop build's system RAM. Independent of
/// GPU VRAM and not fit-check-relevant for a discrete accelerator (the fit
/// check pools on `accelerator.memory`, not this field, for `Gpu`/`Tpu` kinds -
/// see `amicu_registry::fit::device_pool`), but [`HardwareProfile::memory`] is
/// required, so a plausible independent figure is recorded rather than
/// duplicating the VRAM size.
const DESKTOP_HOST_MEMORY: &str = "64GiB";

/// The profile id keeps the slug HCL already references (`rtx_4090`).
fn discrete_gpu(id: &str, vram: &str, bandwidth_gbps: u32) -> HardwareProfile {
    let memory = vram.parse().expect("valid ByteSize literal");
    let card = resolve_gpu(id, Some(memory)).expect("consumer GPU is in the GPU catalog");
    HardwareProfile {
        id: id.into(),
        provider: "on_prem".to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: card.id.to_string(),
            count: 1,
            memory,
            memory_bandwidth_gbps: Some(bandwidth_gbps),
        }),
        cpu: None,
        memory: DESKTOP_HOST_MEMORY.parse().expect("valid ByteSize literal"),
        interconnect: None,
        // Owned hardware has no rental rate.
        price: None,
    }
}

fn apple_silicon(id: &str, unified_memory: &str, bandwidth_gbps: u32) -> HardwareProfile {
    HardwareProfile {
        id: id.into(),
        provider: "on_prem".to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::AppleSilicon,
            model: id.to_string(),
            count: 1,
            memory: unified_memory.parse().expect("valid ByteSize literal"),
            memory_bandwidth_gbps: Some(bandwidth_gbps),
        }),
        cpu: None,
        // Unified memory: host RAM and accelerator memory are the same pool.
        memory: unified_memory.parse().expect("valid ByteSize literal"),
        interconnect: None,
        price: None,
    }
}

/// NVIDIA GeForce RTX 5090: 32 GB GDDR7, 1792 GB/s memory bandwidth (NVIDIA
/// spec sheet).
#[must_use]
pub fn rtx_5090() -> HardwareProfile {
    discrete_gpu("rtx_5090", "32GiB", 1792)
}

/// NVIDIA GeForce RTX 4090: 24 GB GDDR6X, 1008 GB/s memory bandwidth (NVIDIA
/// spec sheet).
#[must_use]
pub fn rtx_4090() -> HardwareProfile {
    discrete_gpu("rtx_4090", "24GiB", 1008)
}

/// NVIDIA GeForce RTX 3090: 24 GB GDDR6X, 936 GB/s memory bandwidth (NVIDIA
/// spec sheet).
#[must_use]
pub fn rtx_3090() -> HardwareProfile {
    discrete_gpu("rtx_3090", "24GiB", 936)
}

/// NVIDIA GeForce RTX 4080: 16 GB GDDR6X, 717 GB/s memory bandwidth (NVIDIA
/// spec sheet).
#[must_use]
pub fn rtx_4080() -> HardwareProfile {
    discrete_gpu("rtx_4080", "16GiB", 717)
}

/// NVIDIA GeForce RTX 4070 Ti Super: 16 GB GDDR6X, 672 GB/s memory bandwidth
/// (NVIDIA spec sheet).
#[must_use]
pub fn rtx_4070_ti_super() -> HardwareProfile {
    discrete_gpu("rtx_4070_ti_super", "16GiB", 672)
}

/// NVIDIA GeForce RTX 3060 Ti: 8 GB GDDR6 (Ampere, CUDA compute capability
/// 8.6 / sm_86), 448 GB/s memory bandwidth (NVIDIA spec sheet).
#[must_use]
pub fn rtx_3060_ti() -> HardwareProfile {
    discrete_gpu("rtx_3060_ti", "8GiB", 448)
}

/// AMD Radeon RX 7900 XTX: 24 GB GDDR6, 960 GB/s memory bandwidth (AMD spec
/// sheet). Catalogued as [`AcceleratorKind::Gpu`] - see the module doc for why.
#[must_use]
pub fn rx_7900_xtx() -> HardwareProfile {
    discrete_gpu("rx_7900_xtx", "24GiB", 960)
}

/// One unified-memory configuration an Apple Silicon chip ships in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryConfig {
    /// Unified memory, in GiB.
    pub memory_gib: u32,
    /// Unified memory bandwidth of this configuration, in GB/s (Apple spec
    /// sheet). It can differ between configurations of one chip (binned
    /// M4 Max parts).
    pub bandwidth_gbps: u32,
}

/// An Apple Silicon chip and the memory configurations it is sold in.
///
/// The catalog id is the **chip only** (`m4_pro`, `m5_pro`, ...): the memory
/// size is not part of the id. A user's HCL sets it with the profile's own
/// `memory` field and `accelerator.model` is free-form, so baking the size
/// into the id would only duplicate it. The sizes a chip is available in are
/// data here, for the wizard/recommend engine to enumerate candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppleSiliconChip {
    /// The chip id, also the profile id and `accelerator.model`.
    pub id: &'static str,
    /// Available memory configurations, smallest first.
    pub configs: &'static [MemoryConfig],
}

const fn cfg(memory_gib: u32, bandwidth_gbps: u32) -> MemoryConfig {
    MemoryConfig {
        memory_gib,
        bandwidth_gbps,
    }
}

// Sizes and bandwidths are from Apple's tech-specs pages. Where a chip is
// binned into parts with different bandwidth for the same memory size (M3 Max
// 14-core CPU / 30-core GPU 300 GB/s vs 400 GB/s, M4 Max 32-core GPU 410 vs
// 546 GB/s, M5 Max 32-core GPU 460 vs 614 GB/s), the lower figure is used when
// the size is sold with the slower bin, so estimates stay conservative. M4 Max 36 GB is the
// only slower-bin size; 48 GB and up ship with the 16-core CPU / 40-core GPU
// bin at 546 GB/s. There is no M4 Ultra or M5 Ultra.
const APPLE_SILICON_CHIPS: &[AppleSiliconChip] = &[
    AppleSiliconChip {
        id: "m1",
        configs: &[cfg(8, 68), cfg(16, 68)],
    },
    AppleSiliconChip {
        id: "m1_pro",
        configs: &[cfg(16, 200), cfg(32, 200)],
    },
    AppleSiliconChip {
        id: "m1_max",
        configs: &[cfg(32, 400), cfg(64, 400)],
    },
    AppleSiliconChip {
        id: "m1_ultra",
        configs: &[cfg(64, 800), cfg(128, 800)],
    },
    AppleSiliconChip {
        id: "m2",
        configs: &[cfg(8, 100), cfg(16, 100), cfg(24, 100)],
    },
    AppleSiliconChip {
        id: "m2_pro",
        configs: &[cfg(16, 200), cfg(32, 200)],
    },
    AppleSiliconChip {
        id: "m2_max",
        configs: &[cfg(32, 400), cfg(64, 400), cfg(96, 400)],
    },
    AppleSiliconChip {
        id: "m2_ultra",
        configs: &[cfg(64, 800), cfg(128, 800), cfg(192, 800)],
    },
    AppleSiliconChip {
        id: "m3",
        configs: &[cfg(8, 100), cfg(16, 100), cfg(24, 100)],
    },
    AppleSiliconChip {
        id: "m3_pro",
        configs: &[cfg(18, 150), cfg(36, 150)],
    },
    AppleSiliconChip {
        id: "m3_max",
        configs: &[
            cfg(36, 300),
            cfg(48, 400),
            cfg(64, 400),
            cfg(96, 300),
            cfg(128, 400),
        ],
    },
    AppleSiliconChip {
        id: "m3_ultra",
        configs: &[cfg(96, 819), cfg(256, 819), cfg(512, 819)],
    },
    AppleSiliconChip {
        id: "m4",
        configs: &[cfg(16, 120), cfg(24, 120), cfg(32, 120)],
    },
    AppleSiliconChip {
        id: "m4_pro",
        configs: &[cfg(24, 273), cfg(48, 273), cfg(64, 273)],
    },
    AppleSiliconChip {
        id: "m4_max",
        configs: &[cfg(36, 410), cfg(48, 546), cfg(64, 546), cfg(128, 546)],
    },
    AppleSiliconChip {
        id: "m5",
        configs: &[cfg(16, 153), cfg(24, 153), cfg(32, 153)],
    },
    AppleSiliconChip {
        id: "m5_pro",
        configs: &[cfg(24, 307), cfg(48, 307), cfg(64, 307)],
    },
    AppleSiliconChip {
        id: "m5_max",
        configs: &[cfg(36, 460), cfg(48, 614), cfg(64, 614), cfg(128, 614)],
    },
];

/// Every Apple Silicon chip in the catalog, with its memory configurations.
#[must_use]
pub fn apple_silicon_chips() -> &'static [AppleSiliconChip] {
    APPLE_SILICON_CHIPS
}

/// The Apple Silicon chip with this id (`m4_pro`, ...), if catalogued.
#[must_use]
pub fn apple_silicon_chip(id: &str) -> Option<&'static AppleSiliconChip> {
    APPLE_SILICON_CHIPS.iter().find(|c| c.id == id)
}

impl AppleSiliconChip {
    /// The profile for the `memory_gib` configuration, or `None` when the chip
    /// is not sold with that much memory. The profile id and
    /// `accelerator.model` are the chip id; memory lives in the profile's
    /// `memory` (and `accelerator.memory`) fields.
    #[must_use]
    pub fn profile(&self, memory_gib: u32) -> Option<HardwareProfile> {
        let config = self.configs.iter().find(|c| c.memory_gib == memory_gib)?;
        Some(self.profile_for(*config))
    }

    /// The profile for the chip's smallest memory configuration.
    #[must_use]
    pub fn base_profile(&self) -> HardwareProfile {
        self.profile_for(self.configs[0])
    }

    fn profile_for(&self, config: MemoryConfig) -> HardwareProfile {
        apple_silicon(
            self.id,
            &format!("{}GiB", config.memory_gib),
            config.bandwidth_gbps,
        )
    }
}

/// Apple M4 Pro (24, 48 or 64 GB), ~273 GB/s unified memory bandwidth (Apple spec
/// sheet). The profile is the 24 GB configuration; see
/// [`AppleSiliconChip::profile`] for the others.
#[must_use]
pub fn m4_pro() -> HardwareProfile {
    chip_base("m4_pro")
}

/// Apple M5 Pro (24, 48 or 64 GB), 307 GB/s unified memory bandwidth (Apple
/// spec sheet). The profile is the 24 GB configuration.
#[must_use]
pub fn m5_pro() -> HardwareProfile {
    chip_base("m5_pro")
}

/// Apple M4 Max (36, 48, 64 or 128 GB), 410 / 546 GB/s unified memory bandwidth
/// (Apple spec sheet). The profile is the 36 GB configuration.
#[must_use]
pub fn m4_max() -> HardwareProfile {
    chip_base("m4_max")
}

/// Apple M3 Ultra (96, 256 or 512 GB), 819 GB/s unified memory bandwidth
/// (Apple spec sheet). The profile is the 96 GB configuration.
#[must_use]
pub fn m3_ultra() -> HardwareProfile {
    chip_base("m3_ultra")
}

/// Apple M2 Ultra (64, 128 or 192 GB), 800 GB/s unified memory bandwidth (Apple
/// spec sheet). The profile is the 64 GB configuration.
#[must_use]
pub fn m2_ultra() -> HardwareProfile {
    chip_base("m2_ultra")
}

fn chip_base(id: &str) -> HardwareProfile {
    apple_silicon_chip(id)
        .expect("chip is in the catalog")
        .base_profile()
}

/// Every consumer/on-prem hardware profile this module catalogs: six NVIDIA
/// desktop GPUs, one AMD desktop GPU (memory is fixed per model), and one entry
/// per Apple Silicon chip at its smallest memory configuration. Apple Silicon
/// memory is chosen with the HCL `memory` field; the sizes a chip comes in are
/// [`apple_silicon_chips`].
#[must_use]
pub fn catalog() -> Vec<HardwareProfile> {
    let mut entries = vec![
        rtx_5090(),
        rtx_4090(),
        rtx_3090(),
        rtx_4080(),
        rtx_4070_ti_super(),
        rtx_3060_ti(),
        rx_7900_xtx(),
    ];
    entries.extend(
        APPLE_SILICON_CHIPS
            .iter()
            .map(AppleSiliconChip::base_profile),
    );
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_twenty_five_entries() {
        // Seven discrete GPUs and eighteen Apple Silicon chips.
        assert_eq!(catalog().len(), 25);
    }

    #[test]
    fn apple_silicon_ids_are_chip_only() {
        for chip in apple_silicon_chips() {
            assert!(
                !chip.id.ends_with("gb"),
                "{} bakes in a memory size",
                chip.id
            );
            assert_ne!(chip.configs, []);
        }
        let ids: Vec<_> = apple_silicon_chips().iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            [
                "m1", "m1_pro", "m1_max", "m1_ultra", "m2", "m2_pro", "m2_max", "m2_ultra", "m3",
                "m3_pro", "m3_max", "m3_ultra", "m4", "m4_pro", "m4_max", "m5", "m5_pro", "m5_max",
            ]
        );
    }

    #[test]
    fn chip_profiles_carry_the_memory_as_data() {
        let m5 = apple_silicon_chip("m5_pro").unwrap();
        let sizes: Vec<u32> = m5.configs.iter().map(|c| c.memory_gib).collect();
        assert_eq!(sizes, [24, 48, 64]);
        let hw = m5.profile(64).expect("64 GiB config");
        assert_eq!(hw.id.as_str(), "m5_pro");
        assert_eq!(hw.memory, "64GiB".parse().unwrap());
        let accel = hw.accelerator.unwrap();
        assert_eq!(accel.model, "m5_pro");
        assert_eq!(accel.memory, "64GiB".parse().unwrap());
        assert_eq!(accel.memory_bandwidth_gbps, Some(307));
        assert!(m5.profile(32).is_none());
        // Bandwidth is per configuration (binned M4 Max).
        let max = apple_silicon_chip("m4_max").unwrap();
        assert_eq!(
            max.profile(128)
                .unwrap()
                .accelerator
                .unwrap()
                .memory_bandwidth_gbps,
            Some(546)
        );
        assert_eq!(m4_pro().memory, "24GiB".parse().unwrap());
    }

    #[test]
    fn catalog_ids_are_unique() {
        let entries = catalog();
        let mut ids: Vec<&str> = entries.iter().map(|hw| hw.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), entries.len(), "catalog ids must be unique");
    }

    #[test]
    fn catalog_ids_are_lowercase_snake_case() {
        for hw in catalog() {
            let id = hw.id.as_str();
            assert!(
                id.chars().next().is_some_and(|c| c.is_ascii_lowercase()),
                "id {id:?} must start with a lowercase letter"
            );
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "id {id:?} must be lowercase snake_case"
            );
        }
    }

    #[test]
    fn every_entry_is_on_prem_with_no_price() {
        for hw in catalog() {
            assert_eq!(hw.provider, "on_prem");
            assert!(
                hw.price.is_none(),
                "on-prem hardware must not carry a price"
            );
        }
    }

    #[test]
    fn every_entry_carries_a_bandwidth_figure() {
        for hw in catalog() {
            let accel = hw
                .accelerator
                .as_ref()
                .expect("every entry has an accelerator");
            assert!(
                accel.memory_bandwidth_gbps.is_some(),
                "{} is missing a bandwidth figure",
                hw.id.as_str()
            );
        }
    }
}
