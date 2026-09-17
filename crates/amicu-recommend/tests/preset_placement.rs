//! Plan-time placement checks for wizard-preset graphs.
//!
//! These guard against a class of bug the reviewer found: the EU preset once
//! pinned fabricated region codes that no allowed provider offers, which made
//! the residency check a silent no-op and turned an `allowed_regions` allowlist
//! into a trap that rejected genuine EU regions. `amicu-engine` and
//! `amicu-providers` are dev-dependencies only, so these checks exercise the
//! real planner entrypoint (`amicu_engine::report_for`) and residency map
//! (`amicu_providers::resolve_residency`) without adding a production edge from
//! the recommendation engine.

use amicu_core::resource::{Region, Residency};
use amicu_engine::report_for;
use amicu_providers::resolve_residency;
use amicu_recommend::{WizardAnswers, build_graph, catalog};

fn answers() -> WizardAnswers {
    WizardAnswers {
        context_length: 8192,
        concurrency: 16,
        eu_residency_required: false,
        budget: None,
        max_latency_ms: Some(1500),
        priority: None,
    }
}

fn eu_preset_graph() -> amicu_core::graph::Graph {
    let eu = catalog()
        .iter()
        .find(|p| p.id.as_str() == "private_eu_production")
        .expect("eu preset in catalog");
    build_graph(eu, &answers()).expect("build eu graph")
}

#[test]
fn eu_preset_hardware_region_is_a_real_eu_code() {
    // The emitted hardware region must resolve to Eu via the provider catalog,
    // i.e. it is a genuine code, not a fabricated one that resolves to None.
    let graph = eu_preset_graph();
    let hw = &graph.hardware_profiles[0];
    let region = hw
        .region
        .as_ref()
        .expect("eu hardware has a region")
        .code
        .as_str();
    assert_eq!(
        resolve_residency(hw.provider.as_str(), region),
        Some(Residency::Eu),
        "region {region:?} on provider {:?} must resolve to Eu",
        hw.provider
    );
}

#[test]
fn eu_preset_graph_passes_plan_time_checks() {
    // The preset's own placement must be free of error-severity diagnostics.
    let report = report_for(&eu_preset_graph());
    assert!(
        report.valid,
        "EU preset graph should pass plan-time checks, got: {:?}",
        report.diagnostics
    );
}

#[test]
fn valid_edit_to_another_eu_region_is_not_rejected() {
    // A user editing the hardware to a different genuine EU region on the same
    // allowed provider must NOT be rejected. This fails if a self-defeating
    // allowed_regions code allowlist is ever reintroduced.
    for other_eu_code in ["nbg1", "hel1"] {
        assert_eq!(
            resolve_residency("hetzner", other_eu_code),
            Some(Residency::Eu),
            "{other_eu_code} should be a real hetzner EU region"
        );
        let mut graph = eu_preset_graph();
        graph.hardware_profiles[0].region = Some(Region::from_code(other_eu_code));
        let report = report_for(&graph);
        assert!(
            report.valid,
            "editing to EU region {other_eu_code} should stay valid, got: {:?}",
            report.diagnostics
        );
    }
}

#[test]
fn non_eu_edit_is_rejected() {
    // Editing the EU preset's hardware to a non-EU provider and region must be
    // rejected by the planner: the data class pins EU residency and the
    // governance rule allows only EU providers, so a US placement violates both.
    assert_eq!(
        resolve_residency("aws", "us-east-1"),
        Some(Residency::Us),
        "us-east-1 on aws should resolve to the US jurisdiction"
    );
    let mut graph = eu_preset_graph();
    graph.hardware_profiles[0].provider = "aws".to_string();
    graph.hardware_profiles[0].region = Some(Region::from_code("us-east-1"));
    let report = report_for(&graph);
    assert!(
        !report.valid,
        "a non-EU (aws/us-east-1) placement must be rejected, got: {:?}",
        report.diagnostics
    );
}

#[test]
fn every_preset_honors_eu_residency_required_without_an_invalid_graph() {
    // Generalizes the local_dev_chat/cost_optimized_batch regressions guarded
    // in `amicu_recommend::preset`'s own tests to every catalog preset
    // (amicu-planning#149): whatever shape a preset's base graph takes,
    // setting `eu_residency_required = true` must never produce a graph
    // `report_for` rejects.
    let mut a = answers();
    a.eu_residency_required = true;
    for preset in catalog() {
        let graph = build_graph(preset, &a).expect("build");
        let report = report_for(&graph);
        assert!(
            report.valid,
            "{} with eu_residency_required=true should validate cleanly, got {:?}",
            preset.id, report.diagnostics
        );
    }
}

#[test]
fn eu_preset_pins_no_region_code_allowlist() {
    // Belt-and-braces on top of the engine check: the governance rule must rely
    // on the EU-only provider set, not a region-code allowlist.
    let graph = eu_preset_graph();
    for rule in &graph.governance_rules {
        assert!(
            rule.allowed_regions.is_empty(),
            "no governance rule may pin an allowed_regions code allowlist"
        );
        assert!(!rule.allowed_providers.is_empty());
    }
}
