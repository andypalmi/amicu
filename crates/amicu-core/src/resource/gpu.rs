//! Provider-agnostic GPU catalog: one entry per exact card variant.
//!
//! Canonical ids are `<family>[-<variant>]-<vram>gb` (`h100-sxm-80gb`,
//! `l4-24gb`); the variant appears only when a family ships in several form
//! factors or editions. See [`resolve_gpu`] for how a model name maps to a card.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::units::ByteSize;

const GB: u64 = 1_000_000_000;
const GIB: u64 = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuVendor {
    Nvidia,
    Amd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuCard {
    pub id: &'static str,
    pub family: &'static str,
    pub vendor: GpuVendor,
    /// Nominal VRAM, in GB.
    pub vram_gb: u32,
    /// Provider and `nvidia-smi` spellings. They pin the card, so memory is
    /// not checked against them.
    pub aliases: &'static [&'static str],
}

impl GpuCard {
    #[must_use]
    pub fn stem(&self) -> &'static str {
        self.id.rsplit_once('-').map_or(self.id, |(stem, _)| stem)
    }

    /// In GiB, the convention configs use for `accelerator.memory`.
    #[must_use]
    pub fn memory(&self) -> ByteSize {
        ByteSize::from_bytes(u64::from(self.vram_gb) * GIB)
    }

    #[must_use]
    pub fn memory_matches(&self, memory: ByteSize) -> bool {
        vram_matches(self.vram_gb, memory)
    }

    fn distance(&self, memory: ByteSize) -> f64 {
        vram_distance(self.vram_gb, memory)
    }

    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} ({} GB)", self.id, self.vram_gb)
    }
}

/// Relative tolerance between a memory size (read as GB or GiB) and a card's
/// nominal VRAM. Real `nvidia-smi` totals sit within 0.6% (81559 MiB for an
/// 80 GB H100, 143771 MiB for a 141 GB H200), while 94 vs 96 GB is 2.1% apart.
pub const VRAM_TOLERANCE: f64 = 0.02;

#[allow(clippy::cast_precision_loss)] // byte counts far below 2^52
fn vram_distance(vram_gb: u32, memory: ByteSize) -> f64 {
    let bytes = memory.bytes() as f64;
    let vram = f64::from(vram_gb);
    let rel = |unit: u64| ((bytes / unit as f64) - vram).abs() / vram;
    rel(GB).min(rel(GIB))
}

#[must_use]
pub fn vram_matches(vram_gb: u32, memory: ByteSize) -> bool {
    vram_gb > 0 && vram_distance(vram_gb, memory) <= VRAM_TOLERANCE
}

const fn nv(
    id: &'static str,
    family: &'static str,
    vram_gb: u32,
    aliases: &'static [&'static str],
) -> GpuCard {
    GpuCard {
        id,
        family,
        vendor: GpuVendor::Nvidia,
        vram_gb,
        aliases,
    }
}

const GPU_CARDS: &[GpuCard] = &[
    nv("t4-16gb", "t4", 16, &[]),
    nv("p100-16gb", "p100", 16, &[]),
    nv(
        "v100-16gb",
        "v100",
        16,
        &["Tesla V100-SXM2-16GB", "Tesla V100-PCIE-16GB"],
    ),
    nv(
        "v100-32gb",
        "v100",
        32,
        &["Tesla V100-SXM2-32GB", "Tesla V100-PCIE-32GB", "V100S"],
    ),
    nv("a10-24gb", "a10", 24, &[]),
    nv("a10g-24gb", "a10g", 24, &[]),
    nv("a40-48gb", "a40", 48, &[]),
    nv("l4-24gb", "l4", 24, &[]),
    nv("l40-48gb", "l40", 48, &[]),
    nv("l40s-48gb", "l40s", 48, &[]),
    nv("a100-sxm4-80gb", "a100", 80, &["NVIDIA A100-SXM4-80GB"]),
    nv(
        "a100-pcie-80gb",
        "a100",
        80,
        &["NVIDIA A100 80GB PCIe", "A100-PCIE-80GB"],
    ),
    nv("a100-sxm4-40gb", "a100", 40, &["NVIDIA A100-SXM4-40GB"]),
    nv(
        "a100-pcie-40gb",
        "a100",
        40,
        &["NVIDIA A100-PCIE-40GB", "A100 40GB PCIe"],
    ),
    nv(
        "h100-sxm-80gb",
        "h100",
        80,
        &["NVIDIA H100 80GB HBM3", "H100 SXM5", "H100-SXM5-80GB"],
    ),
    nv(
        "h100-pcie-80gb",
        "h100",
        80,
        &["NVIDIA H100 PCIe", "H100 80GB PCIe"],
    ),
    nv("h100-nvl-94gb", "h100", 94, &["NVIDIA H100 NVL"]),
    nv("h200-sxm-141gb", "h200", 141, &["H200 SXM"]),
    // RunPod lists the NVL at 143 GB; the nominal size is 141 GB.
    nv(
        "h200-nvl-141gb",
        "h200",
        141,
        &["NVIDIA H200 NVL", "h200-nvl-143gb"],
    ),
    nv("b200-180gb", "b200", 180, &[]),
    nv(
        "b300-288gb",
        "b300",
        288,
        &["NVIDIA B300 SXM6 AC", "B300 SXM6"],
    ),
    nv("rtx-a4000-16gb", "rtx-a4000", 16, &["A4000"]),
    nv("rtx-a4500-20gb", "rtx-a4500", 20, &["A4500"]),
    nv("rtx-a5000-24gb", "rtx-a5000", 24, &["A5000"]),
    nv("rtx-a6000-48gb", "rtx-a6000", 48, &["A6000"]),
    nv(
        "rtx-2000-ada-16gb",
        "rtx-2000-ada",
        16,
        &["NVIDIA RTX 2000 Ada Generation", "RTX 2000"],
    ),
    nv(
        "rtx-4000-ada-20gb",
        "rtx-4000-ada",
        20,
        &["NVIDIA RTX 4000 Ada Generation", "RTX 4000"],
    ),
    nv(
        "rtx-6000-ada-48gb",
        "rtx-6000-ada",
        48,
        &["NVIDIA RTX 6000 Ada Generation", "6000 Ada"],
    ),
    nv(
        "rtx-pro-4500-32gb",
        "rtx-pro-4500",
        32,
        &["NVIDIA RTX PRO 4500 Blackwell"],
    ),
    nv(
        "rtx-pro-6000-96gb",
        "rtx-pro-6000",
        96,
        &[
            "NVIDIA RTX PRO 6000 Blackwell Workstation Edition",
            "RTX PRO 6000 Blackwell",
        ],
    ),
    nv(
        "rtx-pro-6000-maxq-96gb",
        "rtx-pro-6000",
        96,
        &[
            "NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition",
            "RTX PRO 6000 Blackwell Max-Q",
        ],
    ),
    nv(
        "rtx-pro-6000-server-96gb",
        "rtx-pro-6000",
        96,
        &[
            "NVIDIA RTX PRO 6000 Blackwell Server Edition",
            "RTX PRO 6000 Blackwell Server",
        ],
    ),
    nv(
        "rtx-pro-6000-mig-48gb",
        "rtx-pro-6000",
        48,
        &["NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 2g.48gb"],
    ),
    nv(
        "rtx-pro-6000-mig-24gb",
        "rtx-pro-6000",
        24,
        &["NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb"],
    ),
    nv("rtx-3060-ti-8gb", "rtx-3060-ti", 8, &[]),
    nv("rtx-3090-24gb", "rtx-3090", 24, &["3090"]),
    nv("rtx-4070-ti-super-16gb", "rtx-4070-ti-super", 16, &[]),
    nv("rtx-4080-16gb", "rtx-4080", 16, &[]),
    nv("rtx-4090-24gb", "rtx-4090", 24, &["4090"]),
    nv("rtx-5090-32gb", "rtx-5090", 32, &["5090"]),
    GpuCard {
        id: "rx-7900-xtx-24gb",
        family: "rx-7900-xtx",
        vendor: GpuVendor::Amd,
        vram_gb: 24,
        aliases: &[],
    },
];

#[must_use]
pub fn gpu_cards() -> &'static [GpuCard] {
    GPU_CARDS
}

#[must_use]
pub fn gpu_card(id: &str) -> Option<&'static GpuCard> {
    GPU_CARDS.iter().find(|c| c.id == id)
}

#[must_use]
pub fn normalize_gpu_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    for noise in ["nvidia", "geforce", "tesla", "radeon", "amd"] {
        s = s.replace(noise, "");
    }
    s
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuResolveError {
    Unknown {
        model: String,
    },
    Ambiguous {
        model: String,
        memory: Option<ByteSize>,
        candidates: Vec<&'static GpuCard>,
    },
    MemoryMismatch {
        model: String,
        memory: ByteSize,
        candidates: Vec<&'static GpuCard>,
    },
}

fn list(cards: &[&GpuCard]) -> String {
    cards
        .iter()
        .map(|c| c.describe())
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for GpuResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { model } => write!(
                f,
                "`{model}` is not in the GPU catalog; use a canonical GPU id such as \
                 `h100-sxm-80gb` or `l4-24gb`"
            ),
            Self::Ambiguous {
                model,
                memory,
                candidates,
            } => {
                write!(f, "`{model}`")?;
                if let Some(memory) = memory {
                    write!(f, " with {memory}")?;
                }
                write!(
                    f,
                    " is ambiguous: {}; set `accelerator.model` to one of these ids",
                    list(candidates)
                )
            }
            Self::MemoryMismatch {
                model,
                memory,
                candidates,
            } => match candidates.as_slice() {
                [card] if card.id == model => write!(
                    f,
                    "`{model}` has {} GB of VRAM, but `accelerator.memory` is {memory}",
                    card.vram_gb
                ),
                _ => write!(
                    f,
                    "no `{model}` card has {memory} of VRAM: {}",
                    list(candidates)
                ),
            },
        }
    }
}

impl std::error::Error for GpuResolveError {}

fn alias_card(key: &str) -> Option<&'static GpuCard> {
    GPU_CARDS
        .iter()
        .find(|c| c.aliases.iter().any(|a| normalize_gpu_name(a) == key))
}

/// The card an exact alias pins when the declared `memory` is outside the
/// tolerance of its VRAM (resolution ignores `memory` for aliases).
#[must_use]
pub fn alias_memory_conflict(model: &str, memory: ByteSize) -> Option<&'static GpuCard> {
    alias_card(&normalize_gpu_name(model)).filter(|c| !c.memory_matches(memory))
}

/// Relative VRAM band for a probed total: `nvidia-smi` totals and provider
/// sizing can drift further from nominal than a declared `memory` does.
pub const PROBE_VRAM_TOLERANCE: f64 = 0.05;

/// Checks one probed accelerator (`nvidia-smi` name and total VRAM) against the
/// resolved `card`. The name is matched first, by card family; a name outside
/// the catalog is left to the VRAM check.
///
/// # Errors
/// A message naming the probed and expected card.
pub fn verify_probed_gpu(
    card: &GpuCard,
    name: Option<&str>,
    total: Option<ByteSize>,
) -> Result<(), String> {
    let mismatch = || {
        let probed = match (name, total) {
            (Some(n), Some(t)) => format!("{n} ({t})"),
            (Some(n), None) => n.to_owned(),
            (None, Some(t)) => t.to_string(),
            (None, None) => "unknown".to_owned(),
        };
        format!(
            "the running GPU is {probed}, not the declared {}",
            card.describe()
        )
    };
    if let Some(name) = name {
        let same_family = match resolve_gpu(name, total) {
            Ok(found) => Some(found.family == card.family),
            Err(
                GpuResolveError::Ambiguous { candidates, .. }
                | GpuResolveError::MemoryMismatch { candidates, .. },
            ) => Some(candidates.iter().any(|c| c.family == card.family)),
            Err(GpuResolveError::Unknown { .. }) => None,
        };
        if same_family == Some(false) {
            return Err(mismatch());
        }
    }
    match total {
        Some(total) if vram_distance(card.vram_gb, total) > PROBE_VRAM_TOLERANCE => Err(mismatch()),
        _ => Ok(()),
    }
}

/// Resolve `model` (with the optional per-card `memory`) to exactly one card.
///
/// A canonical id must match `memory`; an alias pins its card regardless. A
/// family, family+variant (`a100-sxm4`) or family+size (`a100-80gb`) must be
/// narrowed by `memory` (nearest VRAM wins) to a single card. Matching ignores
/// case, punctuation and vendor words. An alias that disagrees with `memory`
/// still resolves; [`alias_memory_conflict`] reports it.
pub fn resolve_gpu(
    model: &str,
    memory: Option<ByteSize>,
) -> Result<&'static GpuCard, GpuResolveError> {
    let key = normalize_gpu_name(model);
    let unknown = || GpuResolveError::Unknown {
        model: model.to_owned(),
    };
    if key.is_empty() {
        return Err(unknown());
    }

    if let Some(card) = alias_card(&key) {
        return Ok(card);
    }
    if let Some(card) = GPU_CARDS.iter().find(|c| normalize_gpu_name(c.id) == key) {
        return match memory {
            Some(memory) if !card.memory_matches(memory) => Err(GpuResolveError::MemoryMismatch {
                model: model.to_owned(),
                memory,
                candidates: vec![card],
            }),
            _ => Ok(card),
        };
    }

    let mut group: Vec<&'static GpuCard> = GPU_CARDS
        .iter()
        .filter(|c| normalize_gpu_name(c.family) == key || normalize_gpu_name(c.stem()) == key)
        .collect();
    if group.is_empty() {
        // `a100-80gb`: a family or stem plus a size.
        group = GPU_CARDS
            .iter()
            .filter(|c| {
                [c.family, c.stem()].iter().any(|prefix| {
                    key.strip_prefix(&normalize_gpu_name(prefix))
                        .and_then(|rest| rest.strip_suffix("gb"))
                        .is_some_and(|n| {
                            !n.starts_with('0') && n.parse::<u32>().is_ok_and(|n| n == c.vram_gb)
                        })
                })
            })
            .collect();
    }
    if group.is_empty() {
        return Err(unknown());
    }

    let fitting: Vec<&'static GpuCard> = match memory {
        Some(memory) => {
            let matching: Vec<&'static GpuCard> = group
                .iter()
                .copied()
                .filter(|c| c.memory_matches(memory))
                .collect();
            let best = matching
                .iter()
                .map(|c| c.distance(memory))
                .fold(f64::INFINITY, f64::min);
            matching
                .into_iter()
                .filter(|c| (c.distance(memory) - best).abs() < 1e-9)
                .collect()
        }
        None => group.clone(),
    };
    match fitting.as_slice() {
        [card] => Ok(card),
        [] => Err(GpuResolveError::MemoryMismatch {
            model: model.to_owned(),
            memory: memory.expect("an empty match implies a memory filter"),
            candidates: group,
        }),
        _ => Err(GpuResolveError::Ambiguous {
            model: model.to_owned(),
            memory,
            candidates: fitting,
        }),
    }
}

/// Providers that place GPUs by catalog card, so they cannot deploy a model
/// that does not resolve.
pub const CATALOG_GPU_PROVIDERS: [&str; 2] = ["modal", "runpod"];

/// The card a profile on `provider` runs on. For a short name that is now
/// ambiguous this is the card `modal`/`runpod` picked before exact names were
/// required, so rewriting `h100` to `h100-sxm-80gb` is not a config change.
#[must_use]
pub fn effective_gpu_card(
    provider: &str,
    model: &str,
    memory: Option<ByteSize>,
) -> Option<&'static GpuCard> {
    match resolve_gpu(model, memory) {
        Ok(card) => Some(card),
        Err(GpuResolveError::Ambiguous { candidates, .. })
            if CATALOG_GPU_PROVIDERS.contains(&provider) =>
        {
            let legacy = |id: &str| candidates.iter().copied().find(|c| c.id == id);
            let sxm: Vec<&'static GpuCard> = candidates
                .iter()
                .copied()
                .filter(|c| {
                    let stem = c.stem();
                    stem.ends_with("-sxm") || stem.ends_with("-sxm4")
                })
                .collect();
            match sxm.as_slice() {
                [card] => Some(card),
                _ if provider == "runpod" => legacy("rtx-pro-6000-96gb"),
                _ => legacy("rtx-pro-6000-server-96gb"),
            }
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[allow(clippy::unnecessary_wraps)] // reads as the optional memory argument
    fn gib(n: u64) -> Option<ByteSize> {
        Some(ByteSize::from_bytes(n * GIB))
    }

    #[allow(clippy::unnecessary_wraps)]
    fn gb(n: u64) -> Option<ByteSize> {
        Some(ByteSize::from_bytes(n * GB))
    }

    fn id(model: &str, memory: Option<ByteSize>) -> &'static str {
        resolve_gpu(model, memory)
            .unwrap_or_else(|e| panic!("{model}: {e}"))
            .id
    }

    #[test]
    fn ids_follow_the_scheme() {
        let mut ids = HashSet::new();
        for c in GPU_CARDS {
            assert!(ids.insert(c.id), "duplicate id {}", c.id);
            assert!(
                c.id.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{}",
                c.id
            );
            assert!(c.id.starts_with(c.family), "{} not in {}", c.id, c.family);
            assert!(
                c.id.ends_with(&format!("-{}gb", c.vram_gb)),
                "{} must end with its VRAM",
                c.id
            );
        }
    }

    #[test]
    fn exact_names_are_unique_and_never_a_family_or_stem() {
        let mut groups: HashMap<String, HashSet<&str>> = HashMap::new();
        for c in GPU_CARDS {
            for g in [c.family, c.stem()] {
                groups
                    .entry(normalize_gpu_name(g))
                    .or_default()
                    .insert(c.id);
            }
        }
        let mut seen: HashMap<String, &str> = HashMap::new();
        for c in GPU_CARDS {
            for name in std::iter::once(&c.id).chain(c.aliases) {
                let key = normalize_gpu_name(name);
                assert_ne!(key, "");
                // An exact name may only spell a group that is this card alone,
                // or it would bypass the family's ambiguity check.
                if let Some(members) = groups.get(&key) {
                    assert_eq!(
                        members,
                        &HashSet::from([c.id]),
                        "`{name}` shadows a family or stem"
                    );
                }
                if let Some(other) = seen.insert(key, c.id) {
                    assert_eq!(other, c.id, "`{name}` names two cards");
                }
            }
        }
    }

    #[test]
    fn every_card_resolves_by_its_id_and_aliases() {
        for c in GPU_CARDS {
            assert_eq!(id(c.id, None), c.id);
            assert_eq!(id(c.id, Some(c.memory())), c.id);
            for a in c.aliases {
                assert_eq!(id(a, None), c.id, "{a}");
            }
        }
    }

    #[test]
    fn provider_spellings_resolve() {
        assert_eq!(id("NVIDIA A100-SXM4-80GB", None), "a100-sxm4-80gb");
        assert_eq!(id("NVIDIA A100 80GB PCIe", None), "a100-pcie-80gb");
        assert_eq!(id("NVIDIA H100 80GB HBM3", None), "h100-sxm-80gb");
        assert_eq!(id("NVIDIA GeForce RTX 4090", None), "rtx-4090-24gb");
        assert_eq!(id("rtx_4090", None), "rtx-4090-24gb");
        assert_eq!(id("Tesla T4", None), "t4-16gb");
        assert_eq!(id("nvidia-tesla-t4", None), "t4-16gb");
        assert_eq!(id("L4", gib(24)), "l4-24gb");
        assert_eq!(id("NVIDIA L40S", None), "l40s-48gb");
        assert_eq!(id("AMD Radeon RX 7900 XTX", None), "rx-7900-xtx-24gb");
        assert_eq!(id("h100-sxm", None), "h100-sxm-80gb");
    }

    #[test]
    fn unique_families_resolve_without_memory() {
        assert_eq!(id("l4", None), "l4-24gb");
        assert_eq!(id("l40s", gib(48)), "l40s-48gb");
        assert_eq!(id("rtx-a6000", None), "rtx-a6000-48gb");
    }

    #[test]
    fn memory_disambiguates_a_family() {
        assert_eq!(id("h100", gib(94)), "h100-nvl-94gb");
        assert_eq!(id("h200-nvl-143gb", None), "h200-nvl-141gb");
        assert!(matches!(
            resolve_gpu("h200", gb(141)),
            Err(GpuResolveError::Ambiguous { .. })
        ));
        assert_eq!(id("rtx-pro-6000", gib(48)), "rtx-pro-6000-mig-48gb");
        assert_eq!(id("a100-sxm4", gib(40)), "a100-sxm4-40gb");
        assert_eq!(id("v100", gib(32)), "v100-32gb");
    }

    #[test]
    fn ambiguous_family_lists_variants_with_vram() {
        let err = resolve_gpu("a100", None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "`a100` is ambiguous: a100-sxm4-80gb (80 GB), a100-pcie-80gb (80 GB), \
             a100-sxm4-40gb (40 GB), a100-pcie-40gb (40 GB); set `accelerator.model` to one \
             of these ids"
        );
        let err = resolve_gpu("a100", gib(80)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "`a100` with 80GiB is ambiguous: a100-sxm4-80gb (80 GB), a100-pcie-80gb (80 GB); \
             set `accelerator.model` to one of these ids"
        );
        assert!(matches!(
            resolve_gpu("a100-80gb", None),
            Err(GpuResolveError::Ambiguous { candidates, .. }) if candidates.len() == 2
        ));
        assert!(matches!(
            resolve_gpu("h100", gib(80)),
            Err(GpuResolveError::Ambiguous { .. })
        ));
    }

    #[test]
    fn memory_must_match_the_card() {
        assert_eq!(id("a100-sxm4-80gb", gib(80)), "a100-sxm4-80gb");
        assert_eq!(id("a100-sxm4-80gb", gb(80)), "a100-sxm4-80gb");
        let err = resolve_gpu("h100-nvl-94gb", gib(96)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "`h100-nvl-94gb` has 94 GB of VRAM, but `accelerator.memory` is 96GiB"
        );
        let err = resolve_gpu("h100", gib(96)).unwrap_err();
        assert!(matches!(err, GpuResolveError::MemoryMismatch { .. }));
        assert!(
            err.to_string()
                .starts_with("no `h100` card has 96GiB of VRAM")
        );
        assert!(resolve_gpu("a100-40gb", gib(80)).is_err());
    }

    #[test]
    fn real_nvidia_smi_totals_match_their_card() {
        const MIB: u64 = 1 << 20;
        let smi = |mib: u64| Some(ByteSize::from_bytes(mib * MIB));
        for (name, mib, want) in [
            ("h100-sxm-80gb", 81_559, "h100-sxm-80gb"),
            ("h200-sxm-141gb", 143_771, "h200-sxm-141gb"),
            ("b200", 183_359, "b200-180gb"),
            ("a100-sxm4", 40_960, "a100-sxm4-40gb"),
            ("a100-sxm4", 81_920, "a100-sxm4-80gb"),
            ("l4", 23_034, "l4-24gb"),
            ("rtx-4090", 24_564, "rtx-4090-24gb"),
            ("h100", 95_830, "h100-nvl-94gb"),
        ] {
            assert_eq!(id(name, smi(mib)), want, "{name} {mib} MiB");
        }
        assert_eq!(id("NVIDIA A100-SXM4-80GB", smi(40_960)), "a100-sxm4-80gb");
        assert!(!vram_matches(94, ByteSize::from_bytes(96 * GIB)));
        assert!(vram_matches(141, ByteSize::from_bytes(143 * GB)));
    }

    #[test]
    fn legacy_short_names_keep_their_former_card_on_catalog_providers() {
        let eff = |provider: &str, model: &str, gib: u64| {
            effective_gpu_card(provider, model, Some(ByteSize::from_bytes(gib * GIB))).map(|c| c.id)
        };
        assert_eq!(eff("runpod", "h100", 80), Some("h100-sxm-80gb"));
        assert_eq!(eff("modal", "a100", 80), Some("a100-sxm4-80gb"));
        assert_eq!(eff("modal", "a100", 40), Some("a100-sxm4-40gb"));
        assert_eq!(eff("runpod", "rtx-pro-6000", 96), Some("rtx-pro-6000-96gb"));
        assert_eq!(
            eff("modal", "rtx-pro-6000", 96),
            Some("rtx-pro-6000-server-96gb")
        );
        assert_eq!(eff("on_prem", "h100", 80), None);
        assert_eq!(eff("runpod", "h100-sxm-80gb", 80), Some("h100-sxm-80gb"));
    }

    #[test]
    fn unknown_names_are_errors() {
        for model in ["", "potato", "mi300x", "a1000gb", "a10080"] {
            assert!(
                matches!(
                    resolve_gpu(model, None),
                    Err(GpuResolveError::Unknown { .. })
                ),
                "{model}"
            );
        }
        // `a10` must not swallow `a100-80gb`'s size.
        assert_ne!(resolve_gpu("a100-80gb", None).map(|c| c.id), Ok("a10-24gb"));
    }

    #[test]
    fn probed_gpus_are_verified_by_name_then_vram() {
        const MIB: u64 = 1 << 20;
        let card = gpu_card("h200-sxm-141gb").unwrap();
        let mib = |n: u64| Some(ByteSize::from_bytes(n * MIB));
        assert!(verify_probed_gpu(card, Some("NVIDIA H200"), mib(143_771)).is_ok());
        let h100 = gpu_card("h100-sxm-80gb").unwrap();
        assert!(verify_probed_gpu(h100, Some("NVIDIA H100 80GB HBM3"), mib(81_559)).is_ok());
        let err = verify_probed_gpu(h100, Some("NVIDIA H200"), mib(143_771)).unwrap_err();
        assert!(
            err.contains("H200") && err.contains("h100-sxm-80gb"),
            "{err}"
        );
        assert!(verify_probed_gpu(h100, Some("NVIDIA A100-SXM4-80GB"), mib(81_920)).is_err());
        assert!(verify_probed_gpu(h100, Some("Mystery GPU"), mib(143_771)).is_err());
        assert!(verify_probed_gpu(h100, Some("Mystery GPU"), mib(81_559)).is_ok());
        assert!(verify_probed_gpu(h100, Some("NVIDIA H100 80GB HBM3"), None).is_ok());
    }

    #[test]
    fn an_alias_with_a_disagreeing_memory_is_reported() {
        let size = |g: u64| ByteSize::from_bytes(g * GIB);
        assert!(alias_memory_conflict("NVIDIA H100 80GB HBM3", size(40)).is_some());
        assert!(alias_memory_conflict("NVIDIA H100 80GB HBM3", size(80)).is_none());
        assert!(alias_memory_conflict("h100-sxm-80gb", size(40)).is_none());
    }
}
