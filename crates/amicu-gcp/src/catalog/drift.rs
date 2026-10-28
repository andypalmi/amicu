//! Drift check: compares the catalog with what a project's Compute Engine
//! API lists, so a stale table shows up before a deploy fails on it.
//!
//! [`fetch_live_state`] reads `aggregated/acceleratorTypes` and
//! `aggregated/machineTypes` (the latter filtered to the GPU series) and
//! [`compare`] diffs them against the catalog. Both only read. The
//! `verify-catalog` example runs them against a real project.
//!
//! The API lists only what the queried project can see. Zones gated to
//! specific projects (and AI zones such as `europe-west4-ai1a`) show up as
//! MISSING even though the catalog is right. AI-zone entries are therefore
//! reported separately in [`Diff::hidden`] and never count as drift; a
//! missing entry in any other zone may be project gating too, so run the
//! check from a project known to have access.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use amicu_providers::HttpMethod;
use serde_json::Value;

use super::{GcpCatalog, MACHINE_TYPES, ZONES, resolve_model};
use crate::client::{COMPUTE_BASE_URL, GcpClient};
use crate::error::GcpError;
use crate::validate::validate_project_id;

/// Machine type names of the GPU series, as a Compute Engine list filter.
const MACHINE_TYPE_FILTER: &str = r#"name eq "(a4x|a4|a3|a2|g4|g2)-.*""#;

/// N1 GPUs have no machine type of their own, so machine types never show them.
const N1_ONLY: [&str; 3] = ["nvidia-tesla-t4", "nvidia-tesla-p4", "nvidia-tesla-v100"];

/// `a3-edgegpu-8g-nolssd` and `a3-ultragpu-8g-nolssd` are the same shapes as
/// their base machine types minus the local SSD, so the catalog does not list them.
const NO_LSSD_SUFFIX: &str = "-nolssd";

/// A `(zone, name)` pair, where the name is a GPU or a machine type.
pub type Pair = (String, String);

/// What the API lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveState {
    /// Zone and accelerator type, from `acceleratorTypes`.
    pub accelerators: BTreeSet<Pair>,
    /// Zone and GPU machine type, from `machineTypes`.
    pub machine_types: BTreeSet<Pair>,
    /// Zone and accelerator type, inferred from `machineTypes`.
    pub machine_type_gpus: BTreeSet<Pair>,
}

/// The difference between what the catalog expects and what is live.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub expected: usize,
    pub live: usize,
    /// In the catalog, not visible in the API. Excludes [`Self::hidden`].
    pub missing: BTreeSet<Pair>,
    /// In the catalog but not visible in the API, in an AI zone
    /// ([`super::Zone::ai_zone`]) that the API may hide. Not drift.
    pub hidden: BTreeSet<Pair>,
    /// In the API, not in the catalog.
    pub extra: BTreeSet<Pair>,
}

impl Diff {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftReport {
    pub zone_gpus: Diff,
    pub zone_gpus_from_machine_types: Diff,
    pub zone_machine_types: Diff,
}

impl DriftReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.zone_gpus.is_empty()
            && self.zone_gpus_from_machine_types.is_empty()
            && self.zone_machine_types.is_empty()
    }
}

fn pairs(items: impl IntoIterator<Item = (&'static str, &'static str)>) -> BTreeSet<Pair> {
    items
        .into_iter()
        .map(|(z, n)| (z.to_owned(), n.to_owned()))
        .collect()
}

/// Zone and GPU pairs the catalog lists.
#[must_use]
pub fn expected_zone_gpus() -> BTreeSet<Pair> {
    pairs(
        ZONES
            .iter()
            .flat_map(|z| z.gpus.iter().map(move |g| (z.name, *g))),
    )
}

/// Zone and machine type pairs the catalog lists.
#[must_use]
pub fn expected_zone_machine_types() -> BTreeSet<Pair> {
    pairs(ZONES.iter().flat_map(|z| {
        MACHINE_TYPES
            .iter()
            .filter(|m| z.offers_machine_type(m))
            .map(move |m| (z.name, m.name))
    }))
}

/// Compares `live` with the catalog.
#[must_use]
pub fn compare(live: &LiveState) -> DriftReport {
    let zone_gpus = expected_zone_gpus();
    let from_machine_types = zone_gpus
        .iter()
        .filter(|(_, g)| !N1_ONLY.contains(&g.as_str()))
        .cloned()
        .collect();
    // Compute Engine lists some accelerator types in zones where no machine
    // type carries them, so nothing can use them there.
    let usable: BTreeSet<Pair> = live
        .accelerators
        .iter()
        .filter(|p| N1_ONLY.contains(&p.1.as_str()) || live.machine_type_gpus.contains(*p))
        .cloned()
        .collect();
    // The API lists the fractional G4 machine types in every G4 zone, but
    // Google documents them in one zone only.
    let machine_types: BTreeSet<Pair> = live
        .machine_types
        .iter()
        .filter(|(zone, name)| {
            MACHINE_TYPES
                .iter()
                .find(|m| m.name == name)
                .is_none_or(|m| {
                    !m.is_fractional()
                        || ZONES
                            .iter()
                            .any(|z| z.name == zone && z.offers_machine_type(m))
                })
        })
        .cloned()
        .collect();
    DriftReport {
        zone_gpus: diff(&zone_gpus, &usable),
        zone_gpus_from_machine_types: diff(&from_machine_types, &live.machine_type_gpus),
        zone_machine_types: diff(&expected_zone_machine_types(), &machine_types),
    }
}

#[must_use]
pub fn diff(expected: &BTreeSet<Pair>, live: &BTreeSet<Pair>) -> Diff {
    let (hidden, missing) = expected
        .difference(live)
        .cloned()
        .partition(|(zone, _)| ZONES.iter().any(|z| z.name == zone && z.ai_zone));
    Diff {
        expected: expected.len(),
        live: live.len(),
        missing,
        hidden,
        extra: live.difference(expected).cloned().collect(),
    }
}

/// `zones/us-central1-a` to `us-central1-a`; other scopes (`regions/...`) are
/// not zones.
fn zone_of(scope: &str) -> Option<&str> {
    scope.strip_prefix("zones/")
}

/// The accelerator type without its `-vws` suffix, if the catalog knows it.
fn known_gpu(name: &str) -> Option<&'static str> {
    resolve_model(name.strip_suffix("-vws").unwrap_or(name))
        .filter(|m| m.name == name.strip_suffix("-vws").unwrap_or(name))
        .map(|m| m.name)
}

/// Adds one `acceleratorTypes` aggregated-list page to `state`.
pub fn add_accelerator_page(state: &mut LiveState, page: &Value) {
    for (scope, items) in page["items"].as_object().into_iter().flatten() {
        let Some(zone) = zone_of(scope) else { continue };
        for a in items["acceleratorTypes"].as_array().into_iter().flatten() {
            if let Some(gpu) = a["name"].as_str().and_then(known_gpu) {
                state.accelerators.insert((zone.to_owned(), gpu.to_owned()));
            }
        }
    }
}

/// Adds one `machineTypes` aggregated-list page to `state`. A machine type
/// the catalog does not know still counts when it carries a known GPU.
pub fn add_machine_type_page(state: &mut LiveState, page: &Value) {
    let catalog = GcpCatalog::new();
    for (scope, items) in page["items"].as_object().into_iter().flatten() {
        let Some(zone) = zone_of(scope) else { continue };
        for m in items["machineTypes"].as_array().into_iter().flatten() {
            let Some(name) = m["name"].as_str() else {
                continue;
            };
            let gpu = match catalog.machine_type(name) {
                Some(known) => Some(known.gpu),
                None => m["accelerators"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find_map(|a| a["guestAcceleratorType"].as_str().and_then(known_gpu)),
            };
            if let Some(gpu) = gpu {
                if !name.ends_with(NO_LSSD_SUFFIX) {
                    state
                        .machine_types
                        .insert((zone.to_owned(), name.to_owned()));
                }
                state
                    .machine_type_gpus
                    .insert((zone.to_owned(), gpu.to_owned()));
            }
        }
    }
}

/// Percent-encodes a query value.
fn encode(value: &str) -> String {
    value.bytes().fold(String::new(), |mut out, b| {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
        out
    })
}

async fn pages(
    client: &GcpClient,
    base: &str,
    filter: Option<&str>,
    mut each: impl FnMut(&Value),
) -> Result<(), GcpError> {
    let mut token: Option<String> = None;
    loop {
        let mut url = format!("{base}?maxResults=500");
        if let Some(filter) = filter {
            let _ = write!(url, "&filter={}", encode(filter));
        }
        if let Some(token) = &token {
            let _ = write!(url, "&pageToken={}", encode(token));
        }
        let page = client.request(HttpMethod::Get, &url, None).await?;
        each(&page);
        match page["nextPageToken"].as_str().filter(|t| !t.is_empty()) {
            Some(next) => token = Some(next.to_owned()),
            None => return Ok(()),
        }
    }
}

/// Reads what `project` lists, following `nextPageToken`. Only `GET`s.
///
/// # Errors
/// [`GcpError::Invalid`] for a malformed project id, and any client error
/// (permission denied when the project cannot list Compute Engine zones).
pub async fn fetch_live_state(client: &GcpClient, project: &str) -> Result<LiveState, GcpError> {
    validate_project_id(project)?;
    let mut state = LiveState::default();
    let base = format!("{COMPUTE_BASE_URL}/projects/{project}/aggregated");
    pages(client, &format!("{base}/acceleratorTypes"), None, |page| {
        add_accelerator_page(&mut state, page);
    })
    .await?;
    pages(
        client,
        &format!("{base}/machineTypes"),
        Some(MACHINE_TYPE_FILTER),
        |page| add_machine_type_page(&mut state, page),
    )
    .await?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::client::tests::{Mock, client};

    fn live_matching_the_catalog() -> LiveState {
        let mut live = LiveState {
            accelerators: expected_zone_gpus(),
            machine_types: expected_zone_machine_types(),
            ..LiveState::default()
        };
        live.machine_type_gpus = compare(&LiveState::default())
            .zone_gpus_from_machine_types
            .missing;
        live
    }

    #[test]
    fn identical_state_has_no_drift() {
        let report = compare(&live_matching_the_catalog());
        assert!(report.is_empty(), "{report:?}");
        assert!(report.zone_gpus.expected > 200);
    }

    #[test]
    fn reports_missing_and_extra_pairs() {
        let mut live = live_matching_the_catalog();
        let gone = ("us-central1-a".to_owned(), "nvidia-l4".to_owned());
        let new = ("us-central1-z".to_owned(), "nvidia-h100-80gb".to_owned());
        assert!(live.accelerators.remove(&gone));
        live.accelerators.insert(new.clone());
        live.machine_type_gpus.insert(new.clone());
        let report = compare(&live);
        assert_eq!(report.zone_gpus.missing, BTreeSet::from([gone]));
        assert_eq!(report.zone_gpus.extra, BTreeSet::from([new]));
        assert!(report.zone_machine_types.is_empty());
        assert!(!report.is_empty());
    }

    #[test]
    fn ai_zone_entries_missing_from_the_api_are_hidden_not_drift() {
        let mut live = live_matching_the_catalog();
        let ai: Vec<Pair> = expected_zone_gpus()
            .into_iter()
            .filter(|(z, _)| z == "europe-west4-ai1a")
            .collect();
        assert_ne!(ai.len(), 0);
        for p in &ai {
            live.accelerators.remove(p);
        }
        let report = compare(&live);
        assert!(report.is_empty(), "{report:?}");
        assert_eq!(report.zone_gpus.hidden, ai.into_iter().collect());
        assert!(report.zone_gpus.missing.is_empty());
        // The same entries missing from an ordinary zone are real drift.
        live.accelerators
            .remove(&("us-central1-a".to_owned(), "nvidia-l4".to_owned()));
        assert!(!compare(&live).is_empty());
    }

    #[test]
    fn parses_accelerator_pages() {
        let mut state = LiveState::default();
        add_accelerator_page(
            &mut state,
            &json!({"items": {
                "zones/us-central1-a": {"acceleratorTypes": [
                    {"name": "nvidia-l4"}, {"name": "nvidia-l4-vws"},
                    {"name": "nvidia-unknown"}, {"name": "nvidia-tesla-t4"}]},
                "zones/us-east1-b": {"warning": {"code": "NO_RESULTS_ON_PAGE"}},
                "regions/us-east1": {"acceleratorTypes": [{"name": "nvidia-l4"}]}
            }}),
        );
        assert_eq!(
            state.accelerators,
            BTreeSet::from([
                ("us-central1-a".to_owned(), "nvidia-l4".to_owned()),
                ("us-central1-a".to_owned(), "nvidia-tesla-t4".to_owned()),
            ])
        );
    }

    #[test]
    fn parses_machine_type_pages_including_unknown_gpu_types() {
        let mut state = LiveState::default();
        add_machine_type_page(
            &mut state,
            &json!({"items": {"zones/us-central1-b": {"machineTypes": [
                {"name": "a3-highgpu-8g"},
                {"name": "g2-standard-4"},
                {"name": "a5-newgpu-8g", "accelerators": [
                    {"guestAcceleratorType": "nvidia-b200", "guestAcceleratorCount": 8}]},
                {"name": "a9-nogpu-1g"}
            ]}}}),
        );
        let zone = "us-central1-b".to_owned();
        assert_eq!(
            state.machine_types,
            BTreeSet::from([
                (zone.clone(), "a3-highgpu-8g".to_owned()),
                (zone.clone(), "g2-standard-4".to_owned()),
                (zone.clone(), "a5-newgpu-8g".to_owned()),
            ])
        );
        assert!(
            state
                .machine_type_gpus
                .contains(&(zone, "nvidia-b200".to_owned()))
        );
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(
            encode(MACHINE_TYPE_FILTER),
            "name%20eq%20%22%28a4x%7Ca4%7Ca3%7Ca2%7Cg4%7Cg2%29-.%2A%22"
        );
        assert_eq!(encode("CAE="), "CAE%3D");
    }

    #[tokio::test]
    async fn fetch_follows_pages_and_only_reads() {
        let (c, mock) = client(
            Mock::default()
                .on(
                    HttpMethod::Get,
                    "/aggregated/acceleratorTypes?maxResults=500",
                    200,
                    r#"{"items":{"zones/us-central1-a":{"acceleratorTypes":[{"name":"nvidia-l4"}]}},"nextPageToken":"CAE="}"#,
                )
                .on(
                    HttpMethod::Get,
                    "/aggregated/acceleratorTypes?maxResults=500&pageToken=CAE%3D",
                    200,
                    r#"{"items":{"zones/us-central1-b":{"acceleratorTypes":[{"name":"nvidia-b200"}]}}}"#,
                )
                .on(
                    HttpMethod::Get,
                    "%22",
                    200,
                    r#"{"items":{"zones/us-central1-a":{"machineTypes":[{"name":"g2-standard-4"}]}}}"#,
                ),
        );
        let live = fetch_live_state(&c, "my-project").await.unwrap();
        assert_eq!(live.accelerators.len(), 2);
        assert_eq!(live.machine_types.len(), 1);
        let seen = mock.requests();
        assert_eq!(seen.len(), 3);
        assert!(
            seen.iter()
                .all(|r| r.method == HttpMethod::Get && r.body.is_none())
        );
        assert!(seen[2].url.starts_with(
            "https://compute.googleapis.com/compute/v1/projects/my-project/aggregated/machineTypes?"
        ));
    }

    #[tokio::test]
    async fn fetch_rejects_a_bad_project_before_any_call() {
        let (c, mock) = client(Mock::default());
        assert!(matches!(
            fetch_live_state(&c, "Bad Project").await,
            Err(GcpError::Invalid { .. })
        ));
        assert!(mock.requests().is_empty());
    }
}
