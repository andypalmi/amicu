//! The provider-agnostic GPU catalog: one entry per exact card variant, with a
//! canonical id, its family, vendor, VRAM and the spellings it goes by.
//!
//! # Canonical ids
//!
//! `<family>[-<variant>]-<vram>gb`, lowercase and hyphenated:
//!
//! - `family` is the short product name users already write (`a100`, `h100`,
//!   `l40s`, `rtx-4090`, `rtx-pro-6000`).
//! - `variant` is present only when the family ships in more than one form
//!   factor or edition with otherwise identical names: `sxm` / `sxm4`
//!   (vendor's own spelling for that generation), `pcie`, `nvl`, `maxq`,
//!   `server`, `mig`.
//! - `vram` is the vendor's nominal VRAM per card, in GB, always present, so an
//!   id is unambiguous on its own (`a100-sxm4-40gb` vs `a100-sxm4-80gb`).
//!
//! # Resolving `accelerator.model`
//!
//! [`resolve_gpu`] maps a model string plus an optional memory size onto
//! exactly one card:
//!
//! 1. A canonical id or an exact alias (a provider spelling such as RunPod's
//!    `NVIDIA A100-SXM4-80GB` or `nvidia-smi`'s `NVIDIA H100 80GB HBM3`) names
//!    one card. If a memory size is also given it must match that card's VRAM.
//! 2. A family (`a100`), a family plus variant without the size (`a100-sxm4`)
//!    or a family plus size (`a100-80gb`) names a group of cards. The memory
//!    size narrows the group; it must end with exactly one card, otherwise the
//!    error lists the candidates with their VRAM.
//!
//! Matching ignores case and punctuation and drops the vendor words `nvidia`,
//! `geforce`, `tesla`, `amd` and `radeon`. A memory size matches a card when
//! it rounds to the card's VRAM read either as GB or as GiB, so `80GiB`
//! matches an 80 GB card but `96GiB` does not match a 94 GB one.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::units::ByteSize;

const GB: u64 = 1_000_000_000;
const GIB: u64 = 1 << 30;

/// Who makes the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuVendor {
    Nvidia,
    Amd,
}

/// One exact GPU variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuCard {
    /// Canonical id, e.g. `a100-sxm4-80gb`. See the module docs for the scheme.
    pub id: &'static str,
    /// Short family name, e.g. `a100`. Never resolves to a card on its own
    /// unless the family has a single card (or memory picks one).
    pub family: &'static str,
    pub vendor: GpuVendor,
    /// Nominal VRAM per card, in GB.
    pub vram_gb: u32,
    /// Other exact spellings of this card (provider type ids, `nvidia-smi`
    /// names). Matched after normalization; see the module docs.
    pub aliases: &'static [&'static str],
}

impl GpuCard {
    /// The id without its `-<vram>gb` suffix (`a100-sxm4`).
    #[must_use]
    pub fn stem(&self) -> &'static str {
        self.id.rsplit_once('-').map_or(self.id, |(stem, _)| stem)
    }

    /// The card's VRAM as a [`ByteSize`], in GiB (`80GiB` for an 80 GB card),
    /// the convention amicu's configs use for `accelerator.memory`.
    #[must_use]
    pub fn memory(&self) -> ByteSize {
        ByteSize::from_bytes(u64::from(self.vram_gb) * GIB)
    }

    /// Whether `memory` is this card's VRAM, read as GB or as GiB.
    #[must_use]
    pub fn memory_matches(&self, memory: ByteSize) -> bool {
        let vram = u64::from(self.vram_gb);
        let bytes = memory.bytes();
        rounded(bytes, GB) == vram || rounded(bytes, GIB) == vram
    }

    /// `a100-sxm4-80gb (80 GB)`.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} ({} GB)", self.id, self.vram_gb)
    }
}

fn rounded(bytes: u64, unit: u64) -> u64 {
    (bytes + unit / 2) / unit
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
    // --- NVIDIA data center ---
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
    nv("h200-nvl-143gb", "h200", 143, &["NVIDIA H200 NVL"]),
    nv("b200-180gb", "b200", 180, &[]),
    nv(
        "b300-288gb",
        "b300",
        288,
        &["NVIDIA B300 SXM6 AC", "B300 SXM6"],
    ),
    // --- NVIDIA workstation ---
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
    // --- NVIDIA consumer ---
    nv("rtx-3060-ti-8gb", "rtx-3060-ti", 8, &[]),
    nv("rtx-3090-24gb", "rtx-3090", 24, &["3090"]),
    nv("rtx-4070-ti-super-16gb", "rtx-4070-ti-super", 16, &[]),
    nv("rtx-4080-16gb", "rtx-4080", 16, &[]),
    nv("rtx-4090-24gb", "rtx-4090", 24, &["4090"]),
    nv("rtx-5090-32gb", "rtx-5090", 32, &["5090"]),
    // --- AMD ---
    GpuCard {
        id: "rx-7900-xtx-24gb",
        family: "rx-7900-xtx",
        vendor: GpuVendor::Amd,
        vram_gb: 24,
        aliases: &[],
    },
];

/// Every card in the catalog.
#[must_use]
pub fn gpu_cards() -> &'static [GpuCard] {
    GPU_CARDS
}

/// The card with this exact canonical id.
#[must_use]
pub fn gpu_card(id: &str) -> Option<&'static GpuCard> {
    GPU_CARDS.iter().find(|c| c.id == id)
}

/// Lowercase, keep `[a-z0-9]`, drop vendor words.
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

/// Why a model string did not resolve to exactly one card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuResolveError {
    /// Nothing in the catalog goes by this name.
    Unknown { model: String },
    /// Several cards fit; `candidates` are the ones still in the running.
    Ambiguous {
        model: String,
        memory: Option<ByteSize>,
        candidates: Vec<&'static GpuCard>,
    },
    /// The name is known but no card of it has this much VRAM.
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

/// Resolve `model` (and optionally the declared per-card `memory`) to exactly
/// one card. See the module docs for the rules.
///
/// # Errors
/// [`GpuResolveError`] when the name is unknown, still ambiguous, or no card
/// of it has `memory`.
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

    if let Some(card) = GPU_CARDS.iter().find(|c| {
        normalize_gpu_name(c.id) == key || c.aliases.iter().any(|a| normalize_gpu_name(a) == key)
    }) {
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
        Some(memory) => group
            .iter()
            .copied()
            .filter(|c| c.memory_matches(memory))
            .collect(),
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
        // Group name -> the cards it covers.
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
        assert_eq!(id("h200", gb(141)), "h200-sxm-141gb");
        assert_eq!(id("h200", gb(143)), "h200-nvl-143gb");
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
        // GiB and GB spellings of the same nominal size both match.
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
        // Size spelled in the name and as memory must agree.
        assert!(resolve_gpu("a100-40gb", gib(80)).is_err());
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
}
