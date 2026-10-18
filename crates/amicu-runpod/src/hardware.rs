//! Map an amicu GPU spec onto RunPod serverless GPU pool ids and a per-worker
//! count.
//!
//! RunPod serverless endpoints select hardware by **pool** (`ADA_24`,
//! `AMPERE_80`, ...), not by GPU type id (that is what pods use). A pool can
//! hold several card models; the pool chosen for a model is the one that
//! contains it. Models with no pool are a typed error rather than a silent
//! fallback, so a deployment never lands on different hardware than the plan
//! said.

use amicu_core::resource::{Accelerator, AcceleratorKind};

use crate::error::RunPodError;

/// Normalized model aliases -> serverless pool id. Aliases are matched after
/// [`normalize`] (upper-case, only `[A-Z0-9]`, `NVIDIA`/`GEFORCE` dropped).
const POOLS: &[(&[&str], &str)] = &[
    (&["A4000", "A4500", "RTX4000", "RTX2000"], "AMPERE_16"),
    (&["L4", "A5000", "RTX3090"], "AMPERE_24"),
    (&["RTX4090", "4090"], "ADA_24"),
    (&["A40", "A6000", "RTXA6000"], "AMPERE_48"),
    (&["L40", "L40S", "RTX6000ADA", "6000ADA"], "ADA_48_PRO"),
    (&["A100", "A10080GB", "A100SXM", "A100PCIE"], "AMPERE_80"),
    (&["H100", "H100SXM", "H100PCIE", "H100NVL"], "ADA_80_PRO"),
    (&["H200"], "HOPPER_141"),
    (&["RTXPRO6000", "PRO6000"], "BLACKWELL_96"),
    (&["B200"], "BLACKWELL_180"),
];

/// Where a deployment's GPUs go: the pool ids to list under `gpu.pools` and the
/// GPUs per worker for `gpu.count`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuPlacement {
    /// Serverless GPU pool ids.
    pub pools: Vec<String>,
    /// GPUs per worker.
    pub count: u32,
}

fn normalize(model: &str) -> String {
    let upper = model.to_ascii_uppercase();
    let mut s: String = upper.chars().filter(char::is_ascii_alphanumeric).collect();
    for noise in ["NVIDIA", "GEFORCE"] {
        s = s.replace(noise, "");
    }
    s
}

/// The pool id for a GPU model name such as `"h100"` or `"rtx-4090"`.
fn pool_for(model: &str) -> Result<&'static str, RunPodError> {
    let key = normalize(model);
    POOLS
        .iter()
        .find(|(aliases, _)| aliases.contains(&key.as_str()))
        .map(|(_, pool)| *pool)
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
    Ok(GpuPlacement {
        pools: vec![pool_for(model)?.to_owned()],
        count,
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
            ("h200", "HOPPER_141"),
            ("b200", "BLACKWELL_180"),
        ] {
            let p = gpu_placement(&acc(AcceleratorKind::Gpu, model, 1)).unwrap();
            assert_eq!(p.pools, vec![pool.to_owned()], "{model}");
            assert_eq!(p.count, 1);
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
                count: 2
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
