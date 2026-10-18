//! Map an amicu GPU spec onto RunPod serverless GPU pool ids, a per-worker
//! count, and the GPU types to exclude so only the requested card is eligible.
//!
//! The accelerator's `model` (plus `memory`) first resolves to exactly one
//! card of the shared GPU catalog ([`amicu_core::resource::resolve_gpu`]); an
//! ambiguous name such as `a100` is an error, never a silent pick. The table
//! below then maps that canonical id to RunPod's GPU type id and pool.
//!
//! RunPod serverless endpoints select hardware by **pool** (`ADA_24`,
//! `AMPERE_80`, ...), not by GPU type id (that is what pods use), and a pool
//! can hold several card models. To make the requested card the *only*
//! eligible one, the placement lists the pool and puts every other type of that
//! pool in `excludedTypes`. Cards with no pool are a typed error rather than a
//! silent fallback, so a deployment never lands on different hardware than the
//! plan said.
//!
//! The table is static and serves offline planning. At deploy time the
//! exclusion list is recomputed from the live `GET /v2/catalog/gpus`
//! ([`exclusions_from_catalog`]), so a card RunPod later adds to a pool is
//! still excluded, and the live entry's memory is checked against the card's
//! VRAM ([`verify_catalog_memory`]); the table's own exclusions are only the
//! offline approximation.

use amicu_core::resource::{Accelerator, AcceleratorKind, GpuResolveError, resolve_gpu};

use crate::client::GpuType;
use crate::error::RunPodError;

/// One GPU catalog card RunPod serves: its canonical id, serverless pool and
/// RunPod's GPU type id (as used in `excludedTypes`).
struct Card {
    id: &'static str,
    pool: &'static str,
    type_id: &'static str,
}

const fn card(id: &'static str, pool: &'static str, type_id: &'static str) -> Card {
    Card { id, pool, type_id }
}

const CARDS: &[Card] = &[
    card("rtx-a4000-16gb", "AMPERE_16", "NVIDIA RTX A4000"),
    card("rtx-a4500-20gb", "AMPERE_16", "NVIDIA RTX A4500"),
    card(
        "rtx-4000-ada-20gb",
        "AMPERE_16",
        "NVIDIA RTX 4000 Ada Generation",
    ),
    card(
        "rtx-2000-ada-16gb",
        "AMPERE_16",
        "NVIDIA RTX 2000 Ada Generation",
    ),
    card("l4-24gb", "AMPERE_24", "NVIDIA L4"),
    card("rtx-a5000-24gb", "AMPERE_24", "NVIDIA RTX A5000"),
    card("rtx-3090-24gb", "AMPERE_24", "NVIDIA GeForce RTX 3090"),
    card("rtx-4090-24gb", "ADA_24", "NVIDIA GeForce RTX 4090"),
    card("rtx-5090-32gb", "ADA_32_PRO", "NVIDIA GeForce RTX 5090"),
    card("a40-48gb", "AMPERE_48", "NVIDIA A40"),
    card("rtx-a6000-48gb", "AMPERE_48", "NVIDIA RTX A6000"),
    card("l40-48gb", "ADA_48_PRO", "NVIDIA L40"),
    card("l40s-48gb", "ADA_48_PRO", "NVIDIA L40S"),
    card(
        "rtx-6000-ada-48gb",
        "ADA_48_PRO",
        "NVIDIA RTX 6000 Ada Generation",
    ),
    card("a100-sxm4-80gb", "AMPERE_80", "NVIDIA A100-SXM4-80GB"),
    card("a100-pcie-80gb", "AMPERE_80", "NVIDIA A100 80GB PCIe"),
    card("h100-sxm-80gb", "ADA_80_PRO", "NVIDIA H100 80GB HBM3"),
    card("h100-pcie-80gb", "ADA_80_PRO", "NVIDIA H100 PCIe"),
    card("h100-nvl-94gb", "ADA_80_PRO", "NVIDIA H100 NVL"),
    card("h200-sxm-141gb", "HOPPER_141", "NVIDIA H200"),
    card(
        "rtx-pro-6000-96gb",
        "BLACKWELL_96",
        "NVIDIA RTX PRO 6000 Blackwell Workstation Edition",
    ),
    card(
        "rtx-pro-6000-maxq-96gb",
        "BLACKWELL_96",
        "NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition",
    ),
    card(
        "rtx-pro-6000-server-96gb",
        "BLACKWELL_96",
        "NVIDIA RTX PRO 6000 Blackwell Server Edition",
    ),
    // MIG slices of the RTX PRO 6000 Server Edition share pools with whole
    // cards; listed so they are excluded when the whole card is requested.
    card(
        "rtx-pro-6000-mig-48gb",
        "ADA_48_PRO",
        "NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 2g.48gb",
    ),
    card(
        "rtx-pro-6000-mig-24gb",
        "AMPERE_24",
        "NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb",
    ),
    card(
        "rtx-pro-4500-32gb",
        "BLACKWELL_32",
        "NVIDIA RTX PRO 4500 Blackwell",
    ),
    card("b200-180gb", "BLACKWELL_180", "NVIDIA B200"),
    card("b300-288gb", "BLACKWELL_288", "NVIDIA B300 SXM6 AC"),
];

/// Where a deployment's GPUs go: the pool ids to list under `gpu.pools`, the
/// GPUs per worker for `gpu.count`, and the other types of those pools to list
/// under `gpu.excludedTypes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuPlacement {
    /// Serverless GPU pool ids.
    pub pools: Vec<String>,
    /// GPUs per worker.
    pub count: u32,
    /// GPU type ids excluded so only the requested card is eligible.
    pub excluded_types: Vec<String>,
    /// RunPod's GPU type id of the requested card (`"NVIDIA L40S"`).
    pub type_id: String,
    /// The card's canonical GPU catalog id (`"l40s-48gb"`).
    pub card_id: String,
    /// The card's VRAM in GB, checked against the live catalog at deploy.
    pub vram_gb: u32,
}

fn card_for(
    model: &str,
    memory: Option<amicu_core::ByteSize>,
) -> Result<(&'static Card, u32), RunPodError> {
    let resolved = resolve_gpu(model, memory).map_err(|e| match e {
        GpuResolveError::Unknown { .. } => RunPodError::UnknownGpu {
            model: model.to_owned(),
        },
        other => RunPodError::InvalidGpu(other.to_string()),
    })?;
    CARDS
        .iter()
        .find(|c| c.id == resolved.id)
        .map(|c| (c, resolved.vram_gb))
        .ok_or_else(|| RunPodError::UnknownGpu {
            model: resolved.id.to_owned(),
        })
}

/// Map an amicu [`Accelerator`] (`model` resolved with its `memory`) to a
/// RunPod [`GpuPlacement`].
pub fn gpu_placement(accelerator: &Accelerator) -> Result<GpuPlacement, RunPodError> {
    if accelerator.kind != AcceleratorKind::Gpu {
        return Err(RunPodError::InvalidGpu(format!(
            "RunPod serverless has no {:?} offering; only GPUs can be deployed there",
            accelerator.kind
        )));
    }
    placement(
        &accelerator.model,
        Some(accelerator.memory),
        accelerator.count,
    )
}

/// The `excludedTypes` for `type_id` in `pool`, from the live GPU catalog:
/// every other type RunPod lists in that pool, so only the requested card is
/// eligible even after RunPod adds a card the static table does not know.
///
/// # Errors
/// [`RunPodError::InvalidGpu`] when the catalog does not list `type_id` in
/// `pool` (the static table is out of date; deploying could land elsewhere).
pub fn exclusions_from_catalog(
    type_id: &str,
    pool: &str,
    catalog: &[GpuType],
) -> Result<Vec<String>, RunPodError> {
    let members: Vec<&GpuType> = catalog
        .iter()
        .filter(|g| g.pool.as_deref() == Some(pool))
        .collect();
    if !members.iter().any(|g| g.id == type_id) {
        return Err(RunPodError::InvalidGpu(format!(
            "RunPod's GPU catalog does not list `{type_id}` in pool `{pool}`"
        )));
    }
    Ok(members
        .into_iter()
        .filter(|g| g.id != type_id)
        .map(|g| g.id.clone())
        .collect())
}

/// Check that the live catalog's memory for `type_id` is the requested card's
/// VRAM, so a RunPod type id that came to mean a different variant is caught
/// before deploying. An entry without a memory figure is not checked.
///
/// # Errors
/// [`RunPodError::InvalidGpu`] when the catalog lists a different memory size,
/// or does not list `type_id` at all.
pub fn verify_catalog_memory(
    type_id: &str,
    vram_gb: u32,
    catalog: &[GpuType],
) -> Result<(), RunPodError> {
    let entry = catalog.iter().find(|g| g.id == type_id).ok_or_else(|| {
        RunPodError::InvalidGpu(format!("RunPod's GPU catalog does not list `{type_id}`"))
    })?;
    match entry.memory {
        Some(memory) if memory != vram_gb => Err(RunPodError::InvalidGpu(format!(
            "RunPod's GPU catalog lists `{type_id}` with {memory} GB, but the requested card \
             has {vram_gb} GB"
        ))),
        _ => Ok(()),
    }
}

/// Map a Modal-style spec string (`"h100-sxm-80gb"`, `"h100-sxm-80gb:2"`) to
/// a [`GpuPlacement`]. With no memory to go on, the model must name exactly
/// one card on its own (a canonical id, an exact alias, or a single-card
/// family such as `l4`).
pub fn parse_gpu_spec(spec: &str) -> Result<GpuPlacement, RunPodError> {
    let (model, count) = match spec.split_once(':') {
        Some((model, count)) => {
            let count: u32 = count
                .trim()
                .parse()
                .map_err(|_| RunPodError::InvalidGpu(format!("bad GPU count in `{spec}`")))?;
            (model, count)
        }
        None => (spec, 1),
    };
    placement(model.trim(), None, count)
}

fn placement(
    model: &str,
    memory: Option<amicu_core::ByteSize>,
    count: u32,
) -> Result<GpuPlacement, RunPodError> {
    if count == 0 {
        return Err(RunPodError::InvalidGpu(
            "GPU count must be at least 1".into(),
        ));
    }
    let (card, vram_gb) = card_for(model, memory)?;
    Ok(GpuPlacement {
        pools: vec![card.pool.to_owned()],
        count,
        excluded_types: CARDS
            .iter()
            .filter(|c| c.pool == card.pool && c.type_id != card.type_id)
            .map(|c| c.type_id.to_owned())
            .collect(),
        type_id: card.type_id.to_owned(),
        card_id: card.id.to_owned(),
        vram_gb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::resource::gpu_card;
    use amicu_core::units::ByteSize;

    const GIB: u64 = 1 << 30;

    fn acc(kind: AcceleratorKind, model: &str, count: u32, vram_gib: u64) -> Accelerator {
        Accelerator {
            kind,
            model: model.to_owned(),
            count,
            memory: ByteSize::from_bytes(vram_gib * GIB),
            memory_bandwidth_gbps: None,
        }
    }

    fn placed(model: &str) -> GpuPlacement {
        parse_gpu_spec(model).unwrap_or_else(|e| panic!("{model}: {e}"))
    }

    #[test]
    fn every_table_entry_is_a_catalog_card() {
        let mut ids = std::collections::HashSet::new();
        let mut type_ids = std::collections::HashSet::new();
        for c in CARDS {
            assert!(gpu_card(c.id).is_some(), "{} is not a catalog id", c.id);
            assert!(ids.insert(c.id), "{}", c.id);
            assert!(type_ids.insert(c.type_id), "{}", c.type_id);
            // RunPod's own type id resolves back to the same card.
            let vram = gpu_card(c.id).unwrap().memory();
            assert_eq!(resolve_gpu(c.type_id, Some(vram)).unwrap().id, c.id);
        }
    }

    #[test]
    fn maps_models_to_pools() {
        for (model, gib, pool) in [
            ("h100-sxm-80gb", 80, "ADA_80_PRO"),
            ("h100", 94, "ADA_80_PRO"),
            ("a100-sxm4-80gb", 80, "AMPERE_80"),
            ("l40s", 48, "ADA_48_PRO"),
            ("rtx-a6000", 48, "AMPERE_48"),
            ("rtx-4090", 24, "ADA_24"),
            ("NVIDIA GeForce RTX 4090", 24, "ADA_24"),
            ("rtx_3090", 24, "AMPERE_24"),
            ("rtx-a4000", 16, "AMPERE_16"),
            ("rtx-a4500", 20, "AMPERE_16"),
            ("rtx-a5000", 24, "AMPERE_24"),
            ("rtx-5090", 32, "ADA_32_PRO"),
            ("h200", 141, "HOPPER_141"),
            ("b200", 180, "BLACKWELL_180"),
            // RunPod's own GPU type names.
            ("NVIDIA RTX 4000 Ada Generation", 20, "AMPERE_16"),
            ("NVIDIA RTX 6000 Ada Generation", 48, "ADA_48_PRO"),
            ("NVIDIA H100 80GB HBM3", 80, "ADA_80_PRO"),
            ("NVIDIA H100 PCIe", 80, "ADA_80_PRO"),
            ("NVIDIA H100 NVL", 94, "ADA_80_PRO"),
            ("NVIDIA A100-SXM4-80GB", 80, "AMPERE_80"),
            ("NVIDIA A100 80GB PCIe", 80, "AMPERE_80"),
            ("NVIDIA GeForce RTX 5090", 32, "ADA_32_PRO"),
            ("NVIDIA RTX A6000", 48, "AMPERE_48"),
        ] {
            let p = gpu_placement(&acc(AcceleratorKind::Gpu, model, 1, gib))
                .unwrap_or_else(|e| panic!("{model}: {e}"));
            assert_eq!(p.pools, vec![pool.to_owned()], "{model}");
            assert_eq!(p.count, 1);
        }
    }

    #[test]
    fn ambiguous_models_are_rejected_not_guessed() {
        // Both A100 80 GB variants share a pool; a bare `a100` used to pick SXM.
        let err = gpu_placement(&acc(AcceleratorKind::Gpu, "a100", 1, 80)).unwrap_err();
        assert!(
            matches!(err, RunPodError::InvalidGpu(ref m) if m.contains("ambiguous")),
            "{err}"
        );
        assert!(matches!(
            gpu_placement(&acc(AcceleratorKind::Gpu, "h100", 1, 80)),
            Err(RunPodError::InvalidGpu(_))
        ));
        // A size the card does not have.
        assert!(matches!(
            gpu_placement(&acc(AcceleratorKind::Gpu, "h100-nvl-94gb", 1, 96)),
            Err(RunPodError::InvalidGpu(_))
        ));
    }

    #[test]
    fn only_the_requested_card_stays_eligible() {
        assert_eq!(
            placed("l40s").excluded_types,
            vec![
                "NVIDIA L40",
                "NVIDIA RTX 6000 Ada Generation",
                "NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 2g.48gb"
            ]
        );
        assert_eq!(
            placed("h100-sxm-80gb").excluded_types,
            vec!["NVIDIA H100 PCIe", "NVIDIA H100 NVL"]
        );
        assert_eq!(
            placed("h100-pcie").excluded_types,
            vec!["NVIDIA H100 80GB HBM3", "NVIDIA H100 NVL"]
        );
        assert_eq!(
            placed("rtx_3090").excluded_types,
            vec![
                "NVIDIA L4",
                "NVIDIA RTX A5000",
                "NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 1g.24gb"
            ]
        );
        assert_eq!(placed("rtx-a6000").excluded_types, vec!["NVIDIA A40"]);
        assert_eq!(
            placed("a100-sxm4-80gb").excluded_types,
            vec!["NVIDIA A100 80GB PCIe"]
        );
        assert!(
            placed("rtx-4090").excluded_types.is_empty(),
            "sole member of its pool"
        );
        assert!(
            placed("h200-sxm-141gb").excluded_types.is_empty(),
            "sole member of its pool"
        );
    }

    #[test]
    fn new_blackwell_pools_and_members_are_known() {
        assert_eq!(
            placed("rtx-pro-6000-96gb").excluded_types,
            vec![
                "NVIDIA RTX PRO 6000 Blackwell Max-Q Workstation Edition",
                "NVIDIA RTX PRO 6000 Blackwell Server Edition"
            ]
        );
        assert_eq!(placed("rtx-pro-4500").pools, vec!["BLACKWELL_32"]);
        assert_eq!(placed("b300").pools, vec!["BLACKWELL_288"]);
    }

    fn gt(id: &str, pool: Option<&str>, memory: Option<u32>) -> GpuType {
        GpuType {
            id: id.into(),
            name: String::new(),
            pool: pool.map(Into::into),
            memory,
        }
    }

    #[test]
    fn catalog_exclusions_cover_every_other_pool_member() {
        let catalog = [
            gt("NVIDIA L40S", Some("ADA_48_PRO"), None),
            gt("NVIDIA L40", Some("ADA_48_PRO"), None),
            gt("Brand New Card", Some("ADA_48_PRO"), None),
            gt("NVIDIA A40", Some("AMPERE_48"), None),
            gt("No Pool", None, None),
        ];
        assert_eq!(
            exclusions_from_catalog("NVIDIA L40S", "ADA_48_PRO", &catalog).unwrap(),
            vec!["NVIDIA L40", "Brand New Card"]
        );
        assert!(matches!(
            exclusions_from_catalog("NVIDIA L40S", "AMPERE_48", &catalog),
            Err(RunPodError::InvalidGpu(_))
        ));
    }

    #[test]
    fn catalog_memory_must_match_the_card() {
        let catalog = [
            gt("NVIDIA H100 NVL", Some("ADA_80_PRO"), Some(94)),
            gt("NVIDIA H100 PCIe", Some("ADA_80_PRO"), Some(96)),
            gt("NVIDIA L4", Some("AMPERE_24"), None),
        ];
        assert!(verify_catalog_memory("NVIDIA H100 NVL", 94, &catalog).is_ok());
        let err = verify_catalog_memory("NVIDIA H100 PCIe", 80, &catalog).unwrap_err();
        assert!(err.to_string().contains("96 GB"), "{err}");
        // No figure to compare: not checked.
        assert!(verify_catalog_memory("NVIDIA L4", 24, &catalog).is_ok());
        assert!(verify_catalog_memory("NVIDIA B200", 180, &catalog).is_err());
    }

    #[test]
    fn count_is_carried_through() {
        let p = gpu_placement(&acc(AcceleratorKind::Gpu, "h100-sxm-80gb", 4, 80)).unwrap();
        assert_eq!(p.count, 4);
        assert_eq!(p.card_id, "h100-sxm-80gb");
        assert_eq!(p.vram_gb, 80);
    }

    #[test]
    fn unknown_or_unserved_model_is_a_typed_error() {
        assert_eq!(
            gpu_placement(&acc(AcceleratorKind::Gpu, "mi300x", 1, 192)),
            Err(RunPodError::UnknownGpu {
                model: "mi300x".into()
            })
        );
        // In the catalog, but RunPod serverless has no pool for it.
        assert_eq!(
            parse_gpu_spec("t4"),
            Err(RunPodError::UnknownGpu {
                model: "t4-16gb".into()
            })
        );
    }

    #[test]
    fn non_gpu_and_zero_count_are_rejected() {
        assert!(matches!(
            gpu_placement(&acc(AcceleratorKind::Tpu, "h100-sxm-80gb", 1, 80)),
            Err(RunPodError::InvalidGpu(_))
        ));
        assert!(matches!(
            gpu_placement(&acc(AcceleratorKind::Gpu, "h100-sxm-80gb", 0, 80)),
            Err(RunPodError::InvalidGpu(_))
        ));
    }

    #[test]
    fn parses_modal_style_specs() {
        assert_eq!(
            parse_gpu_spec("h100-sxm-80gb:2").unwrap(),
            GpuPlacement {
                pools: vec!["ADA_80_PRO".into()],
                count: 2,
                excluded_types: vec!["NVIDIA H100 PCIe".into(), "NVIDIA H100 NVL".into()],
                type_id: "NVIDIA H100 80GB HBM3".into(),
                card_id: "h100-sxm-80gb".into(),
                vram_gb: 80,
            }
        );
        assert_eq!(parse_gpu_spec("L4").unwrap().count, 1);
        // Without a memory size a family must be a single card.
        assert!(matches!(
            parse_gpu_spec("H100:1"),
            Err(RunPodError::InvalidGpu(_))
        ));
        assert!(matches!(
            parse_gpu_spec("l4:x"),
            Err(RunPodError::InvalidGpu(_))
        ));
        assert!(matches!(
            parse_gpu_spec("l4:0"),
            Err(RunPodError::InvalidGpu(_))
        ));
        assert!(matches!(
            parse_gpu_spec("potato:1"),
            Err(RunPodError::UnknownGpu { .. })
        ));
    }
}
