//! Residency-aware data-center selection, from the same table the RunPod pod
//! provider in `amicu-providers` filters on.

use amicu_core::resource::Residency;
use amicu_providers::RunPodProvider;

use crate::error::RunPodError;

const GROUPS: &[(&str, Residency)] = &[
    ("eu", Residency::Eu),
    ("us", Residency::Us),
    ("ca", Residency::Ca),
    ("apac", Residency::Apac),
];

fn residency_of_code(code: &str) -> Option<(Residency, &'static str)> {
    GROUPS.iter().find_map(|(_, r)| {
        RunPodProvider::datacenter_codes(r)
            .into_iter()
            .find(|c| c.eq_ignore_ascii_case(code))
            .map(|c| (r.clone(), c))
    })
}

fn residency_label(r: &Residency) -> String {
    r.as_token().to_owned()
}

/// The `dataCenterIds` for `region`, enforcing `required` residency if given.
///
/// `region` is either a region group (`"eu"`, `"us"`, `"ca"`, `"apac"`) or an
/// exact RunPod data-center id (`"EU-RO-1"`, case-insensitive). A region
/// outside `required` is rejected rather than silently ignored; an unknown
/// region is rejected rather than widened to "anywhere".
pub fn select_data_centers(
    region: &str,
    required: Option<&Residency>,
) -> Result<Vec<String>, RunPodError> {
    let wanted = region.trim();
    let (residency, ids): (Residency, Vec<&'static str>) = if let Some((_, r)) = GROUPS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
    {
        (r.clone(), RunPodProvider::datacenter_codes(r))
    } else if let Some((r, code)) = residency_of_code(wanted) {
        (r, vec![code])
    } else {
        return Err(RunPodError::UnknownRegion {
            region: region.to_owned(),
        });
    };
    if let Some(required) = required
        && &residency != required
    {
        return Err(RunPodError::ResidencyViolation {
            region: region.to_owned(),
            required: residency_label(required),
        });
    }
    if ids.is_empty() {
        // Empty `dataCenterIds` would mean "anywhere".
        return Err(RunPodError::UnknownRegion {
            region: region.to_owned(),
        });
    }
    Ok(ids.into_iter().map(str::to_owned).collect())
}

/// Narrows `selected` (from [`select_data_centers`]; empty means anywhere)
/// to the `allowed` residencies, e.g. a project's `data_class` allowlist.
/// An empty `allowed` changes nothing; with no region the result is every
/// data center of the allowed residencies; a region outside them is rejected.
pub fn restrict_to_residencies(
    region: Option<&str>,
    selected: Vec<String>,
    allowed: &[Residency],
) -> Result<Vec<String>, RunPodError> {
    if allowed.is_empty() {
        return Ok(selected);
    }
    let violation = || RunPodError::ResidencyViolation {
        region: region.unwrap_or("any RunPod data center").to_owned(),
        required: allowed
            .iter()
            .map(residency_label)
            .collect::<Vec<_>>()
            .join(" or "),
    };
    if selected.is_empty() {
        let ids: Vec<String> = allowed
            .iter()
            .flat_map(RunPodProvider::datacenter_codes)
            .map(str::to_owned)
            .collect();
        // Empty `dataCenterIds` would mean "anywhere".
        return if ids.is_empty() {
            Err(violation())
        } else {
            Ok(ids)
        };
    }
    let inside = |id: &String| residency_of_code(id).is_some_and(|(r, _)| allowed.contains(&r));
    if selected.iter().all(inside) {
        return Ok(selected);
    }
    Err(violation())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eu_region_yields_only_eu_data_centers() {
        let ids = select_data_centers("eu", Some(&Residency::Eu)).unwrap();
        assert!(ids.len() > 1);
        assert!(ids.contains(&"EU-RO-1".to_owned()));
        assert!(ids.iter().all(|id| id.starts_with("EU")), "{ids:?}");
        assert!(!ids.iter().any(|id| id.starts_with("US")));
    }

    #[test]
    fn exact_data_center_is_case_insensitive() {
        assert_eq!(
            select_data_centers("eu-se-1", Some(&Residency::Eu)).unwrap(),
            vec!["EU-SE-1".to_owned()]
        );
    }

    #[test]
    fn non_eu_region_with_eu_residency_is_rejected() {
        for region in ["us", "US-CA-2", "apac"] {
            assert_eq!(
                select_data_centers(region, Some(&Residency::Eu)),
                Err(RunPodError::ResidencyViolation {
                    region: region.into(),
                    required: "eu".into()
                }),
                "{region}"
            );
        }
    }

    #[test]
    fn without_a_residency_requirement_any_known_region_is_allowed() {
        let ids = select_data_centers("us", None).unwrap();
        assert!(ids.contains(&"US-CA-2".to_owned()));
    }

    #[test]
    fn a_residency_allowlist_places_or_rejects() {
        let eu = select_data_centers("eu", None).unwrap();
        assert_eq!(
            restrict_to_residencies(Some("eu"), eu.clone(), &[]).unwrap(),
            eu
        );
        assert_eq!(
            restrict_to_residencies(Some("eu"), eu.clone(), &[Residency::Eu]).unwrap(),
            eu
        );
        assert_eq!(
            restrict_to_residencies(None, Vec::new(), &[Residency::Eu]).unwrap(),
            eu
        );
        assert_eq!(
            restrict_to_residencies(Some("US-CA-2"), vec!["US-CA-2".into()], &[Residency::Eu]),
            Err(RunPodError::ResidencyViolation {
                region: "US-CA-2".into(),
                required: "eu".into()
            })
        );
    }

    #[test]
    fn an_allowlist_with_no_runpod_data_center_is_refused_not_widened() {
        for allowed in [vec![Residency::Uk], vec![Residency::Other("latam".into())]] {
            let err = restrict_to_residencies(None, Vec::new(), &allowed).unwrap_err();
            assert!(
                matches!(err, RunPodError::ResidencyViolation { .. }),
                "{err:?}"
            );
        }
        assert!(matches!(
            restrict_to_residencies(Some("eu"), vec!["EU-RO-1".into()], &[Residency::Uk]),
            Err(RunPodError::ResidencyViolation { .. })
        ));
    }

    #[test]
    fn eu_includes_france_iceland_and_norway() {
        let ids = select_data_centers("eu", None).unwrap();
        for dc in ["EU-FR-1", "EUR-IS-1", "EUR-IS-5", "EUR-NO-2"] {
            assert!(ids.contains(&dc.to_owned()), "{dc}: {ids:?}");
        }
    }

    #[test]
    fn unknown_region_is_rejected() {
        assert_eq!(
            select_data_centers("mars-1", None),
            Err(RunPodError::UnknownRegion {
                region: "mars-1".into()
            })
        );
    }
}
