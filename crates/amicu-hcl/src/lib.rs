//! HCL front end for Amicu: parse the user DSL into the canonical
//! [`amicu_core::graph::Graph`] and format a graph back to HCL.
//!
//! The round-trip contract is semantic, not textual:
//! `parse_graph(&format_graph(&g)) == g` for any graph produced by
//! [`parse_graph`].

mod de;
mod error;
mod ser;

pub use de::parse_graph;
pub use error::HclError;
pub use ser::format_graph;

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::resource::{Quantization, Runtime, SourceRegistry};

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
        let reparsed = parse_graph(&format_graph(&graph)).unwrap();
        assert_eq!(graph, reparsed);
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
