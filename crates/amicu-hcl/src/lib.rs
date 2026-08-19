//! HCL front end for Amicu: parse the user DSL into the canonical
//! [`amicu_core::graph::Graph`] and format a graph back to HCL.
//!
//! The round-trip contract is semantic, not textual:
//! `parse_graph(&format_graph(&g)?) == g` for any graph produced by
//! [`parse_graph`]. [`format_graph`] is fallible: it rejects graphs whose
//! resource ids or label keys are not round-trip-safe HCL identifiers rather
//! than corrupting them silently.

mod de;
mod error;
mod ser;

pub use de::parse_graph;
pub use error::HclError;
pub use ser::format_graph;

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::graph::Graph;
    use amicu_core::resource::{
        Artifact, ArtifactFormat, Model, ModelSource, Quantization, Runtime, SourceRegistry, Tenant,
    };
    use std::collections::BTreeMap;

    const SAMPLE: &str = include_str!("../tests/sample.amicu.hcl");

    #[test]
    fn parses_prd_example_fields() {
        let graph = parse_graph(SAMPLE).unwrap();

        let model = &graph.models[0];
        assert_eq!(model.id.as_str(), "support_chat");
        assert_eq!(model.source.registry, SourceRegistry::HuggingFace);
        assert_eq!(model.source.reference, "Qwen/Qwen2.5-7B-Instruct");
        assert_eq!(model.artifact.quantization, Some(Quantization::Q4KM));
        assert_eq!(model.context_length, Some(8192));

        let hw = &graph.hardware_profiles[0];
        assert_eq!(hw.id.as_str(), "developer_mac");
        assert_eq!(hw.memory.to_string(), "48GiB");

        let dep = &graph.deployments[0];
        assert_eq!(dep.model.as_str(), "support_chat");
        assert_eq!(dep.hardware_profile.as_str(), "developer_mac");
        match &dep.runtime {
            Runtime::LlamaCpp(s) => {
                assert_eq!(s.context_length, 8192);
                assert_eq!(s.port, 8000);
                assert_eq!(s.n_gpu_layers, -1);
            }
            Runtime::Vllm(_) => panic!("expected llama_cpp runtime"),
        }
    }

    #[test]
    fn all_nine_kinds_are_present() {
        let graph = parse_graph(SAMPLE).unwrap();
        assert!(!graph.models.is_empty());
        assert!(!graph.hardware_profiles.is_empty());
        assert!(!graph.deployments.is_empty());
        assert!(!graph.routing_policies.is_empty());
        assert!(!graph.data_classes.is_empty());
        assert!(!graph.governance_rules.is_empty());
        assert!(!graph.budgets.is_empty());
        assert!(!graph.tenants.is_empty());
        assert!(!graph.use_cases.is_empty());
        assert!(graph.missing_references().is_empty());
    }

    #[test]
    fn round_trips_without_semantic_loss() {
        let graph = parse_graph(SAMPLE).unwrap();
        let reparsed = parse_graph(&format_graph(&graph).unwrap()).unwrap();
        assert_eq!(graph, reparsed);
    }

    fn model_with_id(id: &str) -> Model {
        Model {
            id: id.into(),
            source: ModelSource {
                registry: SourceRegistry::HuggingFace,
                reference: "org/model".into(),
            },
            architecture: "llama".into(),
            modalities: Vec::new(),
            artifact: Artifact {
                format: ArtifactFormat::Gguf,
                quantization: None,
            },
            context_length: None,
            variants: Vec::new(),
        }
    }

    fn graph_with_model(id: &str) -> Graph {
        let mut g = Graph::new();
        g.models.push(model_with_id(id));
        g
    }

    fn tenant_with_labels(id: &str, labels: BTreeMap<String, String>) -> Graph {
        let mut g = Graph::new();
        g.tenants.push(Tenant {
            id: id.into(),
            display_name: None,
            quota: None,
            labels,
        });
        g
    }

    #[test]
    fn non_identifier_ids_are_rejected_not_corrupted() {
        for bad in ["model.with.dots", "has space", "1leading", ""] {
            let g = graph_with_model(bad);
            assert!(
                matches!(format_graph(&g), Err(HclError::InvalidIdentifier { .. })),
                "id {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn identifier_safe_id_round_trips_exactly() {
        let g = graph_with_model("some_model-2");
        let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
        assert_eq!(reparsed.models[0].id.as_str(), "some_model-2");
    }

    #[test]
    fn non_identifier_label_keys_are_rejected() {
        for bad_key in ["cost.center", "cost center"] {
            let mut labels = BTreeMap::new();
            labels.insert(bad_key.to_string(), "x".to_string());
            let g = tenant_with_labels("acme", labels);
            assert!(
                matches!(format_graph(&g), Err(HclError::InvalidIdentifier { .. })),
                "label key {bad_key:?} should be rejected"
            );
        }
    }

    #[test]
    fn identifier_safe_label_key_round_trips() {
        let mut labels = BTreeMap::new();
        labels.insert("cost_center".to_string(), "eng".to_string());
        let g = tenant_with_labels("acme", labels);
        let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
        assert_eq!(
            reparsed.tenants[0]
                .labels
                .get("cost_center")
                .map(String::as_str),
            Some("eng")
        );
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(matches!(
            parse_graph("resource \"model\" {"),
            Err(HclError::Parse(_))
        ));
    }

    #[test]
    fn unknown_kind_is_a_schema_error() {
        let src = "resource \"widget\" \"w\" {}";
        assert!(matches!(parse_graph(src), Err(HclError::Schema(_))));
    }
}
