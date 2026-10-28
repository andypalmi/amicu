//! Drift check: compares the catalog with what a project's Compute Engine
//! API lists, so a stale table shows up before a deploy fails on it.
//!
//! [`fetch_live_state`] lists the project's zones, then each zone's
//! `acceleratorTypes` and `machineTypes` (the latter filtered to the GPU
//! series), and [`compare`] diffs them against the catalog. Both only read.
//! The `verify-catalog` example runs them against a real project. The
//! `aggregated/*` list calls are not used: they omit zones the zonal calls
//! return, and differ between runs.
//!
//! The API lists only what the queried project can see. Zones gated to
//! specific projects (and AI zones such as `europe-west4-ai1a`) show up as
//! MISSING even though the catalog is right. AI-zone entries are therefore
//! reported separately in [`Diff::hidden`] and never count as drift; a
//! missing entry in any other zone may be project gating too, so run the
//! check from a project known to have access.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::Duration;

use futures_util::stream::{self, StreamExt as _};

use amicu_providers::HttpMethod;
use serde_json::Value;

use super::{GcpCatalog, MACHINE_TYPES, ZONES, resolve_model};
use crate::client::{COMPUTE_BASE_URL, GcpClient};
use crate::error::GcpError;
use crate::validate::validate_project_id;

/// GPUs the API never lists as an accelerator type: only their machine types
/// (A4X Max) carry them, in every zone.
const MACHINE_TYPE_ONLY: [&str; 1] = ["nvidia-gb300"];

/// Machine type names of the GPU series, as a Compute Engine list filter.
const MACHINE_TYPE_FILTER: &str = r#"name eq "(a4x|a4|a3|a2|g4|g2)-.*""#;

/// N1 GPUs have no machine type of their own, so machine types never show them.
const N1_ONLY: [&str; 3] = ["nvidia-tesla-t4", "nvidia-tesla-p4", "nvidia-tesla-v100"];

/// `a3-edgegpu-8g-nolssd` and `a3-ultragpu-8g-nolssd` are the same shapes as
/// their base machine types minus the local SSD, so the catalog does not list them.
const NO_LSSD_SUFFIX: &str = "-nolssd";

/// Zones fetched at once.
const ZONE_CONCURRENCY: usize = 8;

/// Attempts per request; a retryable failure (429, 5xx, transport) waits
/// `RETRY_BASE * 2^attempt` first.
const MAX_ATTEMPTS: u32 = 6;
const RETRY_BASE: Duration = Duration::from_secs(1);

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
    let accelerator_view: BTreeSet<Pair> = zone_gpus
        .iter()
        .filter(|(_, g)| !MACHINE_TYPE_ONLY.contains(&g.as_str()))
        .cloned()
        .collect();
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
        zone_gpus: diff(&accelerator_view, &usable),
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

/// The accelerator type without its `-vws` suffix, if the catalog knows it.
fn known_gpu(name: &str) -> Option<&'static str> {
    resolve_model(name.strip_suffix("-vws").unwrap_or(name))
        .filter(|m| m.name == name.strip_suffix("-vws").unwrap_or(name))
        .map(|m| m.name)
}

/// Adds one zonal `acceleratorTypes` list page to `state`.
pub fn add_accelerator_page(state: &mut LiveState, zone: &str, page: &Value) {
    for a in page["items"].as_array().into_iter().flatten() {
        if let Some(gpu) = a["name"].as_str().and_then(known_gpu) {
            state.accelerators.insert((zone.to_owned(), gpu.to_owned()));
        }
    }
}

/// Adds one zonal `machineTypes` list page to `state`. A machine type the
/// catalog does not know still counts when it carries a known GPU.
pub fn add_machine_type_page(state: &mut LiveState, zone: &str, page: &Value) {
    let catalog = GcpCatalog::new();
    for m in page["items"].as_array().into_iter().flatten() {
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

/// `GET`s `url`, retrying rate limits, 5xx and transport failures.
async fn get(client: &GcpClient, url: &str) -> Result<Value, GcpError> {
    let mut attempt = 0;
    loop {
        match client.request(HttpMethod::Get, url, None).await {
            Err(e) if e.is_retryable() && attempt + 1 < MAX_ATTEMPTS => {
                tokio::time::sleep(RETRY_BASE * 2u32.pow(attempt)).await;
                attempt += 1;
            }
            other => return other,
        }
    }
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
        let page = get(client, &url).await?;
        each(&page);
        match page["nextPageToken"].as_str().filter(|t| !t.is_empty()) {
            Some(next) => token = Some(next.to_owned()),
            None => return Ok(()),
        }
    }
}

async fn fetch_zone(client: &GcpClient, base: &str, zone: &str) -> Result<LiveState, GcpError> {
    let mut state = LiveState::default();
    let base = format!("{base}/zones/{zone}");
    pages(client, &format!("{base}/acceleratorTypes"), None, |page| {
        add_accelerator_page(&mut state, zone, page);
    })
    .await?;
    pages(
        client,
        &format!("{base}/machineTypes"),
        Some(MACHINE_TYPE_FILTER),
        |page| add_machine_type_page(&mut state, zone, page),
    )
    .await?;
    Ok(state)
}

/// Reads what `project` lists, zone by zone, following `nextPageToken`. Only
/// `GET`s.
///
/// # Errors
/// [`GcpError::Invalid`] for a malformed project id, and any client error
/// (permission denied when the project cannot list Compute Engine zones).
pub async fn fetch_live_state(client: &GcpClient, project: &str) -> Result<LiveState, GcpError> {
    validate_project_id(project)?;
    let base = format!("{COMPUTE_BASE_URL}/projects/{project}");
    let mut zones = BTreeSet::new();
    pages(client, &format!("{base}/zones"), None, |page| {
        zones.extend(
            page["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|z| z["name"].as_str().map(str::to_owned)),
        );
    })
    .await?;
    let base = &base;
    let mut results = stream::iter(&zones)
        .map(|zone| async move { fetch_zone(client, base, zone).await })
        .buffer_unordered(ZONE_CONCURRENCY);
    let mut state = LiveState::default();
    while let Some(zone) = results.next().await {
        let zone = zone?;
        state.accelerators.extend(zone.accelerators);
        state.machine_types.extend(zone.machine_types);
        state.machine_type_gpus.extend(zone.machine_type_gpus);
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::client::tests::{Mock, client};

    fn live_matching_the_catalog() -> LiveState {
        let mut live = LiveState {
            accelerators: expected_zone_gpus()
                .into_iter()
                .filter(|(_, g)| !MACHINE_TYPE_ONLY.contains(&g.as_str()))
                .collect(),
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
            "us-central1-a",
            &json!({"items": [
                {"name": "nvidia-l4"}, {"name": "nvidia-l4-vws"},
                {"name": "nvidia-unknown"}, {"name": "nvidia-tesla-t4"}]}),
        );
        add_accelerator_page(&mut state, "us-east1-b", &json!({}));
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
            "us-central1-b",
            &json!({"items": [
                {"name": "a3-highgpu-8g"},
                {"name": "g2-standard-4"},
                {"name": "a5-newgpu-8g", "accelerators": [
                    {"guestAcceleratorType": "nvidia-b200", "guestAcceleratorCount": 8}]},
                {"name": "a9-nogpu-1g"}
            ]}),
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

    #[tokio::test(start_paused = true)]
    async fn fetch_lists_each_zone_follows_pages_retries_and_only_reads() {
        let (c, mock) = client(
            Mock::default()
                .on(
                    HttpMethod::Get,
                    "/projects/my-project/zones?maxResults=500",
                    200,
                    r#"{"items":[{"name":"us-central1-a"},{"name":"us-central1-b"}]}"#,
                )
                .on(
                    HttpMethod::Get,
                    "/zones/us-central1-a/acceleratorTypes?maxResults=500",
                    200,
                    r#"{"items":[{"name":"nvidia-l4"}],"nextPageToken":"CAE="}"#,
                )
                .on(
                    HttpMethod::Get,
                    "/zones/us-central1-a/acceleratorTypes?maxResults=500&pageToken=CAE%3D",
                    200,
                    r#"{"items":[{"name":"nvidia-tesla-t4"}]}"#,
                )
                .on(
                    HttpMethod::Get,
                    "/zones/us-central1-b/acceleratorTypes?maxResults=500",
                    429,
                    r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"slow"}}"#,
                )
                .on(
                    HttpMethod::Get,
                    "/zones/us-central1-b/acceleratorTypes?maxResults=500",
                    200,
                    r#"{"items":[{"name":"nvidia-b200"}]}"#,
                )
                .on(
                    HttpMethod::Get,
                    "/zones/us-central1-a/machineTypes?maxResults=500&filter=name%20eq%20%22%28a4x%7Ca4%7Ca3%7Ca2%7Cg4%7Cg2%29-.%2A%22",
                    200,
                    r#"{"items":[{"name":"g2-standard-4"}]}"#,
                )
                .on(
                    HttpMethod::Get,
                    "/zones/us-central1-b/machineTypes?maxResults=500&filter=name%20eq%20%22%28a4x%7Ca4%7Ca3%7Ca2%7Cg4%7Cg2%29-.%2A%22",
                    200,
                    "{}",
                ),
        );
        let live = fetch_live_state(&c, "my-project").await.unwrap();
        assert_eq!(live.accelerators.len(), 3);
        assert_eq!(live.machine_types.len(), 1);
        let seen = mock.requests();
        assert_eq!(seen.len(), 7);
        assert!(
            seen.iter()
                .all(|r| r.method == HttpMethod::Get && r.body.is_none())
        );
        assert!(
            seen.iter()
                .all(|r| !r.url.contains("/aggregated/") && r.url.contains("/projects/my-project/"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_gives_up_after_repeated_server_errors() {
        let (c, mock) = client(Mock::default());
        assert!(fetch_live_state(&c, "my-project").await.is_err());
        assert_eq!(mock.requests().len(), MAX_ATTEMPTS as usize);
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
