//! Map an amicu GPU spec onto RunPod serverless GPU pool ids, a per-worker
//! count, and the GPU types to exclude so only the requested card is eligible.
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
//! still excluded, and the live memory must match the card
//! ([`verify_catalog_memory`]).

use amicu_core::ByteSize;
use amicu_core::resource::{
    Accelerator, AcceleratorKind, GpuResolveError, resolve_gpu, vram_matches,
};

use crate::client::GpuType;
use crate::error::RunPodError;

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
    /// GPU catalog id (`"l40s-48gb"`).
    pub card_id: String,
    /// Nominal VRAM in GB.
    pub vram_gb: u32,
}

/// Cards RunPod offers only as pods (no serverless pool): catalog id to
/// RunPod GPU type id.
// TODO: MI300X, RTX PRO 4000/5000 and V100 once the shared catalog has them.
const POD_ONLY_CARDS: &[(&str, &str)] = &[("h200-nvl-141gb", "NVIDIA H200 NVL")];

fn resolve(model: &str, memory: Option<ByteSize>) -> Result<(&'static str, u32), RunPodError> {
    let resolved = resolve_gpu(model, memory).map_err(|e| match e {
        GpuResolveError::Unknown { .. } => RunPodError::UnknownGpu {
            model: model.to_owned(),
        },
        other => RunPodError::InvalidGpu(other.to_string()),
    })?;
    Ok((resolved.id, resolved.vram_gb))
}

fn card_for(model: &str, memory: Option<ByteSize>) -> Result<(&'static Card, u32), RunPodError> {
    let (id, vram_gb) = resolve(model, memory)?;
    if let Some(card) = CARDS.iter().find(|c| c.id == id) {
        return Ok((card, vram_gb));
    }
    if POD_ONLY_CARDS.iter().any(|(c, _)| *c == id) {
        return Err(RunPodError::InvalidGpu(format!(
            "`{id}` has no RunPod serverless pool; it is only available as a pod (mode = \"pod\")"
        )));
    }
    Err(RunPodError::UnknownGpu {
        model: id.to_owned(),
    })
}

/// Where a pod's GPUs go: an exact RunPod GPU type id and a count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodGpuPlacement {
    /// RunPod's GPU type id (`"NVIDIA H100 80GB HBM3"`).
    pub type_id: String,
    /// GPUs on the pod.
    pub count: u32,
    /// GPU catalog id (`"h100-sxm-80gb"`).
    pub card_id: String,
    /// Nominal VRAM in GB.
    pub vram_gb: u32,
}

/// Map an amicu [`Accelerator`] to a pod's GPU type. Unlike
/// [`gpu_placement`], pods-only cards are allowed.
pub fn pod_gpu_placement(accelerator: &Accelerator) -> Result<PodGpuPlacement, RunPodError> {
    if accelerator.kind != AcceleratorKind::Gpu {
        return Err(RunPodError::InvalidGpu(format!(
            "RunPod pods are GPU-only here; {:?} is not supported",
            accelerator.kind
        )));
    }
    if accelerator.count == 0 {
        return Err(RunPodError::InvalidGpu(
            "GPU count must be at least 1".into(),
        ));
    }
    let (id, vram_gb) = resolve(&accelerator.model, Some(accelerator.memory))?;
    let type_id = CARDS
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.type_id)
        .or_else(|| {
            POD_ONLY_CARDS
                .iter()
                .find(|(c, _)| *c == id)
                .map(|(_, t)| *t)
        })
        .ok_or_else(|| RunPodError::UnknownGpu {
            model: id.to_owned(),
        })?;
    Ok(PodGpuPlacement {
        type_id: type_id.to_owned(),
        count: accelerator.count,
        card_id: id.to_owned(),
        vram_gb,
    })
}

/// Map an amicu [`Accelerator`] to a RunPod [`GpuPlacement`].
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

/// Catches a RunPod type id that came to mean a different variant. An entry
/// without a memory figure is not checked.
pub fn verify_catalog_memory(
    type_id: &str,
    vram_gb: u32,
    catalog: &[GpuType],
) -> Result<(), RunPodError> {
    let entry = catalog.iter().find(|g| g.id == type_id).ok_or_else(|| {
        RunPodError::InvalidGpu(format!("RunPod's GPU catalog does not list `{type_id}`"))
    })?;
    match entry.memory {
        Some(memory)
            if !vram_matches(
                vram_gb,
                ByteSize::from_bytes(u64::from(memory) * 1_000_000_000),
            ) =>
        {
            Err(RunPodError::InvalidGpu(format!(
                "RunPod's GPU catalog lists `{type_id}` with {memory} GB, but the requested card \
             has {vram_gb} GB"
            )))
        }
        _ => Ok(()),
    }
}

/// Map a Modal-style spec string (`"h100-sxm-80gb:2"`) to a [`GpuPlacement`].
/// Without a memory size, the model must name a single card on its own.
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
    memory: Option<ByteSize>,
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
            // `NVIDIA H200` is a family name (SXM and NVL).
            let vram = gpu_card(c.id).unwrap().memory();
            match resolve_gpu(c.type_id, Some(vram)) {
                Ok(card) => assert_eq!(card.id, c.id),
                Err(GpuResolveError::Ambiguous { candidates, .. }) => {
                    assert!(candidates.iter().any(|k| k.id == c.id), "{}", c.type_id);
                }
                Err(e) => panic!("{}: {e}", c.type_id),
            }
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
            ("h200-sxm", 141, "HOPPER_141"),
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
        let err = gpu_placement(&acc(AcceleratorKind::Gpu, "a100", 1, 80)).unwrap_err();
        assert!(
            matches!(err, RunPodError::InvalidGpu(ref m) if m.contains("ambiguous")),
            "{err}"
        );
        assert!(matches!(
            gpu_placement(&acc(AcceleratorKind::Gpu, "h100", 1, 80)),
            Err(RunPodError::InvalidGpu(_))
        ));
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
        // RunPod's 143 GB for a 141 GB H200 NVL is within tolerance.
        let h200 = [gt("NVIDIA H200 NVL", None, Some(143))];
        assert!(verify_catalog_memory("NVIDIA H200 NVL", 141, &h200).is_ok());
        let err = verify_catalog_memory("NVIDIA H100 PCIe", 80, &catalog).unwrap_err();
        assert!(err.to_string().contains("96 GB"), "{err}");
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

    #[test]
    fn pods_take_an_exact_type_id_and_allow_pods_only_cards() {
        let p = pod_gpu_placement(&acc(AcceleratorKind::Gpu, "h100-sxm-80gb", 2, 80)).unwrap();
        assert_eq!(
            (p.type_id.as_str(), p.count, p.card_id.as_str()),
            ("NVIDIA H100 80GB HBM3", 2, "h100-sxm-80gb")
        );
        let nvl = pod_gpu_placement(&acc(AcceleratorKind::Gpu, "NVIDIA H200 NVL", 1, 141)).unwrap();
        assert_eq!(nvl.type_id, "NVIDIA H200 NVL");

        let err = parse_gpu_spec("h200-nvl-141gb").unwrap_err();
        assert!(err.to_string().contains("mode = \"pod\""), "{err}");

        assert!(pod_gpu_placement(&acc(AcceleratorKind::Gpu, "h100-sxm-80gb", 0, 80)).is_err());
        assert!(pod_gpu_placement(&acc(AcceleratorKind::Tpu, "v5e", 1, 16)).is_err());
    }
}
