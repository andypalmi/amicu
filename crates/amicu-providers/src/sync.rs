//! Capability & pricing metadata sync.
//!
//! A sync snapshots each registered provider's [`ProviderCapabilities`] (which
//! carries its pricing) with a freshness/provenance tag, so stale or
//! unavailable data is *surfaced*, never fabricated. Following the family split,
//! the freshness decision lives in pure helpers ([`classify`], [`snapshot`]);
//! [`sync_all`] is the thin async driver that consults each
//! [`Adapter::health`](amicu_core::adapter::Adapter::health) and assembles the
//! report.
//!
//! Today's capabilities come from each adapter's *static* hardcoded catalog
//! (the pure [`Provider::capabilities`]), not a live provider fetch. Snapshots
//! are therefore tagged honestly — [`Provenance::Formula`] +
//! [`Confidence::Estimated`] — so catalog data is never presented as live
//! truth. TASK-068 (live fetch) will upgrade this provenance/confidence.

use amicu_core::adapter::{AdapterId, HealthStatus, Registry};
use amicu_core::diagnostic::{Diagnostic, Severity, catalog};
use amicu_core::estimate::{Confidence, Provenance};
use serde::{Deserialize, Serialize};

use crate::capability::ProviderCapabilities;
use crate::provider::Provider;

/// Freshness of a synced capability snapshot.
///
/// Mirrors the [`AvailabilityState::Unknown`](crate::capability::AvailabilityState)
/// precedent: when refresh cannot confirm live data, the snapshot says so
/// rather than presenting stale data as current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// The provider is healthy; its snapshot reflects the current catalog.
    Fresh,
    /// The provider is impaired; the last-known snapshot is surfaced as stale.
    Stale,
    /// The provider is unreachable; no capabilities are carried.
    Unavailable,
}

impl Freshness {
    /// Whether this snapshot is [`Freshness::Fresh`].
    #[must_use]
    pub fn is_fresh(self) -> bool {
        matches!(self, Freshness::Fresh)
    }
}

/// A single provider's synced capability snapshot with its provenance tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncedCapabilities {
    /// The provider this snapshot describes.
    pub provider: AdapterId,
    /// The snapshotted descriptor, or `None` when the provider is unavailable.
    /// Never fabricated: absence is reported as absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ProviderCapabilities>,
    /// When the snapshot was taken, as UNIX epoch seconds (injected by the
    /// caller so pure code stays deterministic).
    pub synced_at: u64,
    /// How much this snapshot can be trusted as current.
    pub freshness: Freshness,
    /// Where the snapshot's data comes from. Catalog-sourced today
    /// ([`Provenance::Formula`]); TASK-068 live fetch will upgrade this.
    pub provenance: Provenance,
    /// Confidence in the snapshot. [`Confidence::Estimated`] while data is
    /// catalog-sourced rather than freshly fetched.
    pub confidence: Confidence,
}

/// The outcome of one sync pass: a snapshot per provider plus any surfaced
/// staleness diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncReport {
    /// One entry per registered provider, sorted by provider id for stable,
    /// diffable output.
    pub entries: Vec<SyncedCapabilities>,
    /// Surfaced staleness/unavailability warnings (`AMICU-PROVIDER-003`).
    pub diagnostics: Vec<Diagnostic>,
}

/// Pure freshness classification from a provider's health.
///
/// `Healthy → Fresh`, `Degraded → Stale`, `Unavailable → Unavailable`.
#[must_use]
pub fn classify(health: &HealthStatus) -> Freshness {
    match health {
        HealthStatus::Healthy => Freshness::Fresh,
        HealthStatus::Degraded { .. } => Freshness::Stale,
        HealthStatus::Unavailable { .. } => Freshness::Unavailable,
    }
}

/// Pure snapshot builder: tag one provider's catalog `capabilities` by its
/// `health` at `now`, returning the snapshot and any staleness diagnostic.
///
/// An unreachable provider yields `None` capabilities — the `capabilities`
/// argument is dropped, never fabricated into the report.
#[must_use]
pub fn snapshot(
    provider: AdapterId,
    capabilities: ProviderCapabilities,
    health: &HealthStatus,
    now: u64,
) -> (SyncedCapabilities, Option<Diagnostic>) {
    let freshness = classify(health);
    let (capabilities, diagnostic) = match freshness {
        Freshness::Fresh => (Some(capabilities), None),
        Freshness::Stale => (
            Some(capabilities),
            Some(stale_diagnostic(
                &provider,
                "capability/pricing data is stale",
            )),
        ),
        // Never fabricate: an unreachable provider carries no capabilities.
        Freshness::Unavailable => (
            None,
            Some(stale_diagnostic(
                &provider,
                "capability/pricing data could not be refreshed",
            )),
        ),
    };
    let entry = SyncedCapabilities {
        provider,
        capabilities,
        synced_at: now,
        freshness,
        provenance: Provenance::Formula,
        confidence: Confidence::Estimated,
    };
    (entry, diagnostic)
}

fn stale_diagnostic(provider: &AdapterId, detail: &str) -> Diagnostic {
    Diagnostic {
        resource: Some(provider.to_string()),
        ..Diagnostic::new(
            catalog::PROVIDER_METADATA_STALE,
            Severity::Warning,
            format!("provider '{provider}': {detail}"),
        )
    }
}

/// Sync every registered provider: consult its health and snapshot its
/// capabilities, surfacing stale/unavailable data instead of fabricating it.
///
/// `now` (UNIX epoch seconds) is injected so the driver — not this logic —
/// owns the wall clock. A provider whose `health()` call itself fails is
/// treated as unavailable.
pub async fn sync_all(registry: &Registry<dyn Provider>, now: u64) -> SyncReport {
    let mut entries = Vec::new();
    let mut diagnostics = Vec::new();
    for provider in registry.list() {
        let health = match provider.health().await {
            Ok(health) => health,
            Err(err) => HealthStatus::Unavailable {
                detail: err.to_string(),
            },
        };
        let (entry, diagnostic) = snapshot(provider.id(), provider.capabilities(), &health, now);
        if let Some(diagnostic) = diagnostic {
            diagnostics.push(diagnostic);
        }
        entries.push(entry);
    }
    // `registry.list()` yields providers in unspecified (hash) order; sort by id
    // so successive reports are stable and diffable.
    entries.sort_by(|a, b| a.provider.as_str().cmp(b.provider.as_str()));
    SyncReport {
        entries,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeProvider;
    use amicu_core::adapter::Registry;
    use std::sync::Arc;

    #[test]
    fn classify_maps_health_to_freshness() {
        assert_eq!(classify(&HealthStatus::Healthy), Freshness::Fresh);
        assert_eq!(
            classify(&HealthStatus::Degraded {
                detail: "maintenance".into()
            }),
            Freshness::Stale
        );
        assert_eq!(
            classify(&HealthStatus::Unavailable {
                detail: "down".into()
            }),
            Freshness::Unavailable
        );
    }

    #[test]
    fn healthy_snapshot_is_fresh_with_capabilities_and_no_diagnostic() {
        let caps = FakeProvider::sample_capabilities(&AdapterId::new("p"));
        let (entry, diag) = snapshot(AdapterId::new("p"), caps, &HealthStatus::Healthy, 42);
        assert_eq!(entry.freshness, Freshness::Fresh);
        assert!(entry.capabilities.is_some());
        assert_eq!(entry.synced_at, 42);
        assert_eq!(entry.provenance, Provenance::Formula);
        assert_eq!(entry.confidence, Confidence::Estimated);
        assert!(diag.is_none());
    }

    #[test]
    fn unavailable_snapshot_drops_capabilities_and_warns() {
        let caps = FakeProvider::sample_capabilities(&AdapterId::new("p"));
        let (entry, diag) = snapshot(
            AdapterId::new("p"),
            caps,
            &HealthStatus::Unavailable {
                detail: "down".into(),
            },
            7,
        );
        assert_eq!(entry.freshness, Freshness::Unavailable);
        // Never fabricated.
        assert!(entry.capabilities.is_none());
        let diag = diag.expect("unavailable provider surfaces a diagnostic");
        assert_eq!(diag.code.to_string(), "AMICU-PROVIDER-003");
        assert_eq!(diag.severity, Severity::Warning);
    }

    #[test]
    fn degraded_snapshot_is_stale_and_warns() {
        let caps = FakeProvider::sample_capabilities(&AdapterId::new("p"));
        let (entry, diag) = snapshot(
            AdapterId::new("p"),
            caps,
            &HealthStatus::Degraded {
                detail: "impaired".into(),
            },
            1,
        );
        assert_eq!(entry.freshness, Freshness::Stale);
        assert!(entry.capabilities.is_some());
        assert_eq!(
            diag.expect("stale surfaces a diagnostic").code.to_string(),
            "AMICU-PROVIDER-003"
        );
    }

    #[tokio::test]
    async fn sync_all_surfaces_unavailable_without_fabricating() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(FakeProvider::new("healthy-cloud")));
        registry.register(Arc::new(FakeProvider::new("down-cloud").with_health(
            HealthStatus::Unavailable {
                detail: "unreachable".into(),
            },
        )));

        let report = sync_all(&registry, 100).await;
        assert_eq!(report.entries.len(), 2);

        let healthy = report
            .entries
            .iter()
            .find(|e| e.provider == AdapterId::new("healthy-cloud"))
            .unwrap();
        assert_eq!(healthy.freshness, Freshness::Fresh);
        assert!(healthy.capabilities.is_some());
        assert_eq!(healthy.synced_at, 100);

        let down = report
            .entries
            .iter()
            .find(|e| e.provider == AdapterId::new("down-cloud"))
            .unwrap();
        assert_eq!(down.freshness, Freshness::Unavailable);
        // No fabricated capabilities for the unavailable provider.
        assert!(down.capabilities.is_none());

        // Exactly the unavailable provider is surfaced as stale.
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].code, catalog::PROVIDER_METADATA_STALE);
        assert_eq!(report.diagnostics[0].code.to_string(), "AMICU-PROVIDER-003");
    }

    #[tokio::test]
    async fn entries_are_sorted_by_provider_id() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        for id in ["zeta", "alpha", "mu"] {
            registry.register(Arc::new(FakeProvider::new(id)));
        }
        let report = sync_all(&registry, 100).await;
        let ids: Vec<&str> = report.entries.iter().map(|e| e.provider.as_str()).collect();
        assert_eq!(ids, ["alpha", "mu", "zeta"]);
    }

    #[tokio::test]
    async fn sync_report_round_trips_serde() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(FakeProvider::new("healthy-cloud")));
        registry.register(Arc::new(FakeProvider::new("down-cloud").with_health(
            HealthStatus::Unavailable {
                detail: "unreachable".into(),
            },
        )));
        let report = sync_all(&registry, 100).await;

        let json = serde_json::to_string(&report).unwrap();
        let back: SyncReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }
}
