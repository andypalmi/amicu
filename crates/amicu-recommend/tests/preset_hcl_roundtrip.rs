//! Round-trip check for wizard-preset graphs through the HCL front end.
//!
//! `amicu-hcl` is a dev-dependency only: the round-trip property belongs to the
//! preset builder's output, but wiring it as a normal dependency would add a
//! production edge from the recommendation engine to the DSL front end that the
//! model itself does not need. Keeping it in `[dev-dependencies]` proves the
//! property without that edge.

use amicu_core::resource::Money;
use amicu_hcl::{format_graph, parse_graph};
use amicu_recommend::{WizardAnswers, build_graph, catalog};

fn answers() -> WizardAnswers {
    WizardAnswers {
        context_length: 8192,
        concurrency: 16,
        eu_residency_required: false,
        budget: Some(Money::new(500.0, "EUR")),
        max_latency_ms: Some(1500),
    }
}

#[test]
fn every_preset_graph_round_trips_through_hcl() {
    for preset in catalog() {
        let graph = build_graph(preset, &answers()).expect("build graph");
        let text = format_graph(&graph).expect("format to HCL");
        let reparsed = parse_graph(&text).expect("parse HCL back");
        assert_eq!(
            graph, reparsed,
            "preset `{}` did not round-trip through HCL",
            preset.id
        );
    }
}

#[test]
fn eu_preset_graph_forbids_external_providers_after_round_trip() {
    let eu = catalog()
        .iter()
        .find(|p| p.id.as_str() == "private_eu_production")
        .expect("eu preset in catalog");
    let graph = build_graph(eu, &answers()).expect("build graph");
    let reparsed = parse_graph(&format_graph(&graph).expect("format")).expect("parse");

    assert!(
        reparsed
            .data_classes
            .iter()
            .any(|dc| !dc.allow_external_providers),
        "EU preset graph must keep a data class with allow_external_providers == false"
    );
    assert!(
        !reparsed.governance_rules.is_empty(),
        "EU preset graph must carry a governance rule after round-trip"
    );
}
