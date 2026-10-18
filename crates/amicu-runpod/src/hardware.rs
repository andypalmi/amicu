//! Map an amicu GPU spec onto RunPod serverless GPU pool ids, a per-worker
//! count, and the GPU types to exclude so only the requested card is eligible.
//!
//! RunPod serverless endpoints select hardware by **pool** (`ADA_24`,
//! `AMPERE_80`, ...), not by GPU type id (that is what pods use), and a pool
//! can hold several card models. To make the requested card the *only*
//! eligible one, the placement lists the pool and puts every other type of that
//! pool in `excludedTypes`. Models with no pool are a typed error rather than a
//! silent fallback, so a deployment never lands on different hardware than the
//! plan said.
//!
//! The table below is static. It should be checked against the `pool` and `id`
//! fields of `GET /v2/catalog/gpus`; a card RunPod later adds to a pool is not
//! excluded until it is added here.

use amicu_core::resource::{Accelerator, AcceleratorKind};

use crate::error::RunPodError;

/// One GPU type: normalized aliases (see [`normalize`]), its serverless pool,
/// and RunPod's GPU type id (as used in `excludedTypes`).
struct Card {
    aliases: &'static [&'static str],
    pool: &'static str,
    type_id: &'static str,
}

const fn card(aliases: &'static [&'static str], pool: &'static str, type_id: &'static str) -> Card {
    Card {
        aliases,
        pool,
        type_id,
    }
}

const CARDS: &[Card] = &[
    card(&["A4000", "RTXA4000"], "AMPERE_16", "NVIDIA RTX A4000"),
    card(&["A4500", "RTXA4500"], "AMPERE_16", "NVIDIA RTX A4500"),
    card(
        &["RTX4000ADA", "RTX4000ADAGENERATION", "RTX4000"],
        "AMPERE_16",
        "NVIDIA RTX 4000 Ada Generation",
    ),
    card(
        &["RTX2000ADA", "RTX2000ADAGENERATION", "RTX2000"],
        "AMPERE_16",
        "NVIDIA RTX 2000 Ada Generation",
    ),
    card(&["L4"], "AMPERE_24", "NVIDIA L4"),
    card(&["A5000", "RTXA5000"], "AMPERE_24", "NVIDIA RTX A5000"),
    card(&["RTX3090", "3090"], "AMPERE_24", "NVIDIA GeForce RTX 3090"),
    card(&["RTX4090", "4090"], "ADA_24", "NVIDIA GeForce RTX 4090"),
    card(
        &["RTX5090", "5090"],
        "ADA_32_PRO",
        "NVIDIA GeForce RTX 5090",
    ),
    card(&["A40"], "AMPERE_48", "NVIDIA A40"),
    card(&["A6000", "RTXA6000"], "AMPERE_48", "NVIDIA RTX A6000"),
    card(&["L40"], "ADA_48_PRO", "NVIDIA L40"),
    card(&["L40S"], "ADA_48_PRO", "NVIDIA L40S"),
    card(
        &["RTX6000ADA", "RTX6000ADAGENERATION", "6000ADA"],
        "ADA_48_PRO",
        "NVIDIA RTX 6000 Ada Generation",
    ),
    // A bare "A100" / "H100" means the SXM part, the common one.
    card(
        &["A100", "A10080GB", "A100SXM", "A100SXM480GB"],
        "AMPERE_80",
        "NVIDIA A100-SXM4-80GB",
    ),
    card(
        &["A100PCIE", "A10080GBPCIE"],
        "AMPERE_80",
        "NVIDIA A100 80GB PCIe",
    ),
    card(
        &["H100", "H100SXM", "H10080GBHBM3"],
        "ADA_80_PRO",
        "NVIDIA H100 80GB HBM3",
    ),
    card(&["H100PCIE"], "ADA_80_PRO", "NVIDIA H100 PCIe"),
    card(&["H100NVL"], "ADA_80_PRO", "NVIDIA H100 NVL"),
    card(&["H200", "H200SXM"], "HOPPER_141", "NVIDIA H200"),
    card(
        &[
            "RTXPRO6000",
            "PRO6000",
            "RTXPRO6000BLACKWELLWORKSTATIONEDITION",
        ],
        "BLACKWELL_96",
        "NVIDIA RTX PRO 6000 Blackwell Workstation Edition",
    ),
    card(&["B200"], "BLACKWELL_180", "NVIDIA B200"),
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
}

/// Upper-case, keep only `[A-Z0-9]`, drop `NVIDIA` / `GEFORCE`.
fn normalize(model: &str) -> String {
    let upper = model.to_ascii_uppercase();
    let mut s: String = upper.chars().filter(char::is_ascii_alphanumeric).collect();
    for noise in ["NVIDIA", "GEFORCE"] {
        s = s.replace(noise, "");
    }
    s
}

fn card_for(model: &str) -> Result<&'static Card, RunPodError> {
    let key = normalize(model);
    CARDS
        .iter()
        .find(|c| c.aliases.contains(&key.as_str()))
        .ok_or_else(|| RunPodError::UnknownGpu {
            model: model.to_owned(),
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
    placement(&accelerator.model, accelerator.count)
}

/// Map a Modal-style spec string (`"H100"`, `"H100:2"`) to a [`GpuPlacement`].
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
    placement(model.trim(), count)
}

fn placement(model: &str, count: u32) -> Result<GpuPlacement, RunPodError> {
    if count == 0 {
        return Err(RunPodError::InvalidGpu(
            "GPU count must be at least 1".into(),
        ));
    }
    let card = card_for(model)?;
    Ok(GpuPlacement {
        pools: vec![card.pool.to_owned()],
        count,
        excluded_types: CARDS
            .iter()
            .filter(|c| c.pool == card.pool && c.type_id != card.type_id)
            .map(|c| c.type_id.to_owned())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::units::ByteSize;

    fn acc(kind: AcceleratorKind, model: &str, count: u32) -> Accelerator {
        Accelerator {
            kind,
            model: model.to_owned(),
            count,
            memory: ByteSize::from_bytes(0),
            memory_bandwidth_gbps: None,
        }
    }

    fn placed(model: &str) -> GpuPlacement {
        gpu_placement(&acc(AcceleratorKind::Gpu, model, 1)).unwrap()
    }

    #[test]
    fn maps_models_to_pools() {
        for (model, pool) in [
            ("h100", "ADA_80_PRO"),
            ("H100", "ADA_80_PRO"),
            ("a100", "AMPERE_80"),
            ("l40s", "ADA_48_PRO"),
            ("rtx-a6000", "AMPERE_48"),
            ("rtx-4090", "ADA_24"),
            ("NVIDIA GeForce RTX 4090", "ADA_24"),
            ("rtx_3090", "AMPERE_24"),
            ("rtx-a4000", "AMPERE_16"),
            ("rtx-a4500", "AMPERE_16"),
            ("rtx-a5000", "AMPERE_24"),
            ("rtx-5090", "ADA_32_PRO"),
            ("h200", "HOPPER_141"),
            ("b200", "BLACKWELL_180"),
            // RunPod's own GPU type names.
            ("NVIDIA RTX 4000 Ada Generation", "AMPERE_16"),
            ("NVIDIA RTX 6000 Ada Generation", "ADA_48_PRO"),
            ("NVIDIA H100 80GB HBM3", "ADA_80_PRO"),
            ("NVIDIA H100 PCIe", "ADA_80_PRO"),
            ("NVIDIA H100 NVL", "ADA_80_PRO"),
            ("NVIDIA A100-SXM4-80GB", "AMPERE_80"),
            ("NVIDIA A100 80GB PCIe", "AMPERE_80"),
            ("NVIDIA GeForce RTX 5090", "ADA_32_PRO"),
            ("NVIDIA RTX A6000", "AMPERE_48"),
        ] {
            let p = placed(model);
            assert_eq!(p.pools, vec![pool.to_owned()], "{model}");
            assert_eq!(p.count, 1);
        }
    }

    #[test]
    fn only_the_requested_card_stays_eligible() {
        assert_eq!(
            placed("l40s").excluded_types,
            vec!["NVIDIA L40", "NVIDIA RTX 6000 Ada Generation"]
        );
        assert_eq!(
            placed("h100").excluded_types,
            vec!["NVIDIA H100 PCIe", "NVIDIA H100 NVL"]
        );
        assert_eq!(
            placed("h100-pcie").excluded_types,
            vec!["NVIDIA H100 80GB HBM3", "NVIDIA H100 NVL"]
        );
        assert_eq!(
            placed("rtx_3090").excluded_types,
            vec!["NVIDIA L4", "NVIDIA RTX A5000"]
        );
        assert_eq!(placed("rtx-a6000").excluded_types, vec!["NVIDIA A40"]);
        assert_eq!(placed("a100").excluded_types, vec!["NVIDIA A100 80GB PCIe"]);
        assert!(
            placed("rtx-4090").excluded_types.is_empty(),
            "sole member of its pool"
        );
        assert!(
            placed("h200").excluded_types.is_empty(),
            "sole member of its pool"
        );
    }

    #[test]
    fn table_has_no_duplicate_aliases_or_type_ids() {
        let mut aliases = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for c in CARDS {
            assert!(ids.insert(c.type_id), "{}", c.type_id);
            for a in c.aliases {
                assert!(aliases.insert(*a), "{a}");
                assert_eq!(normalize(a), *a, "alias must be pre-normalized");
            }
        }
    }

    #[test]
    fn count_is_carried_through() {
        let p = gpu_placement(&acc(AcceleratorKind::Gpu, "h100", 4)).unwrap();
        assert_eq!(p.count, 4);
    }

    #[test]
    fn unknown_model_is_a_typed_error() {
        assert_eq!(
            gpu_placement(&acc(AcceleratorKind::Gpu, "mi300x", 1)),
            Err(RunPodError::UnknownGpu {
                model: "mi300x".into()
            })
        );
    }

    #[test]
    fn non_gpu_and_zero_count_are_rejected() {
        assert!(matches!(
            gpu_placement(&acc(AcceleratorKind::Tpu, "h100", 1)),
            Err(RunPodError::InvalidGpu(_))
        ));
        assert!(matches!(
            gpu_placement(&acc(AcceleratorKind::Gpu, "h100", 0)),
            Err(RunPodError::InvalidGpu(_))
        ));
    }

    #[test]
    fn parses_modal_style_specs() {
        assert_eq!(
            parse_gpu_spec("H100:2").unwrap(),
            GpuPlacement {
                pools: vec!["ADA_80_PRO".into()],
                count: 2,
                excluded_types: vec!["NVIDIA H100 PCIe".into(), "NVIDIA H100 NVL".into()]
            }
        );
        assert_eq!(parse_gpu_spec("A100").unwrap().count, 1);
        assert!(matches!(
            parse_gpu_spec("H100:x"),
            Err(RunPodError::InvalidGpu(_))
        ));
        assert!(matches!(
            parse_gpu_spec("H100:0"),
            Err(RunPodError::InvalidGpu(_))
        ));
        assert!(matches!(
            parse_gpu_spec("potato:1"),
            Err(RunPodError::UnknownGpu { .. })
        ));
    }
}
