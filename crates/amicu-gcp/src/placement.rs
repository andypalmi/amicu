//! Where and on what a GPU deployment runs: the machine type and accelerator
//! for a GPU, and the ordered zones to try. Pure catalog lookups, shared by
//! the config checks and the supervisor so both agree.

use crate::catalog::{GcpCatalog, Provisioning, resolve_model};
use crate::compute::Accelerator;

/// Why a GPU, machine type, provisioning mode and region do not fit together.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlacementError {
    #[error("`{0}` is not a GPU in the Google Cloud catalog")]
    UnknownGpu(String),
    #[error("machine type `{machine}` is not in the Google Cloud catalog or an N1 shape")]
    UnknownMachine { machine: String },
    #[error("machine type `{machine}` carries {has}, not {gpu}")]
    GpuMismatch {
        machine: String,
        has: String,
        gpu: String,
    },
    #[error("{count} x {gpu} is not an attachment amicu knows for `{machine}`")]
    BadCount {
        machine: String,
        gpu: String,
        count: u32,
    },
    #[error("`{machine}` cannot be obtained as {provisioning}; it offers {offered}")]
    ProvisioningUnsupported {
        machine: String,
        provisioning: &'static str,
        offered: String,
    },
    #[error("no machine type with {count} x {gpu} can be obtained as {provisioning}")]
    NoMachine {
        gpu: String,
        count: u32,
        provisioning: &'static str,
    },
    #[error("no zone {scope} offers {what}")]
    NoZone { scope: String, what: String },
}

/// The machine type and the accelerator to attach (N1 only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub machine_type: String,
    pub accelerator: Option<Accelerator>,
}

fn label(p: Provisioning) -> &'static str {
    p.as_str()
}

/// The machine type for `count` x `gpu` obtained as `provisioning`: `machine`
/// when given (checked), else the catalog's smallest one, or for N1-only GPUs
/// an N1 standard shape of 4 vCPUs per GPU.
///
/// # Errors
/// [`PlacementError`] when the pieces do not fit together.
pub fn resolve_shape(
    gpu: &str,
    count: u32,
    machine: Option<&str>,
    provisioning: Provisioning,
) -> Result<Shape, PlacementError> {
    let model = resolve_model(gpu).ok_or_else(|| PlacementError::UnknownGpu(gpu.to_owned()))?;
    let catalog = GcpCatalog::new();
    let n1 = |name: &str| -> Result<Shape, PlacementError> {
        let rules = catalog
            .n1_gpu(gpu)
            .ok_or_else(|| PlacementError::NoMachine {
                gpu: model.name.to_owned(),
                count,
                provisioning: label(provisioning),
            })?;
        let bad_count = || PlacementError::BadCount {
            machine: name.to_owned(),
            gpu: model.name.to_owned(),
            count,
        };
        let limit = rules
            .limits
            .iter()
            .find(|l| l.gpus == count)
            .ok_or_else(bad_count)?;
        if !rules.consumption.contains(&provisioning) {
            return Err(PlacementError::ProvisioningUnsupported {
                machine: name.to_owned(),
                provisioning: label(provisioning),
                offered: offered(rules.consumption),
            });
        }
        let vcpus: u32 = name
            .rsplit('-')
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(bad_count)?;
        if vcpus > limit.max_vcpus {
            return Err(bad_count());
        }
        Ok(Shape {
            machine_type: name.to_owned(),
            accelerator: Some(Accelerator {
                type_name: model.name.to_owned(),
                count,
            }),
        })
    };
    let Some(name) = machine else {
        if let Some(m) = catalog
            .machine_types_for_gpu(gpu, count)
            .into_iter()
            .find(|m| m.supports(provisioning))
        {
            return Ok(Shape {
                machine_type: m.name.to_owned(),
                accelerator: None,
            });
        }
        let name = format!("n1-standard-{}", 4 * count);
        return n1(&name).map_err(|_| PlacementError::NoMachine {
            gpu: model.name.to_owned(),
            count,
            provisioning: label(provisioning),
        });
    };
    let Some(m) = catalog.machine_type(name) else {
        return if name.starts_with("n1-") {
            n1(name)
        } else {
            Err(PlacementError::UnknownMachine {
                machine: name.to_owned(),
            })
        };
    };
    if m.gpu != model.name {
        return Err(PlacementError::GpuMismatch {
            machine: name.to_owned(),
            has: m.gpu.to_owned(),
            gpu: model.name.to_owned(),
        });
    }
    if m.gpu_count() != Some(count) {
        return Err(PlacementError::BadCount {
            machine: name.to_owned(),
            gpu: model.name.to_owned(),
            count,
        });
    }
    if !m.supports(provisioning) {
        return Err(PlacementError::ProvisioningUnsupported {
            machine: name.to_owned(),
            provisioning: label(provisioning),
            offered: offered(m.consumption),
        });
    }
    Ok(Shape {
        machine_type: name.to_owned(),
        accelerator: None,
    })
}

fn offered(options: &[Provisioning]) -> String {
    options
        .iter()
        .map(|p| label(*p))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The zones to try, in catalog order: `zone` alone when pinned (it must offer
/// the GPU), else the zones of `region` that offer it. AI zones need
/// per-project access and are only used when named; zones where the machine
/// type needs account-team approval are skipped.
///
/// # Errors
/// [`PlacementError::NoZone`] when none qualifies.
pub fn candidate_zones(
    gpu: &str,
    machine: Option<&str>,
    region: Option<&str>,
    zone: Option<&str>,
) -> Result<Vec<&'static str>, PlacementError> {
    let model = resolve_model(gpu).ok_or_else(|| PlacementError::UnknownGpu(gpu.to_owned()))?;
    let catalog = GcpCatalog::new();
    let fixed = machine.and_then(|m| catalog.machine_type(m));
    let mut out = Vec::new();
    for z in catalog.zones_for_gpu(gpu) {
        let wanted = match (zone, region) {
            (Some(name), _) => z.name == name,
            (None, Some(r)) => z.region == r && !z.ai_zone,
            (None, None) => false,
        };
        let offers =
            fixed.is_none_or(|m| z.offers_machine_type(m) && (zone.is_some() || !z.is_limited(m)));
        if wanted && offers {
            out.push(z.name);
        }
    }
    if out.is_empty() {
        let scope = match (zone, region) {
            (Some(z), _) => format!("named `{z}`"),
            (None, Some(r)) => format!("in region `{r}`"),
            (None, None) => "without a `region` or `zone`".to_owned(),
        };
        return Err(PlacementError::NoZone {
            scope,
            what: model.name.to_owned(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t4_spot_is_an_n1_shape_with_an_attached_accelerator() {
        let s = resolve_shape("t4", 1, Some("n1-standard-4"), Provisioning::Spot).unwrap();
        assert_eq!(s.machine_type, "n1-standard-4");
        let a = s.accelerator.unwrap();
        assert_eq!((a.type_name.as_str(), a.count), ("nvidia-tesla-t4", 1));
        let d = resolve_shape("t4", 1, None, Provisioning::Spot).unwrap();
        assert_eq!(d.machine_type, "n1-standard-4");
    }

    #[test]
    fn standard_is_refused_on_a3_highgpu_1g() {
        let err = resolve_shape("h100", 1, Some("a3-highgpu-1g"), Provisioning::OnDemand);
        assert!(
            matches!(err, Err(PlacementError::ProvisioningUnsupported { .. })),
            "{err:?}"
        );
    }

    #[test]
    fn a_default_machine_follows_the_catalog_and_mismatches_are_named() {
        let s = resolve_shape("l4", 1, None, Provisioning::Spot).unwrap();
        assert!(s.machine_type.starts_with("g2-standard"), "{s:?}");
        assert!(s.accelerator.is_none());
        assert!(matches!(
            resolve_shape("l4", 1, Some("a3-highgpu-1g"), Provisioning::Spot),
            Err(PlacementError::GpuMismatch { .. })
        ));
        assert!(matches!(
            resolve_shape("nope", 1, None, Provisioning::Spot),
            Err(PlacementError::UnknownGpu(_))
        ));
    }

    #[test]
    fn zones_follow_the_region_and_the_gpu() {
        let zones = candidate_zones("t4", None, Some("europe-west4"), None).unwrap();
        assert!(
            zones.iter().all(|z| z.starts_with("europe-west4-")),
            "{zones:?}"
        );
        assert!(candidate_zones("t4", None, None, None).is_err());
        assert_eq!(
            candidate_zones("t4", None, None, Some(zones[0])).unwrap(),
            vec![zones[0]]
        );
    }
}
