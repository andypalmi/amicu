//! Resolve a `(provider, region)` pair to its residency jurisdiction from the
//! built-in provider catalog.
//!
//! The DSL carries only a bare region code with no jurisdiction (see
//! [`Region::from_code`](amicu_core::resource::Region::from_code)); residency
//! lives in each provider's capability descriptor. This module builds every
//! catalogued adapter admitting **all** jurisdictions and looks the code up,
//! so a plan-time check can compare a hardware region against a `data_class`
//! residency constraint.

use amicu_core::resource::Residency;

use crate::capability::Region;
use crate::{
    AwsProvider, AzureProvider, GcpProvider, HetznerProvider, NebiusProvider, OvhCloudProvider,
    Provider, RunPodProvider, ScalewayProvider, aws, azure, gcp, hetzner, nebius, ovhcloud, runpod,
    scaleway,
};

/// Every residency jurisdiction, so catalogued providers expose all of their
/// regions (not just the EU-only default).
const ALL_RESIDENCIES: &[Residency] = &[
    Residency::Eu,
    Residency::Uk,
    Residency::Us,
    Residency::Ca,
    Residency::Apac,
];

/// The regions a catalogued provider offers across every jurisdiction, or
/// `None` when `provider` is not in the built-in catalog (e.g. `self-hosted`,
/// whose regions are user-configured).
fn provider_regions(provider: &str) -> Option<Vec<Region>> {
    let caps = match provider {
        aws::PROVIDER_ID => AwsProvider::for_residencies(ALL_RESIDENCIES).capabilities(),
        azure::PROVIDER_ID => AzureProvider::for_residencies(ALL_RESIDENCIES).capabilities(),
        gcp::PROVIDER_ID => GcpProvider::for_residencies(ALL_RESIDENCIES).capabilities(),
        hetzner::PROVIDER_ID => HetznerProvider::for_residencies(ALL_RESIDENCIES).capabilities(),
        nebius::PROVIDER_ID => NebiusProvider::for_residencies(ALL_RESIDENCIES).capabilities(),
        ovhcloud::PROVIDER_ID => OvhCloudProvider::for_residencies(ALL_RESIDENCIES).capabilities(),
        scaleway::PROVIDER_ID => ScalewayProvider::for_residencies(ALL_RESIDENCIES).capabilities(),
        runpod::PROVIDER_ID => RunPodProvider::for_residencies(ALL_RESIDENCIES).capabilities(),
        _ => return None,
    };
    Some(caps.regions)
}

/// Resolve the residency jurisdiction of `region_code` under `provider`.
///
/// Returns `None` when the provider is not catalogued or the region code is
/// unknown to it — the caller cannot then prove a residency constraint and
/// should not manufacture a jurisdiction.
#[must_use]
pub fn resolve_residency(provider: &str, region_code: &str) -> Option<Residency> {
    provider_regions(provider)?
        .into_iter()
        .find(|r| r.code == region_code)
        .map(|r| r.residency)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_known_regions_to_their_jurisdiction() {
        assert_eq!(
            resolve_residency("aws", "eu-central-1"),
            Some(Residency::Eu)
        );
        assert_eq!(resolve_residency("aws", "us-east-1"), Some(Residency::Us));
        assert_eq!(
            resolve_residency("gcp", "asia-east1"),
            Some(Residency::Apac)
        );
        assert_eq!(
            resolve_residency("nebius", "eu-north1"),
            Some(Residency::Eu)
        );
    }

    #[test]
    fn unknown_provider_or_region_is_none() {
        assert_eq!(resolve_residency("local_apple", "anywhere"), None);
        assert_eq!(resolve_residency("aws", "no-such-region"), None);
    }
}
