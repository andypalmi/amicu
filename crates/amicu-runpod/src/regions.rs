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
    format!("{r:?}").to_ascii_lowercase()
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
    Ok(ids.into_iter().map(str::to_owned).collect())
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
        for region in ["us", "US-CA-1", "apac"] {
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
        assert!(ids.contains(&"US-CA-1".to_owned()));
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
