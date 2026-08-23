//! HCL front end for Amicu: parse the user DSL into the canonical
//! [`amicu_core::graph::Graph`] and format a graph back to HCL.
//!
//! The round-trip contract is semantic, not textual:
//! `parse_graph(&format_graph(&g)?) == g` for any graph produced by
//! [`parse_graph`]. [`format_graph`] is fallible: it rejects graphs whose
//! resource ids, label keys, or reference edge values are not round-trip-safe
//! HCL identifiers rather than corrupting them silently.

mod de;
mod error;
mod secret_scan;
mod ser;

pub use de::parse_graph;
pub use error::HclError;
pub use secret_scan::scan_secret_leaks;
pub use ser::format_graph;

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::ByteSize;
    use amicu_core::graph::Graph;
    use amicu_core::resource::{
        Artifact, ArtifactFormat, Budget, BudgetPeriod, DataClass, Deployment, GovernanceRule,
        HardwareProfile, LlamaCppSettings, Model, ModelSource, Money, Quantization, RoutingPolicy,
        RoutingTarget, RuleEffect, Runtime, Sensitivity, SourceRegistry, Tenant, UseCase,
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

    fn hardware(id: &str) -> HardwareProfile {
        HardwareProfile {
            id: id.into(),
            provider: "local".into(),
            region: None,
            accelerator: None,
            cpu: None,
            memory: ByteSize::from_bytes(48 * 1024 * 1024 * 1024),
            interconnect: None,
            price: None,
        }
    }

    fn deployment(id: &str, model_ref: &str, hw_ref: &str) -> Deployment {
        Deployment {
            id: id.into(),
            model: model_ref.into(),
            hardware_profile: hw_ref.into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
            }),
        }
    }

    fn routing(id: &str, dep_ref: &str) -> RoutingPolicy {
        RoutingPolicy {
            id: id.into(),
            aliases: Vec::new(),
            targets: vec![RoutingTarget {
                deployment: dep_ref.into(),
                weight: 1,
            }],
            failover: None,
            optimize_for: None,
        }
    }

    fn data_class(id: &str) -> DataClass {
        DataClass {
            id: id.into(),
            sensitivity: Sensitivity::Public,
            residency: Vec::new(),
            allow_external_providers: false,
            handling: Vec::new(),
        }
    }

    fn governance(id: &str, dc_ref: &str) -> GovernanceRule {
        GovernanceRule {
            id: id.into(),
            description: None,
            effect: RuleEffect::Allow,
            data_classes: vec![dc_ref.into()],
            allowed_providers: Vec::new(),
            allowed_regions: Vec::new(),
        }
    }

    fn budget(id: &str, tenant_ref: Option<&str>) -> Budget {
        Budget {
            id: id.into(),
            tenant: tenant_ref.map(Into::into),
            limit: Money::new(100.0, "USD"),
            period: BudgetPeriod::Monthly,
            alert_thresholds: Vec::new(),
        }
    }

    fn use_case(id: &str, dc_ref: Option<&str>) -> UseCase {
        UseCase {
            id: id.into(),
            description: None,
            modalities: Vec::new(),
            min_context_length: None,
            max_latency_ms: None,
            required_data_class: dc_ref.map(Into::into),
            priority: None,
        }
    }

    #[test]
    fn dangling_non_identifier_references_are_rejected() {
        let rejected =
            |g: Graph| matches!(format_graph(&g), Err(HclError::InvalidIdentifier { .. }));

        let mut g = Graph::new();
        g.deployments.push(deployment("dep", "bad.model", "hw"));
        assert!(rejected(g), "deployment model reference");

        let mut g = Graph::new();
        g.deployments.push(deployment("dep", "model", "bad hw"));
        assert!(rejected(g), "deployment hardware reference");

        let mut g = Graph::new();
        g.routing_policies.push(routing("route", "bad.dep"));
        assert!(rejected(g), "routing target deployment reference");

        let mut g = Graph::new();
        g.governance_rules.push(governance("rule", "bad.dc"));
        assert!(rejected(g), "governance data_class reference");

        let mut g = Graph::new();
        g.budgets.push(budget("bud", Some("bad tenant")));
        assert!(rejected(g), "budget tenant reference");

        let mut g = Graph::new();
        g.use_cases.push(use_case("uc", Some("bad.dc")));
        assert!(rejected(g), "use_case required_data_class reference");
    }

    #[test]
    fn identifier_safe_references_round_trip_even_when_dangling() {
        let mut g = Graph::new();
        g.deployments
            .push(deployment("edge_dep", "phantom_model", "phantom_hw"));
        let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
        assert_eq!(g, reparsed);
    }

    #[test]
    fn non_identifier_ids_are_rejected_across_kinds() {
        let bad = "has.dot";
        let mut cases: Vec<Graph> = Vec::new();
        let mut push = |g: Graph| cases.push(g);

        let mut g = Graph::new();
        g.hardware_profiles.push(hardware(bad));
        push(g);
        let mut g = Graph::new();
        g.deployments.push(deployment(bad, "model", "hw"));
        push(g);
        let mut g = Graph::new();
        g.routing_policies.push(routing(bad, "dep"));
        push(g);
        let mut g = Graph::new();
        g.data_classes.push(data_class(bad));
        push(g);
        let mut g = Graph::new();
        g.governance_rules.push(governance(bad, "dc"));
        push(g);
        let mut g = Graph::new();
        g.budgets.push(budget(bad, None));
        push(g);
        let mut g = Graph::new();
        g.use_cases.push(use_case(bad, None));
        push(g);

        for g in cases {
            assert!(matches!(
                format_graph(&g),
                Err(HclError::InvalidIdentifier { .. })
            ));
        }
    }

    #[test]
    fn all_kinds_with_safe_ids_round_trip() {
        let mut g = Graph::new();
        g.hardware_profiles.push(hardware("hw_1"));
        g.deployments.push(deployment("dep_1", "model_1", "hw_1"));
        g.routing_policies.push(routing("route_1", "dep_1"));
        g.data_classes.push(data_class("dc_1"));
        g.governance_rules.push(governance("rule_1", "dc_1"));
        g.budgets.push(budget("bud_1", Some("tenant_1")));
        g.use_cases.push(use_case("uc_1", Some("dc_1")));
        let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
        assert_eq!(g, reparsed);
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

    // --- amicu-planning#119: HCL secret safety ---

    #[test]
    fn literal_in_secret_designated_field_is_rejected() {
        let src = r#"
            resource "credential" "openai" {
              api_key = "sk-abcdefghijklmnopqrstuvwxyz123456"
            }
        "#;
        let err = parse_graph(src).expect_err("a literal api_key must be rejected");
        let HclError::SecretLiteral { code, field, .. } = &err else {
            panic!("expected HclError::SecretLiteral, got {err:?}");
        };
        assert_eq!(code.to_string(), "AMICU-POLICY-004");
        assert_eq!(field, "api_key");
        assert_eq!(err.code().unwrap().to_string(), "AMICU-POLICY-004");
    }

    #[test]
    fn literal_in_env_map_value_is_rejected() {
        let src = r#"
            resource "credential" "runtime" {
              env {
                HF_TOKEN = "hf_abcdefghijklmnopqrstuvwxyz"
              }
            }
        "#;
        assert!(matches!(
            parse_graph(src),
            Err(HclError::SecretLiteral { .. })
        ));
    }

    #[test]
    fn secret_reference_is_accepted() {
        let src = r#"
            resource "credential" "openai" {
              api_key = secret("vault://kv/openai#api_key")
              token   = secret("vault://kv/openai#token")
              env {
                HF_TOKEN = secret("vault://kv/hf#token")
              }
            }
        "#;
        let graph = parse_graph(src).unwrap();
        let cred = &graph.credentials[0];
        assert_eq!(cred.id.as_str(), "openai");
        assert_eq!(
            cred.api_key,
            Some(amicu_core::secret::SecretRef::new(
                "vault://kv/openai#api_key"
            ))
        );
        assert_eq!(
            cred.token,
            Some(amicu_core::secret::SecretRef::new(
                "vault://kv/openai#token"
            ))
        );
        assert!(cred.password.is_none());
        assert_eq!(
            cred.env.get("HF_TOKEN"),
            Some(&amicu_core::secret::SecretRef::new("vault://kv/hf#token"))
        );
    }

    #[test]
    fn credential_round_trips_through_format_graph() {
        let src = r#"
            resource "credential" "openai" {
              api_key = secret("vault://kv/openai#api_key")
              env {
                HF_TOKEN = secret("vault://kv/hf#token")
              }
            }
        "#;
        let graph = parse_graph(src).unwrap();
        let reparsed = parse_graph(&format_graph(&graph).unwrap()).unwrap();
        assert_eq!(graph, reparsed);
    }

    #[test]
    fn free_text_field_with_high_confidence_secret_warns() {
        let src = "description = \"Remember to rotate AKIAIOSFODNN7EXAMPLE before shipping\"\n";
        let warnings = scan_secret_leaks(src);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code.to_string(), "AMICU-POLICY-005");
        assert_eq!(
            warnings[0].severity,
            amicu_core::diagnostic::Severity::Warning
        );
        // The diagnostic must never echo the matched secret verbatim.
        assert!(!warnings[0].message.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn amicu_allow_comment_suppresses_the_warning() {
        let src = "description = \"Remember to rotate AKIAIOSFODNN7EXAMPLE before shipping\" # amicu:allow\n";
        assert!(scan_secret_leaks(src).is_empty());
    }

    #[test]
    fn normal_free_text_value_does_not_warn() {
        let src = "description = \"Customer PII must stay on EU infrastructure.\"\n";
        assert!(scan_secret_leaks(src).is_empty());
    }

    #[test]
    fn literal_secret_wrapped_in_secret_call_is_still_rejected() {
        // A `secret(...)` call has the right *syntax*, but wrapping a raw
        // secret value in it (rather than an actual reference) must not
        // bypass AMICU-POLICY-004: the argument itself is checked against
        // the same high-confidence shapes the free-text scanner uses.
        let openai = r#"
            resource "credential" "openai" {
              api_key = secret("sk-abcdefghijklmnopqrstuvwxyz123456")
            }
        "#;
        assert!(matches!(
            parse_graph(openai),
            Err(HclError::SecretLiteral { .. })
        ));

        let aws = r#"
            resource "credential" "aws" {
              token = secret("AKIAIOSFODNN7EXAMPLE")
            }
        "#;
        assert!(matches!(
            parse_graph(aws),
            Err(HclError::SecretLiteral { .. })
        ));

        let env_map = r#"
            resource "credential" "runtime" {
              env {
                HF_TOKEN = secret("sk-abcdefghijklmnopqrstuvwxyz123456")
              }
            }
        "#;
        assert!(matches!(
            parse_graph(env_map),
            Err(HclError::SecretLiteral { .. })
        ));

        // Genuine bare-name and URI-shaped references must still work (this
        // is the actual `SecretRef` convention used elsewhere in the repo,
        // e.g. `EnvSecretResolver` resolves by plain env-var name).
        let genuine = r#"
            resource "credential" "openai" {
              api_key = secret("HF_TOKEN")
              token   = secret("vault://kv/openai#token")
            }
        "#;
        assert!(parse_graph(genuine).is_ok());
    }

    #[test]
    fn secret_call_in_free_text_scan_is_not_flagged() {
        // A `secret(...)` reference value is not free text at all (it lives
        // in a secret-designated field), but even scanned as raw text its
        // URI-shaped argument should not trip the entropy heuristic.
        let src = "api_key = secret(\"vault://kv/openai#api_key\")\n";
        assert!(scan_secret_leaks(src).is_empty());
    }
}
