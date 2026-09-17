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

use amicu_core::resource::{Accelerator, AcceleratorKind, HardwareProfile};

/// A typical enthusiast/workstation desktop build's system RAM. Independent of
/// GPU VRAM and not fit-check-relevant for a discrete accelerator (the fit
/// check pools on `accelerator.memory`, not this field, for `Gpu`/`Tpu` kinds -
/// see `amicu_registry::fit::device_pool`), but [`HardwareProfile::memory`] is
/// required, so a plausible independent figure is recorded rather than
/// duplicating the VRAM size.
const DESKTOP_HOST_MEMORY: &str = "64GiB";

fn discrete_gpu(id: &str, vram: &str, bandwidth_gbps: u32) -> HardwareProfile {
    HardwareProfile {
        id: id.into(),
        provider: "on_prem".to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: id.to_string(),
            count: 1,
            memory: vram.parse().expect("valid ByteSize literal"),
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

/// AMD Radeon RX 7900 XTX: 24 GB GDDR6, 960 GB/s memory bandwidth (AMD spec
/// sheet). Catalogued as [`AcceleratorKind::Gpu`] - see the module doc for why.
#[must_use]
pub fn rx_7900_xtx() -> HardwareProfile {
    discrete_gpu("rx_7900_xtx", "24GiB", 960)
}

/// Apple M4 Pro, 24 GB configuration: ~273 GB/s unified memory bandwidth
/// (Apple spec sheet).
#[must_use]
pub fn m4_pro_24gb() -> HardwareProfile {
    apple_silicon("m4_pro_24gb", "24GiB", 273)
}

/// Apple M4 Pro, 48 GB configuration: ~273 GB/s unified memory bandwidth
/// (Apple spec sheet).
#[must_use]
pub fn m4_pro_48gb() -> HardwareProfile {
    apple_silicon("m4_pro_48gb", "48GiB", 273)
}

/// Apple M4 Max, 36 GB configuration: ~410 GB/s unified memory bandwidth
/// (Apple spec sheet; the lower of the two M4 Max memory-bus bins).
#[must_use]
pub fn m4_max_36gb() -> HardwareProfile {
    apple_silicon("m4_max_36gb", "36GiB", 410)
}

/// Apple M4 Max, 128 GB configuration: ~546 GB/s unified memory bandwidth
/// (Apple spec sheet; the full memory-bus bin).
#[must_use]
pub fn m4_max_128gb() -> HardwareProfile {
    apple_silicon("m4_max_128gb", "128GiB", 546)
}

/// Apple M3 Ultra, 96 GB configuration: ~819 GB/s unified memory bandwidth
/// (Apple spec sheet).
#[must_use]
pub fn m3_ultra_96gb() -> HardwareProfile {
    apple_silicon("m3_ultra_96gb", "96GiB", 819)
}

/// Apple M3 Ultra, 256 GB configuration: ~819 GB/s unified memory bandwidth
/// (Apple spec sheet).
#[must_use]
pub fn m3_ultra_256gb() -> HardwareProfile {
    apple_silicon("m3_ultra_256gb", "256GiB", 819)
}

/// Apple M3 Ultra, 512 GB configuration: ~819 GB/s unified memory bandwidth
/// (Apple spec sheet).
#[must_use]
pub fn m3_ultra_512gb() -> HardwareProfile {
    apple_silicon("m3_ultra_512gb", "512GiB", 819)
}

/// Apple M2 Ultra, 64 GB configuration: ~800 GB/s unified memory bandwidth
/// (Apple spec sheet).
#[must_use]
pub fn m2_ultra_64gb() -> HardwareProfile {
    apple_silicon("m2_ultra_64gb", "64GiB", 800)
}

/// Apple M2 Ultra, 192 GB configuration: ~800 GB/s unified memory bandwidth
/// (Apple spec sheet).
#[must_use]
pub fn m2_ultra_192gb() -> HardwareProfile {
    apple_silicon("m2_ultra_192gb", "192GiB", 800)
}

/// Every consumer/on-prem hardware profile this module catalogs: five NVIDIA
/// desktop GPUs, one AMD desktop GPU, and nine Apple Silicon unified-memory
/// configurations.
#[must_use]
pub fn catalog() -> Vec<HardwareProfile> {
    vec![
        rtx_5090(),
        rtx_4090(),
        rtx_3090(),
        rtx_4080(),
        rtx_4070_ti_super(),
        rx_7900_xtx(),
        m4_pro_24gb(),
        m4_pro_48gb(),
        m4_max_36gb(),
        m4_max_128gb(),
        m3_ultra_96gb(),
        m3_ultra_256gb(),
        m3_ultra_512gb(),
        m2_ultra_64gb(),
        m2_ultra_192gb(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_fifteen_entries() {
        assert_eq!(catalog().len(), 15);
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
