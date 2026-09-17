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
pub use secret_scan::{scan_runtime_env_secrets, scan_secret_leaks};
pub use ser::format_graph;

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::ByteSize;
    use amicu_core::graph::Graph;
    use amicu_core::resource::{
        Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Budget, BudgetPeriod, DataClass,
        Deployment, FlashAttention, GovernanceRule, HardwareProfile, KvCacheType, LlamaCppSettings,
        Model, ModelSource, Money, Quantization, RoutingPolicy, RoutingTarget, RuleEffect, Runtime,
        Sensitivity, SourceRegistry, SpeculativeSettings, Tenant, UseCase, VllmKvCacheDtype,
        VllmSettings,
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
            n_layers: None,
            n_kv_heads: None,
            head_dim: None,
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

    fn graph_with_hardware(bandwidth_gbps: Option<u32>) -> Graph {
        let mut g = Graph::new();
        g.hardware_profiles.push(HardwareProfile {
            id: "rtx_5090".into(),
            provider: "on_prem".to_string(),
            region: None,
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::Gpu,
                model: "rtx_5090".to_string(),
                count: 1,
                memory: "32GiB".parse().unwrap(),
                memory_bandwidth_gbps: bandwidth_gbps,
            }),
            cpu: None,
            memory: "64GiB".parse().unwrap(),
            interconnect: None,
            price: None,
        });
        g
    }

    #[test]
    fn accelerator_bandwidth_gbps_round_trips_when_present() {
        let g = graph_with_hardware(Some(1792));
        let formatted = format_graph(&g).unwrap();
        assert!(
            formatted.contains("bandwidth_gbps"),
            "formatted HCL should carry the bandwidth_gbps attribute:\n{formatted}"
        );
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(reparsed, g);
        assert_eq!(
            reparsed.hardware_profiles[0]
                .accelerator
                .as_ref()
                .unwrap()
                .memory_bandwidth_gbps,
            Some(1792)
        );
    }

    #[test]
    fn accelerator_bandwidth_gbps_omitted_when_absent() {
        let g = graph_with_hardware(None);
        let formatted = format_graph(&g).unwrap();
        assert!(
            !formatted.contains("bandwidth_gbps"),
            "formatted HCL should omit bandwidth_gbps when None:\n{formatted}"
        );
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(reparsed, g);
        assert_eq!(
            reparsed.hardware_profiles[0]
                .accelerator
                .as_ref()
                .unwrap()
                .memory_bandwidth_gbps,
            None
        );
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
                ..Default::default()
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

    // --- amicu-planning#150: runtime escape hatch (extra_args / env) ---

    #[test]
    fn llama_cpp_extra_args_and_env_parse() {
        let src = r#"
            resource "model" "m" {
              source {
                registry  = "hugging_face"
                reference = "org/model"
              }
              architecture = "llama"
              artifact { format = "gguf" }
            }
            resource "hardware_profile" "hw" {
              provider = "local"
              memory   = "48GiB"
            }
            resource "deployment" "dep" {
              model    = model.m.id
              hardware = hardware_profile.hw.id
              runtime  = "llama_cpp"
              llama_cpp {
                context_length = 8192
                port           = 8000
                n_gpu_layers   = -1
                extra_args = ["--mmproj", "/models/mmproj.gguf", "--temp", "0.6"]
                env {
                  GGML_CUDA_DISABLE_GRAPHS = "1"
                }
              }
            }
        "#;
        let graph = parse_graph(src).unwrap();
        let Runtime::LlamaCpp(s) = &graph.deployments[0].runtime else {
            panic!("expected llama_cpp runtime");
        };
        assert_eq!(
            s.extra_args,
            vec!["--mmproj", "/models/mmproj.gguf", "--temp", "0.6"]
        );
        assert_eq!(
            s.env.get("GGML_CUDA_DISABLE_GRAPHS").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn vllm_extra_args_and_env_parse() {
        let src = r#"
            resource "model" "m" {
              source {
                registry  = "hugging_face"
                reference = "org/model"
              }
              architecture = "llama"
              artifact { format = "safetensors" }
            }
            resource "hardware_profile" "hw" {
              provider = "local"
              memory   = "48GiB"
            }
            resource "deployment" "dep" {
              model    = model.m.id
              hardware = hardware_profile.hw.id
              runtime  = "vllm"
              vllm {
                context_length = 32768
                port           = 8000
                extra_args = ["--swap-space", "4", "--enforce-eager"]
                env {
                  NCCL_DEBUG = "WARN"
                }
              }
            }
        "#;
        let graph = parse_graph(src).unwrap();
        let Runtime::Vllm(s) = &graph.deployments[0].runtime else {
            panic!("expected vllm runtime");
        };
        assert_eq!(s.extra_args, vec!["--swap-space", "4", "--enforce-eager"]);
        assert_eq!(s.env.get("NCCL_DEBUG").map(String::as_str), Some("WARN"));
    }

    fn llama_cpp_deployment_with(extra_args: Vec<String>, env: BTreeMap<String, String>) -> Graph {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                extra_args,
                env,
                ..Default::default()
            }),
        });
        g
    }

    fn vllm_deployment_with(extra_args: Vec<String>, env: BTreeMap<String, String>) -> Graph {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::Vllm(amicu_core::resource::VllmSettings {
                context_length: 32768,
                port: 8000,
                tensor_parallel_size: None,
                gpu_memory_utilization: None,
                extra_args,
                env,
                ..Default::default()
            }),
        });
        g
    }

    #[test]
    fn llama_cpp_extra_args_and_env_round_trip() {
        let mut env = BTreeMap::new();
        env.insert("GGML_CUDA_DISABLE_GRAPHS".to_string(), "1".to_string());
        let g = llama_cpp_deployment_with(
            vec!["--mmproj".to_string(), "/models/mmproj.gguf".to_string()],
            env,
        );
        let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
        assert_eq!(g, reparsed);
    }

    #[test]
    fn vllm_extra_args_and_env_round_trip() {
        let mut env = BTreeMap::new();
        env.insert("NCCL_DEBUG".to_string(), "WARN".to_string());
        let g = vllm_deployment_with(vec!["--enforce-eager".to_string()], env);
        let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
        assert_eq!(g, reparsed);
    }

    // --- amicu-planning#151: typed sampling/KV-cache/batching/flash-attn/
    // parallel-slots fields, plus optional model shape metadata ------------

    #[test]
    fn llama_cpp_sampling_kv_cache_batching_flash_attn_fields_round_trip() {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 32768,
                port: 8000,
                n_gpu_layers: -1,
                temperature: Some(0.6),
                top_p: Some(0.9),
                top_k: Some(40),
                min_p: Some(0.05),
                repeat_penalty: Some(1.1),
                cache_type_k: Some(KvCacheType::Q8_0),
                cache_type_v: Some(KvCacheType::F16),
                batch_size: Some(2048),
                ubatch_size: Some(512),
                parallel_slots: Some(4),
                flash_attention: Some(FlashAttention::Auto),
                ..Default::default()
            }),
        });
        let formatted = format_graph(&g).unwrap();
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed, "round trip through:\n{formatted}");
    }

    #[test]
    fn llama_cpp_fields_are_independently_optional() {
        // Setting only one field must not force the others to appear.
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                temperature: Some(0.6),
                ..Default::default()
            }),
        });
        let formatted = format_graph(&g).unwrap();
        assert!(formatted.contains("temperature"));
        assert!(!formatted.contains("top_p"));
        assert!(!formatted.contains("cache_type_k"));
        assert!(!formatted.contains("flash_attention"));
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed);
    }

    #[test]
    fn every_kv_cache_type_round_trips() {
        for ty in [
            KvCacheType::F16,
            KvCacheType::BF16,
            KvCacheType::Q8_0,
            KvCacheType::Q4_0,
            KvCacheType::Q4_1,
            KvCacheType::Q5_0,
            KvCacheType::Q5_1,
            KvCacheType::Iq4Nl,
        ] {
            let mut g = Graph::new();
            g.deployments.push(Deployment {
                id: "dep".into(),
                model: "phantom_model".into(),
                hardware_profile: "phantom_hw".into(),
                runtime: Runtime::LlamaCpp(LlamaCppSettings {
                    context_length: 8192,
                    port: 8000,
                    n_gpu_layers: -1,
                    cache_type_k: Some(ty),
                    ..Default::default()
                }),
            });
            let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
            assert_eq!(g, reparsed, "round trip failed for {ty:?}");
        }
    }

    #[test]
    fn every_flash_attention_value_round_trips() {
        for value in [
            FlashAttention::On,
            FlashAttention::Off,
            FlashAttention::Auto,
        ] {
            let mut g = Graph::new();
            g.deployments.push(Deployment {
                id: "dep".into(),
                model: "phantom_model".into(),
                hardware_profile: "phantom_hw".into(),
                runtime: Runtime::LlamaCpp(LlamaCppSettings {
                    context_length: 8192,
                    port: 8000,
                    n_gpu_layers: -1,
                    flash_attention: Some(value),
                    ..Default::default()
                }),
            });
            let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
            assert_eq!(g, reparsed, "round trip failed for {value:?}");
        }
    }

    #[test]
    fn unknown_cache_type_value_is_a_schema_error() {
        let src = r#"
            resource "model" "m" {
              source {
                registry  = "hugging_face"
                reference = "org/model"
              }
              architecture = "llama"
              artifact { format = "gguf" }
            }
            resource "hardware_profile" "hw" {
              provider = "local"
              memory   = "48GiB"
            }
            resource "deployment" "dep" {
              model    = model.m.id
              hardware = hardware_profile.hw.id
              runtime  = "llama_cpp"
              llama_cpp {
                context_length = 8192
                port           = 8000
                n_gpu_layers   = -1
                cache_type_k   = "not_a_real_dtype"
              }
            }
        "#;
        assert!(parse_graph(src).is_err());
    }

    #[test]
    fn unknown_flash_attention_value_is_a_schema_error() {
        let src = r#"
            resource "model" "m" {
              source {
                registry  = "hugging_face"
                reference = "org/model"
              }
              architecture = "llama"
              artifact { format = "gguf" }
            }
            resource "hardware_profile" "hw" {
              provider = "local"
              memory   = "48GiB"
            }
            resource "deployment" "dep" {
              model    = model.m.id
              hardware = hardware_profile.hw.id
              runtime  = "llama_cpp"
              llama_cpp {
                context_length  = 8192
                port            = 8000
                n_gpu_layers    = -1
                flash_attention = "maybe"
              }
            }
        "#;
        assert!(parse_graph(src).is_err());
    }

    // --- amicu-planning#151 PR B: chat template, jinja, reasoning_format,
    // build, GgufVariant.sha256 ---------------------------------------------

    #[test]
    fn llama_cpp_chat_template_jinja_reasoning_format_build_fields_round_trip() {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 32768,
                port: 8000,
                n_gpu_layers: -1,
                chat_template_file: Some("/models/chat.jinja".to_string()),
                jinja: Some(true),
                reasoning_format: Some(amicu_core::resource::ReasoningFormat::Deepseek),
                build: Some("b4500".to_string()),
                ..Default::default()
            }),
        });
        let formatted = format_graph(&g).unwrap();
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed, "round trip through:\n{formatted}");
    }

    #[test]
    fn llama_cpp_chat_template_inline_field_round_trips() {
        // chat_template (inline Jinja text) and chat_template_file are
        // independently settable - only setting *both* is a capability
        // conflict (AMICU-CAP-005), enforced at the adapter layer, not here.
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                chat_template: Some("{{ messages }}".to_string()),
                ..Default::default()
            }),
        });
        let formatted = format_graph(&g).unwrap();
        assert!(formatted.contains("chat_template"));
        assert!(!formatted.contains("chat_template_file"));
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed);
    }

    #[test]
    fn llama_cpp_chat_template_jinja_reasoning_format_build_fields_are_independently_optional() {
        // Setting only `jinja` must not force chat_template/reasoning_format/
        // build to appear.
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                jinja: Some(true),
                ..Default::default()
            }),
        });
        let formatted = format_graph(&g).unwrap();
        assert!(formatted.contains("jinja"));
        assert!(!formatted.contains("chat_template"));
        assert!(!formatted.contains("reasoning_format"));
        assert!(!formatted.contains("build"));
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed);
    }

    #[test]
    fn every_reasoning_format_value_round_trips() {
        for value in [
            amicu_core::resource::ReasoningFormat::None,
            amicu_core::resource::ReasoningFormat::Deepseek,
            amicu_core::resource::ReasoningFormat::DeepseekLegacy,
            amicu_core::resource::ReasoningFormat::Auto,
        ] {
            let mut g = Graph::new();
            g.deployments.push(Deployment {
                id: "dep".into(),
                model: "phantom_model".into(),
                hardware_profile: "phantom_hw".into(),
                runtime: Runtime::LlamaCpp(LlamaCppSettings {
                    context_length: 8192,
                    port: 8000,
                    n_gpu_layers: -1,
                    reasoning_format: Some(value),
                    ..Default::default()
                }),
            });
            let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
            assert_eq!(g, reparsed, "round trip failed for {value:?}");
        }
    }

    #[test]
    fn unknown_reasoning_format_value_is_a_schema_error() {
        let src = r#"
            resource "model" "m" {
              source {
                registry  = "hugging_face"
                reference = "org/model"
              }
              architecture = "llama"
              artifact { format = "gguf" }
            }
            resource "hardware_profile" "hw" {
              provider = "local"
              memory   = "48GiB"
            }
            resource "deployment" "dep" {
              model    = model.m.id
              hardware = hardware_profile.hw.id
              runtime  = "llama_cpp"
              llama_cpp {
                context_length   = 8192
                port             = 8000
                n_gpu_layers     = -1
                reasoning_format = "not_a_real_format"
              }
            }
        "#;
        assert!(parse_graph(src).is_err());
    }

    #[test]
    fn both_chat_template_fields_set_still_parses_and_round_trips() {
        // The mutual-exclusivity check (AMICU-CAP-005) is an adapter-level
        // capability diagnostic, not an HCL schema error - the DSL layer
        // parses and round-trips it like any other pair of fields.
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                chat_template: Some("{{ messages }}".to_string()),
                chat_template_file: Some("/models/chat.jinja".to_string()),
                ..Default::default()
            }),
        });
        let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
        assert_eq!(g, reparsed);
    }

    #[test]
    fn malformed_build_is_a_schema_error() {
        for bad in ["", "b 4500", "b4500\t"] {
            let src = format!(
                r#"
                resource "model" "m" {{
                  source {{
                    registry  = "hugging_face"
                    reference = "org/model"
                  }}
                  architecture = "llama"
                  artifact {{ format = "gguf" }}
                }}
                resource "hardware_profile" "hw" {{
                  provider = "local"
                  memory   = "48GiB"
                }}
                resource "deployment" "dep" {{
                  model    = model.m.id
                  hardware = hardware_profile.hw.id
                  runtime  = "llama_cpp"
                  llama_cpp {{
                    context_length = 8192
                    port           = 8000
                    n_gpu_layers   = -1
                    build          = "{bad}"
                  }}
                }}
            "#
            );
            assert!(
                parse_graph(&src).is_err(),
                "build = {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn build_round_trips() {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                build: Some("b4500".to_string()),
                ..Default::default()
            }),
        });
        let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
        assert_eq!(g, reparsed);
    }

    #[test]
    fn gguf_variant_sha256_round_trips_and_is_normalized_to_lowercase() {
        let mut g = graph_with_model("m");
        g.models[0]
            .variants
            .push(amicu_core::resource::GgufVariant {
                quantization: Quantization::Q4KM,
                file: "m.Q4_K_M.gguf".to_string(),
                size: None,
                sha256: Some("A".repeat(64)),
            });
        let formatted = format_graph(&g).unwrap();
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(
            reparsed.models[0].variants[0].sha256.as_deref(),
            Some("a".repeat(64).as_str())
        );
    }

    #[test]
    fn gguf_variant_sha256_is_optional() {
        let g = graph_with_model("m");
        assert!(g.models[0].variants.is_empty());
    }

    #[test]
    fn malformed_sha256_is_a_schema_error() {
        let too_short = "a".repeat(63);
        let non_hex = "g".repeat(64);
        let too_long = "a".repeat(65);
        for bad in [
            "",
            "abc",
            too_short.as_str(),
            non_hex.as_str(),
            too_long.as_str(),
        ] {
            let src = format!(
                r#"
                resource "model" "m" {{
                  source {{
                    registry  = "hugging_face"
                    reference = "org/model"
                  }}
                  architecture = "llama"
                  artifact {{ format = "gguf" }}
                  variant {{
                    quantization = "q4_k_m"
                    file         = "m.gguf"
                    sha256       = "{bad}"
                  }}
                }}
            "#
            );
            assert!(
                parse_graph(&src).is_err(),
                "sha256 = {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn vllm_kv_cache_dtype_and_batching_fields_round_trip() {
        for dtype in [
            VllmKvCacheDtype::Auto,
            VllmKvCacheDtype::Fp8,
            VllmKvCacheDtype::Fp8E5M2,
            VllmKvCacheDtype::Fp8E4M3,
        ] {
            let mut g = Graph::new();
            g.deployments.push(Deployment {
                id: "dep".into(),
                model: "phantom_model".into(),
                hardware_profile: "phantom_hw".into(),
                runtime: Runtime::Vllm(VllmSettings {
                    context_length: 32768,
                    port: 8000,
                    tensor_parallel_size: Some(2),
                    gpu_memory_utilization: Some(0.9),
                    kv_cache_dtype: Some(dtype),
                    max_num_seqs: Some(4),
                    max_num_batched_tokens: Some(8192),
                    ..Default::default()
                }),
            });
            let formatted = format_graph(&g).unwrap();
            let reparsed = parse_graph(&formatted).unwrap();
            assert_eq!(g, reparsed, "round trip failed for {dtype:?}:\n{formatted}");
        }
    }

    #[test]
    fn model_shape_metadata_round_trips() {
        let mut g = graph_with_model("m");
        g.models[0].n_layers = Some(32);
        g.models[0].n_kv_heads = Some(8);
        g.models[0].head_dim = Some(128);
        let formatted = format_graph(&g).unwrap();
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed, "round trip through:\n{formatted}");
    }

    #[test]
    fn model_shape_metadata_is_optional_and_absent_by_default() {
        let g = graph_with_model("m");
        assert_eq!(g.models[0].n_layers, None);
        let formatted = format_graph(&g).unwrap();
        assert!(!formatted.contains("n_layers"));
        assert!(!formatted.contains("n_kv_heads"));
        assert!(!formatted.contains("head_dim"));
    }

    #[test]
    fn extra_args_declaration_order_is_preserved_through_round_trip() {
        // extra_args order is argv order - it must never get reordered
        // (e.g. by accidentally routing through a sorted map).
        let args = vec![
            "--mmproj".to_string(),
            "/m.gguf".to_string(),
            "--temp".to_string(),
            "0.6".to_string(),
            "--jinja".to_string(),
        ];
        let g = llama_cpp_deployment_with(args.clone(), BTreeMap::new());
        let reparsed = parse_graph(&format_graph(&g).unwrap()).unwrap();
        let Runtime::LlamaCpp(s) = &reparsed.deployments[0].runtime else {
            panic!("expected llama_cpp runtime");
        };
        assert_eq!(s.extra_args, args);
    }

    #[test]
    fn non_identifier_runtime_env_key_is_rejected_not_corrupted() {
        for bad_key in ["cost.center", "cost center", ""] {
            let mut env = BTreeMap::new();
            env.insert(bad_key.to_string(), "value".to_string());
            let g = llama_cpp_deployment_with(Vec::new(), env);
            assert!(
                matches!(format_graph(&g), Err(HclError::InvalidIdentifier { .. })),
                "env key {bad_key:?} should be rejected"
            );
        }
    }

    #[test]
    fn env_secret_suspected_warns_on_secret_looking_value() {
        let mut env = BTreeMap::new();
        env.insert("HF_TOKEN".to_string(), "AKIAIOSFODNN7EXAMPLE".to_string());
        let g = llama_cpp_deployment_with(Vec::new(), env);
        let warnings = scan_runtime_env_secrets(&g);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code.to_string(), "AMICU-POLICY-006");
        assert_eq!(
            warnings[0].severity,
            amicu_core::diagnostic::Severity::Warning
        );
        assert_eq!(warnings[0].path.as_deref(), Some("llama_cpp.env.HF_TOKEN"));
        assert!(!warnings[0].message.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn env_secret_suspected_does_not_warn_on_ordinary_value() {
        let mut env = BTreeMap::new();
        env.insert("GGML_CUDA_DISABLE_GRAPHS".to_string(), "1".to_string());
        let g = llama_cpp_deployment_with(Vec::new(), env);
        assert!(scan_runtime_env_secrets(&g).is_empty());
    }

    // --- amicu-planning#151 PR C: the typed `speculative {}` block ---------

    /// A one-deployment graph whose llama.cpp block carries `spec`.
    fn speculative_graph(spec: SpeculativeSettings) -> Graph {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                speculative: Some(spec),
                ..Default::default()
            }),
        });
        g
    }

    fn draft_spec(model: &str) -> SpeculativeSettings {
        SpeculativeSettings {
            model: model.into(),
            n_max: None,
            n_min: None,
            p_min: None,
            n_gpu_layers: None,
        }
    }

    #[test]
    fn llama_cpp_speculative_block_round_trips() {
        let g = speculative_graph(SpeculativeSettings {
            model: "phantom_draft".into(),
            n_max: Some(8),
            n_min: Some(2),
            p_min: Some(0.75),
            n_gpu_layers: Some(99),
        });
        let formatted = format_graph(&g).unwrap();
        assert!(formatted.contains("speculative"));
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed, "round trip through:\n{formatted}");
    }

    #[test]
    fn speculative_fields_round_trip_individually() {
        let variations = [
            SpeculativeSettings {
                n_max: Some(16),
                ..draft_spec("phantom_draft")
            },
            SpeculativeSettings {
                n_min: Some(1),
                ..draft_spec("phantom_draft")
            },
            SpeculativeSettings {
                p_min: Some(0.4),
                ..draft_spec("phantom_draft")
            },
            SpeculativeSettings {
                n_gpu_layers: Some(-1),
                ..draft_spec("phantom_draft")
            },
            draft_spec("phantom_draft"),
        ];
        for spec in variations {
            let g = speculative_graph(spec.clone());
            let formatted = format_graph(&g).unwrap();
            let reparsed = parse_graph(&formatted).unwrap();
            assert_eq!(g, reparsed, "round trip failed for {spec:?}:\n{formatted}");
        }
    }

    #[test]
    fn speculative_fields_are_independently_optional() {
        // The draft `model` reference is the only required attribute.
        let g = speculative_graph(SpeculativeSettings {
            n_max: Some(8),
            ..draft_spec("phantom_draft")
        });
        let formatted = format_graph(&g).unwrap();
        assert!(formatted.contains("n_max"));
        assert!(!formatted.contains("n_min"));
        assert!(!formatted.contains("p_min"));
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed);
    }

    #[test]
    fn absent_speculative_block_emits_nothing() {
        let g = llama_cpp_deployment_with(Vec::new(), BTreeMap::new());
        let formatted = format_graph(&g).unwrap();
        assert!(!formatted.contains("speculative"));
        assert_eq!(g, parse_graph(&formatted).unwrap());
    }

    #[test]
    fn speculative_draft_model_parses_as_a_reference() {
        let src = r#"
            resource "model" "m" {
              source {
                registry  = "hugging_face"
                reference = "org/model"
              }
              architecture = "llama"
              artifact { format = "gguf" }
            }
            resource "model" "draft" {
              source {
                registry  = "hugging_face"
                reference = "org/draft"
              }
              architecture = "llama"
              artifact { format = "gguf" }
            }
            resource "hardware_profile" "hw" {
              provider = "local"
              memory   = "48GiB"
            }
            resource "deployment" "dep" {
              model    = model.m.id
              hardware = hardware_profile.hw.id
              runtime  = "llama_cpp"
              llama_cpp {
                context_length = 8192
                port           = 8000
                n_gpu_layers   = -1
                speculative {
                  model = model.draft.id
                  n_max = 8
                }
              }
            }
        "#;
        let graph = parse_graph(src).unwrap();
        let Runtime::LlamaCpp(s) = &graph.deployments[0].runtime else {
            panic!("expected llama_cpp runtime");
        };
        let spec = s.speculative.as_ref().expect("speculative block parsed");
        assert_eq!(spec.model.as_str(), "draft");
        assert_eq!(spec.n_max, Some(8));
        // A real edge, resolved against the graph's models.
        assert!(graph.missing_references().is_empty());
    }

    #[test]
    fn speculative_draft_model_that_is_not_a_reference_is_a_schema_error() {
        // The draft model goes through the same `reference` helper the
        // deployment's own `model`/`hardware` attributes use: a traversal or a
        // bare id string, never an arbitrary expression.
        let src = r#"
            resource "model" "m" {
              source {
                registry  = "hugging_face"
                reference = "org/model"
              }
              architecture = "llama"
              artifact { format = "gguf" }
            }
            resource "hardware_profile" "hw" {
              provider = "local"
              memory   = "48GiB"
            }
            resource "deployment" "dep" {
              model    = model.m.id
              hardware = hardware_profile.hw.id
              runtime  = "llama_cpp"
              llama_cpp {
                context_length = 8192
                port           = 8000
                n_gpu_layers   = -1
                speculative {
                  model = 42
                }
              }
            }
        "#;
        assert!(parse_graph(src).is_err());
    }

    #[test]
    fn speculative_block_without_a_model_is_a_schema_error() {
        let src = r#"
            resource "model" "m" {
              source {
                registry  = "hugging_face"
                reference = "org/model"
              }
              architecture = "llama"
              artifact { format = "gguf" }
            }
            resource "hardware_profile" "hw" {
              provider = "local"
              memory   = "48GiB"
            }
            resource "deployment" "dep" {
              model    = model.m.id
              hardware = hardware_profile.hw.id
              runtime  = "llama_cpp"
              llama_cpp {
                context_length = 8192
                port           = 8000
                n_gpu_layers   = -1
                speculative {
                  n_max = 8
                }
              }
            }
        "#;
        assert!(parse_graph(src).is_err());
    }

    #[test]
    fn non_identifier_speculative_draft_reference_is_rejected_not_corrupted() {
        for bad in ["draft.with.dots", "has space", "1leading", ""] {
            let g = speculative_graph(draft_spec(bad));
            assert!(
                matches!(format_graph(&g), Err(HclError::InvalidIdentifier { .. })),
                "draft reference {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_draft_model_equal_to_the_primary_model_still_parses_and_round_trips() {
        // AMICU-CAP-007 is an adapter-level capability diagnostic, not an HCL
        // schema error - the DSL layer round-trips it like any other edge.
        let g = speculative_graph(draft_spec("phantom_model"));
        assert_eq!(g, parse_graph(&format_graph(&g).unwrap()).unwrap());
    }
}
