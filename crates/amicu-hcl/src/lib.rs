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
        Model, ModelSource, Money, MultimodalSettings, Quantization, RoutingPolicy, RoutingTarget,
        RuleEffect, Runtime, Sensitivity, SourceRegistry, SpeculativeSettings, Tenant, UseCase,
        VllmKvCacheDtype, VllmSettings,
    };
    use std::collections::BTreeMap;

    const SAMPLE: &str = include_str!("../tests/sample.amicu.hcl");

    #[test]
    fn parses_prd_example_fields() {
        let graph = parse_graph(SAMPLE).unwrap();

        let model = &graph.models[0];
        assert_eq!(model.id.as_str(), "support_chat");
        assert_eq!(model.source.registry, SourceRegistry::HuggingFace);
        assert_eq!(model.source.reference, "unsloth/Qwen3.5-9B-GGUF");
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
        assert_ne!(graph.models, Vec::new());
        assert_ne!(graph.hardware_profiles, Vec::new());
        assert_ne!(graph.deployments, Vec::new());
        assert_ne!(graph.routing_policies, Vec::new());
        assert_ne!(graph.data_classes, Vec::new());
        assert_ne!(graph.governance_rules, Vec::new());
        assert_ne!(graph.budgets, Vec::new());
        assert_ne!(graph.tenants, Vec::new());
        assert_ne!(graph.use_cases, Vec::new());
        assert_eq!(graph.missing_references(), Vec::new());
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
            n_kv_layers: None,
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
            runpod: None,
            gcp: None,
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

    fn gpu_profile(model: &str, memory: Option<&str>) -> String {
        let memory = memory.map_or(String::new(), |m| format!("    memory = \"{m}\"\n"));
        format!(
            "resource \"hardware_profile\" \"g\" {{\n  provider = \"runpod\"\n  \
             accelerator {{\n    kind  = \"gpu\"\n    model = \"{model}\"\n    count = 1\n\
             {memory}  }}\n  memory = \"64GiB\"\n}}\n"
        )
    }

    fn with_runpod_block(block: &str) -> String {
        gpu_profile("l4", None).replace(
            "  memory = \"64GiB\"\n",
            &format!("  memory = \"64GiB\"\n{block}"),
        )
    }

    #[test]
    fn runpod_block_parses_and_round_trips() {
        use amicu_core::resource::{RunpodCloud, RunpodMode, RunpodSettings};
        let g = parse_graph(&with_runpod_block(
            "  runpod {\n    mode  = \"pod\"\n    cloud = \"community\"\n  }\n",
        ))
        .unwrap();
        assert_eq!(
            g.hardware_profiles[0].runpod,
            Some(RunpodSettings {
                mode: RunpodMode::Pod,
                cloud: Some(RunpodCloud::Community),
                ..Default::default()
            })
        );
        assert_eq!(parse_graph(&format_graph(&g).unwrap()).unwrap(), g);

        let g = parse_graph(&with_runpod_block(
            "  runpod {\n    network_volume = \"vol_abc\"\n    flashboot      = true\n  }\n",
        ))
        .unwrap();
        let r = g.hardware_profiles[0].runpod.clone().unwrap();
        assert_eq!(r.network_volume.as_deref(), Some("vol_abc"));
        assert_eq!(r.flashboot, Some(true));
        assert_eq!(parse_graph(&format_graph(&g).unwrap()).unwrap(), g);

        let g = parse_graph(&with_runpod_block("  runpod {}\n")).unwrap();
        assert_eq!(g.hardware_profiles[0].runpod_mode(), RunpodMode::Serverless);
        assert_eq!(parse_graph(&format_graph(&g).unwrap()).unwrap(), g);

        let g = parse_graph(&gpu_profile("l4", None)).unwrap();
        assert_eq!(g.hardware_profiles[0].runpod, None);
        assert!(!format_graph(&g).unwrap().contains("runpod {"));
    }

    #[test]
    fn runpod_block_rejects_unknown_values_and_fields() {
        let err = parse_graph(&with_runpod_block("  runpod {\n    mode = \"spot\"\n  }\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("mode"), "{err}");
        let err = parse_graph(&with_runpod_block("  runpod {\n    gpus = 2\n  }\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("gpus"), "{err}");
        let err = parse_graph(&with_runpod_block(
            "  runpod {\n    flashboot = \"yes\"\n  }\n",
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("flashboot"), "{err}");
    }

    #[test]
    fn gcp_block_parses_and_round_trips() {
        use amicu_core::resource::{GcpBackend, GcpProvisioning};
        let g = parse_graph(&with_runpod_block(
            "  gcp {\n    project = \"my-proj\"\n    zone = \"europe-west4-a\"\n    \
             machine_type = \"n1-standard-4\"\n    provisioning = \"flex_start\"\n    \
             max_run_duration = \"2h\"\n    idle_shutdown = \"30m\"\n    boot_disk_gb = 100\n    \
             service_account = \"vm@my-proj.iam.gserviceaccount.com\"\n    \
             insecure_metadata_secrets = true\n    \
             allowed_source_cidrs = [\"203.0.113.4/32\"]\n    \
             network = \"vpc-main\"\n    subnetwork = \"gpu\"\n    \
             impersonate_service_account = \"amicu@my-proj.iam.gserviceaccount.com\"\n  }\n",
        ))
        .unwrap();
        let s = g.hardware_profiles[0].gcp.clone().unwrap();
        assert_eq!(s.project.as_deref(), Some("my-proj"));
        assert_eq!(s.backend, GcpBackend::Compute);
        assert_eq!(s.provisioning, GcpProvisioning::FlexStart);
        assert_eq!(s.max_run_duration_secs, Some(7200));
        assert_eq!(s.idle_shutdown_secs, Some(1800));
        assert_eq!(s.allowed_source_cidrs, ["203.0.113.4/32"]);
        assert!(s.insecure_metadata_secrets);
        assert_eq!(s.network.as_deref(), Some("vpc-main"));
        assert_eq!(s.subnetwork.as_deref(), Some("gpu"));
        assert_eq!(parse_graph(&format_graph(&g).unwrap()).unwrap(), g);

        let g = parse_graph(&with_runpod_block(
            "  gcp {\n    project = \"p\"\n    backend = \"cloud_run\"\n  }\n",
        ))
        .unwrap();
        let s = g.hardware_profiles[0].gcp.clone().unwrap();
        assert_eq!(
            (s.backend, s.provisioning),
            (GcpBackend::CloudRun, GcpProvisioning::Spot)
        );
        assert_eq!(s.max_run_secs(), 8 * 3600);
        assert_eq!(parse_graph(&format_graph(&g).unwrap()).unwrap(), g);

        let g = parse_graph(&gpu_profile("l4", None)).unwrap();
        assert_eq!(g.hardware_profiles[0].gcp, None);
        assert!(!format_graph(&g).unwrap().contains("gcp {"));
    }

    #[test]
    fn gcp_block_rejects_unknown_values_and_fields() {
        for bad in [
            "backend = \"gke\"",
            "provisioning = \"preemptible\"",
            "max_run_duration = \"soon\"",
            "idle_shutdown = \"never\"",
            "boot_disk_gb = \"big\"",
            "gpus = 2",
        ] {
            let err = parse_graph(&with_runpod_block(&format!("  gcp {{\n    {bad}\n  }}\n")))
                .unwrap_err()
                .to_string();
            assert!(err.contains(bad.split(' ').next().unwrap()), "{bad}: {err}");
        }
    }

    #[test]
    fn gpu_memory_defaults_from_an_exact_catalog_id() {
        let g = parse_graph(&gpu_profile("h100-nvl-94gb", None)).unwrap();
        let acc = g.hardware_profiles[0].accelerator.as_ref().unwrap();
        assert_eq!(acc.memory, "94GiB".parse().unwrap());
        let g = parse_graph(&gpu_profile("l4", None)).unwrap();
        let acc = g.hardware_profiles[0].accelerator.as_ref().unwrap();
        assert_eq!(acc.memory, "24GiB".parse().unwrap());
        let g = parse_graph(&gpu_profile("a100", Some("80GiB"))).unwrap();
        let acc = g.hardware_profiles[0].accelerator.as_ref().unwrap();
        assert_eq!(acc.memory, "80GiB".parse().unwrap());
    }

    #[test]
    fn gpu_memory_is_required_for_an_ambiguous_family() {
        let err = parse_graph(&gpu_profile("a100", None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing `memory`"), "{err}");
        assert!(err.contains("a100-sxm4-80gb (80 GB)"), "{err}");
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
            runpod: None,
            gcp: None,
        }
    }

    fn deployment(id: &str, model_ref: &str, hw_ref: &str) -> Deployment {
        Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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

    // --- HCL secret safety ---

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
        assert_eq!(scan_secret_leaks(src), Vec::new());
    }

    #[test]
    fn normal_free_text_value_does_not_warn() {
        let src = "description = \"Customer PII must stay on EU infrastructure.\"\n";
        assert_eq!(scan_secret_leaks(src), Vec::new());
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
        assert_eq!(scan_secret_leaks(src), Vec::new());
    }

    // --- runtime escape hatch (extra_args / env) ---

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
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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

    // --- typed sampling/KV-cache/batching/flash-attn/
    // parallel-slots fields, plus optional model shape metadata ------------

    #[test]
    fn llama_cpp_sampling_kv_cache_batching_flash_attn_fields_round_trip() {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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
                scaling: None,
                memory_check: amicu_core::resource::MemoryCheck::default(),
                api_key: None,
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
                scaling: None,
                memory_check: amicu_core::resource::MemoryCheck::default(),
                api_key: None,
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

    // --- chat template, jinja, reasoning_format,
    // build, GgufVariant.sha256 ---------------------------------------------

    #[test]
    fn llama_cpp_chat_template_jinja_reasoning_format_build_fields_round_trip() {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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
                scaling: None,
                memory_check: amicu_core::resource::MemoryCheck::default(),
                api_key: None,
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
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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
        assert_eq!(g.models[0].variants, Vec::new());
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
                scaling: None,
                memory_check: amicu_core::resource::MemoryCheck::default(),
                api_key: None,
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
    fn vllm_enforce_eager_round_trips() {
        // Some(true), Some(false) and None (field absent) must each survive
        // a format -> parse round trip, and None must not emit the attribute
        // at all (see enforce_eager_absent_by_default_omits_attribute).
        for value in [Some(true), Some(false), None] {
            let mut g = Graph::new();
            g.deployments.push(Deployment {
                scaling: None,
                memory_check: amicu_core::resource::MemoryCheck::default(),
                api_key: None,
                id: "dep".into(),
                model: "phantom_model".into(),
                hardware_profile: "phantom_hw".into(),
                runtime: Runtime::Vllm(VllmSettings {
                    context_length: 32768,
                    port: 8000,
                    enforce_eager: value,
                    ..Default::default()
                }),
            });
            let formatted = format_graph(&g).unwrap();
            let reparsed = parse_graph(&formatted).unwrap();
            assert_eq!(g, reparsed, "round trip failed for {value:?}:\n{formatted}");
        }
    }

    #[test]
    fn enforce_eager_absent_by_default_omits_attribute() {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::Vllm(VllmSettings {
                context_length: 32768,
                port: 8000,
                ..Default::default()
            }),
        });
        let formatted = format_graph(&g).unwrap();
        assert!(!formatted.contains("enforce_eager"));
    }

    #[test]
    fn model_shape_metadata_round_trips() {
        let mut g = graph_with_model("m");
        g.models[0].n_layers = Some(32);
        g.models[0].n_kv_heads = Some(8);
        g.models[0].head_dim = Some(128);
        g.models[0].n_kv_layers = Some(8);
        let formatted = format_graph(&g).unwrap();
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed, "round trip through:\n{formatted}");
    }

    fn model_hcl(shape: &str) -> String {
        format!(
            "resource \"model\" \"m\" {{\n  source {{\n    registry = \"hugging_face\"\n    \
             reference = \"org/m\"\n  }}\n  architecture = \"llama\"\n  artifact {{\n    \
             format = \"safetensors\"\n  }}\n{shape}}}\n"
        )
    }

    #[test]
    fn model_shape_rejects_zero_and_oversized_kv_layers() {
        for bad in [
            "  n_layers = 0\n",
            "  n_kv_heads = 0\n",
            "  head_dim = 0\n",
            "  n_kv_layers = 0\n",
            "  n_layers = 8\n  n_kv_layers = 9\n",
        ] {
            assert!(parse_graph(&model_hcl(bad)).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn model_shape_accepts_n_kv_layers_within_bounds_or_alone() {
        for ok in [
            "  n_layers = 8\n  n_kv_layers = 8\n",
            "  n_layers = 48\n  n_kv_layers = 12\n",
            // Alone, it supplies the layer count for the estimate.
            "  n_kv_layers = 12\n",
        ] {
            parse_graph(&model_hcl(ok)).unwrap_or_else(|e| panic!("rejected {ok}: {e}"));
        }
    }

    #[test]
    fn model_shape_metadata_is_optional_and_absent_by_default() {
        let g = graph_with_model("m");
        assert_eq!(g.models[0].n_layers, None);
        let formatted = format_graph(&g).unwrap();
        assert!(!formatted.contains("n_layers"));
        assert!(!formatted.contains("n_kv_heads"));
        assert!(!formatted.contains("head_dim"));
        assert!(!formatted.contains("n_kv_layers"));
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
        assert_eq!(scan_runtime_env_secrets(&g), Vec::new());
    }

    #[test]
    fn runtime_secret_scan_flags_secret_named_env_literal_and_extra_args() {
        let mut env = BTreeMap::new();
        env.insert("MY_API_TOKEN".to_string(), "abc123xyz789".to_string());
        let args = vec![
            "--api-key".to_string(),
            "supersecretvalue".to_string(),
            "--hf-token=hf_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
            "--ctx-size".to_string(),
            "4096".to_string(),
        ];
        let g = llama_cpp_deployment_with(args, env);
        let paths: Vec<_> = scan_runtime_env_secrets(&g)
            .into_iter()
            .map(|d| d.path.unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "llama_cpp.env.MY_API_TOKEN",
                "llama_cpp.extra_args[0]",
                "llama_cpp.extra_args[2]"
            ]
        );
    }

    #[test]
    fn runtime_secret_scan_has_no_false_positives_on_normal_settings() {
        let mut env = BTreeMap::new();
        env.insert("HF_HUB_OFFLINE".to_string(), "1".to_string());
        env.insert("HF_TOKEN_FILE".to_string(), "/run/secrets/hf".to_string());
        env.insert("API_KEY".to_string(), "true".to_string());
        let args = ["--ctx-size", "4096", "--cache-type-k", "q8_0", "--keep=256"]
            .map(String::from)
            .to_vec();
        let g = llama_cpp_deployment_with(args, env);
        assert_eq!(scan_runtime_env_secrets(&g), Vec::new());
    }

    #[test]
    fn runtime_secret_scan_regressions() {
        let args = [
            "--override-kv",
            "tokenizer.ggml.add_bos_token=bool:false",
            "--hf-token",
            "hf_abcdefghijklmnopqrstuvwxyz0123456789",
            "--api-key=${VLLM_KEY}",
            "--api-key",
            "$VLLM_KEY",
        ]
        .map(String::from)
        .to_vec();
        let mut env = BTreeMap::new();
        env.insert("HF_TOKEN".to_string(), "${HF_TOKEN}".to_string());
        let g = llama_cpp_deployment_with(args, env);
        let paths: Vec<_> = scan_runtime_env_secrets(&g)
            .into_iter()
            .map(|d| d.path.unwrap())
            .collect();
        assert_eq!(paths, ["llama_cpp.extra_args[2]"]);
    }

    // --- multimodal projector block ---------------

    #[test]
    fn llama_cpp_multimodal_block_round_trips() {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                multimodal: Some(MultimodalSettings {
                    projector: Some("/models/support_chat/mmproj-f16.gguf".to_string()),
                    offload: Some(true),
                }),
                ..Default::default()
            }),
        });
        let formatted = format_graph(&g).unwrap();
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed, "round trip through:\n{formatted}");
    }

    #[test]
    fn llama_cpp_multimodal_fields_are_independently_optional() {
        // projector alone, with no `multimodal` block at all when unset.
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                multimodal: Some(MultimodalSettings {
                    projector: Some("/models/mmproj-f16.gguf".to_string()),
                    offload: None,
                }),
                ..Default::default()
            }),
        });
        let formatted = format_graph(&g).unwrap();
        assert!(formatted.contains("projector"));
        assert!(!formatted.contains("offload"));
        let reparsed = parse_graph(&formatted).unwrap();
        assert_eq!(g, reparsed);

        // offload alone.
        let mut g2 = Graph::new();
        g2.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                multimodal: Some(MultimodalSettings {
                    projector: None,
                    offload: Some(false),
                }),
                ..Default::default()
            }),
        });
        let formatted2 = format_graph(&g2).unwrap();
        assert!(formatted2.contains("offload"));
        assert!(!formatted2.contains("projector"));
        let reparsed2 = parse_graph(&formatted2).unwrap();
        assert_eq!(g2, reparsed2);

        // Unset entirely - no `multimodal` block at all.
        let mut g3 = Graph::new();
        g3.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
            id: "dep".into(),
            model: "phantom_model".into(),
            hardware_profile: "phantom_hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8000,
                n_gpu_layers: -1,
                ..Default::default()
            }),
        });
        let formatted3 = format_graph(&g3).unwrap();
        assert!(!formatted3.contains("multimodal"));
        let reparsed3 = parse_graph(&formatted3).unwrap();
        assert_eq!(g3, reparsed3);
    }

    // A `multimodal { ... }` block only ever gets parsed inside a `llama_cpp
    // {}` settings block (`amicu_hcl::de::deployment`) - `VllmSettings` has
    // no field for it, so putting one inside `vllm {}` is rejected
    // structurally at parse time rather than silently dropped, per
    // the scope decision (an `AMICU-CAP-*` diagnostic
    // was considered and skipped: the parser already refuses this shape, so
    // there is nothing left for a `validate`-time diagnostic to catch).
    #[test]
    fn multimodal_block_on_vllm_is_a_schema_error() {
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
                context_length = 8192
                port           = 8000
                multimodal {
                  projector = "/models/mmproj-f16.gguf"
                }
              }
            }
        "#;
        assert!(
            matches!(parse_graph(src), Err(HclError::Schema(_))),
            "expected a schema error for `multimodal` inside `vllm {{}}`"
        );
    }

    // --- the provider-agnostic `scaling {}` block ------

    fn scaling_src(scaling_body: &str) -> String {
        format!(
            r#"
            resource "model" "m" {{
              source {{
                registry  = "hugging_face"
                reference = "org/model"
              }}
              architecture = "llama"
              artifact {{ format = "safetensors" }}
            }}
            resource "hardware_profile" "hw" {{
              provider = "modal"
              memory   = "48GiB"
            }}
            resource "deployment" "dep" {{
              model    = model.m.id
              hardware = hardware_profile.hw.id
              runtime  = "vllm"
              vllm {{
                context_length = 8192
                port           = 8000
              }}
              {scaling_body}
            }}
        "#
        )
    }

    #[test]
    fn memory_check_defaults_to_enforce_and_warn_round_trips() {
        use amicu_core::resource::MemoryCheck;
        let none = parse_graph(&scaling_src("")).unwrap();
        assert_eq!(none.deployments[0].memory_check, MemoryCheck::Enforce);
        assert!(!format_graph(&none).unwrap().contains("memory_check"));

        let g = parse_graph(&scaling_src(r#"memory_check = "warn""#)).unwrap();
        assert_eq!(g.deployments[0].memory_check, MemoryCheck::Warn);
        let formatted = format_graph(&g).unwrap();
        assert!(formatted.contains("memory_check"));
        assert_eq!(parse_graph(&formatted).unwrap(), g);

        let e = parse_graph(&scaling_src(r#"memory_check = "enforce""#)).unwrap();
        assert_eq!(e.deployments[0].memory_check, MemoryCheck::Enforce);
    }

    #[test]
    fn memory_check_rejects_unknown_values() {
        for bad in [r#"memory_check = "off""#, "memory_check = true"] {
            assert!(
                matches!(parse_graph(&scaling_src(bad)), Err(HclError::Schema(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn scaling_block_parses_and_round_trips() {
        let src = scaling_src(
            r#"scaling {
                 min_replicas       = 1
                 max_replicas       = 3
                 target_concurrency = 16
                 scaledown_window   = "15m"
               }"#,
        );
        let g = parse_graph(&src).unwrap();
        let sc = g.deployments[0].scaling.clone().expect("scaling parsed");
        assert_eq!(sc.min_replicas, Some(1));
        assert_eq!(sc.max_replicas, Some(3));
        assert_eq!(sc.target_concurrency, Some(16));
        assert_eq!(sc.scaledown_window_secs, Some(900));
        let formatted = format_graph(&g).unwrap();
        assert!(formatted.contains("scaledown_window"));
        assert!(formatted.contains("\"15m\""));
        assert_eq!(parse_graph(&formatted).unwrap(), g);
    }

    #[test]
    fn scaling_fields_are_independently_optional_and_absent_block_emits_nothing() {
        let g = parse_graph(&scaling_src("scaling { max_replicas = 1 }")).unwrap();
        let sc = g.deployments[0].scaling.clone().unwrap();
        assert_eq!(sc.max_replicas, Some(1));
        assert_eq!(sc.min_replicas, None);
        assert_eq!(parse_graph(&format_graph(&g).unwrap()).unwrap(), g);

        let none = parse_graph(&scaling_src("")).unwrap();
        assert!(none.deployments[0].scaling.is_none());
        assert!(!format_graph(&none).unwrap().contains("scaling"));
    }

    #[test]
    fn scaling_window_accepts_seconds_and_hours_and_rejects_garbage() {
        let g = parse_graph(&scaling_src("scaling { scaledown_window = \"90s\" }")).unwrap();
        assert_eq!(
            g.deployments[0]
                .scaling
                .as_ref()
                .unwrap()
                .scaledown_window_secs,
            Some(90)
        );
        let g = parse_graph(&scaling_src("scaling { scaledown_window = \"1h\" }")).unwrap();
        assert_eq!(
            g.deployments[0]
                .scaling
                .as_ref()
                .unwrap()
                .scaledown_window_secs,
            Some(3600)
        );
        for bad in ["15", "m", "1.5m", "-5m", "15 minutes", ""] {
            let src = scaling_src(&format!("scaling {{ scaledown_window = \"{bad}\" }}"));
            assert!(
                matches!(parse_graph(&src), Err(HclError::Schema(_))),
                "`{bad}` should be a schema error"
            );
        }
    }

    // --- per-deployment endpoint `api_key` -------------

    #[test]
    fn deployment_api_key_is_a_secret_reference_and_round_trips() {
        let g = parse_graph(&scaling_src("api_key = secret(\"CHAT_API_KEY\")")).unwrap();
        assert_eq!(
            g.deployments[0].api_key,
            Some(amicu_core::secret::SecretRef::new("CHAT_API_KEY"))
        );
        let formatted = format_graph(&g).unwrap();
        assert!(
            formatted.contains("api_key = secret(\"CHAT_API_KEY\")"),
            "{formatted}"
        );
        assert_eq!(parse_graph(&formatted).unwrap(), g);

        let none = parse_graph(&scaling_src("")).unwrap();
        assert!(none.deployments[0].api_key.is_none());
        assert!(!format_graph(&none).unwrap().contains("api_key"));
    }

    #[test]
    fn deployment_api_key_literal_is_rejected_with_policy_004() {
        for body in [
            "api_key = \"sk-abcdefghijklmnopqrstuvwxyz123456\"",
            "api_key = \"hunter2\"",
            "api_key = 12345",
            // A well-known secret shape wrapped in secret() is still a literal.
            "api_key = secret(\"sk-abcdefghijklmnopqrstuvwxyz123456\")",
        ] {
            let err = parse_graph(&scaling_src(body)).expect_err(body);
            assert!(
                matches!(err, HclError::SecretLiteral { .. }),
                "{body}: {err}"
            );
            assert_eq!(err.code().unwrap().to_string(), "AMICU-POLICY-004");
            // The rejection never echoes the offending value.
            assert!(!err.to_string().contains("hunter2"), "{err}");
            assert!(!err.to_string().contains("sk-abcdef"), "{err}");
        }
    }

    // --- the typed `speculative {}` block ---------

    /// A one-deployment graph whose llama.cpp block carries `spec`.
    fn speculative_graph(spec: SpeculativeSettings) -> Graph {
        let mut g = Graph::new();
        g.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
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
        assert_eq!(graph.missing_references(), Vec::new());
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

    // --- unknown attributes / blocks are rejected ---

    /// Parse `src`, expecting `AMICU-SCHEMA-003`; returns
    /// `(context, kind, name, suggestion)`.
    fn unknown(src: &str) -> (String, &'static str, String, Option<String>) {
        let err = parse_graph(src).expect_err("an unknown field must be rejected");
        assert_eq!(
            err.code().map(|c| c.to_string()).as_deref(),
            Some("AMICU-SCHEMA-003"),
            "{err}"
        );
        let HclError::UnknownField {
            context,
            kind,
            name,
            suggestion,
            ..
        } = err
        else {
            panic!("expected HclError::UnknownField, got {err:?}");
        };
        (context, kind, name, suggestion)
    }

    fn res(kind: &str, name: &str, body: &str) -> String {
        {
            // Allow compact `block { a = 1\nb = 2 }` literals in the cases.
            let body = body.replace("{ ", "{\n").replace(" }", "\n}");
            format!("resource \"{kind}\" \"{name}\" {{\n{body}\n}}\n")
        }
    }

    const MODEL_BODY: &str = r#"
        source {
          registry = "hugging_face"
          reference = "a/b"
        }
        architecture = "qwen35"
        modalities = ["text"]
        artifact {
          format = "gguf"
        }
    "#;

    fn deployment_src(runtime: &str, settings: &str) -> String {
        res(
            "deployment",
            "d",
            &format!(
                "model = model.m.id\nhardware = hardware_profile.h.id\nruntime = \"{runtime}\"\n{settings}"
            ),
        )
    }

    #[test]
    fn unknown_attribute_in_each_top_level_resource_is_rejected() {
        let cases: Vec<(&str, String, &str, &str)> = vec![
            (
                "model",
                format!("{MODEL_BODY}\nmodalites = []"),
                "model h",
                "modalites",
            ),
            (
                "hardware_profile",
                "provider = \"aws\"\nmemory = \"1GiB\"\nbogus = 1".to_string(),
                "hardware_profile h",
                "bogus",
            ),
            (
                "routing_policy",
                "aliases = []\noptimise_for = \"cost\"".to_string(),
                "routing_policy h",
                "optimise_for",
            ),
            (
                "data_class",
                "sensitivity = \"public\"\nresidence = [\"eu\"]".to_string(),
                "data_class h",
                "residence",
            ),
            (
                "governance_rule",
                "effect = \"allow\"\nbogus = 1".to_string(),
                "governance_rule h",
                "bogus",
            ),
            (
                "budget",
                "period = \"monthly\"\nlimit { amount = 1.0\ncurrency = \"USD\" }\nbogus = 1"
                    .to_string(),
                "budget h",
                "bogus",
            ),
            ("tenant", "bogus = 1".to_string(), "tenant h", "bogus"),
            (
                "use_case",
                "min_context_lenght = 1".to_string(),
                "use_case h",
                "min_context_lenght",
            ),
            (
                "credential",
                "api_keys = 1".to_string(),
                "credential h",
                "api_keys",
            ),
        ];
        for (kind, body, ctx, name) in cases {
            let (c, k, n, _) = unknown(&res(kind, "h", &body));
            assert_eq!(
                (c.as_str(), k, n.as_str()),
                (ctx, "attribute", name),
                "{kind}"
            );
        }
    }

    #[test]
    fn unknown_attribute_in_nested_blocks_is_rejected() {
        let model = |extra: &str| res("model", "m", &format!("{MODEL_BODY}\n{extra}"));
        // `source` / `artifact` / `variant` of a model.
        let (c, _, n, _) = unknown(&res(
            "model",
            "m",
            &MODEL_BODY.replace(
                "reference = \"a/b\"",
                "reference = \"a/b\"\nrevision = \"x\"",
            ),
        ));
        assert_eq!((c.as_str(), n.as_str()), ("model m.source", "revision"));
        let (c, _, n, _) = unknown(&res(
            "model",
            "m",
            &MODEL_BODY.replace("format = \"gguf\"", "format = \"gguf\"\nbits = 4"),
        ));
        assert_eq!((c.as_str(), n.as_str()), ("model m.artifact", "bits"));
        let (c, _, n, _) = unknown(&model(
            "variant { quantization = \"q4_k_m\"\nfile = \"f\"\nsise = \"1GiB\" }",
        ));
        assert_eq!((c.as_str(), n.as_str()), ("model m.variant", "sise"));

        let hw = |extra: &str| {
            res(
                "hardware_profile",
                "h",
                &format!("provider = \"aws\"\nmemory = \"1GiB\"\n{extra}"),
            )
        };
        let (c, _, n, _) = unknown(&hw(
            "accelerator { kind = \"gpu\"\nmodel = \"a\"\ncount = 1\nmemory = \"1GiB\"\nvendor = \"x\" }",
        ));
        assert_eq!(
            (c.as_str(), n.as_str()),
            ("hardware_profile h.accelerator", "vendor")
        );
        let (c, _, n, _) = unknown(&hw("cpu { model = \"a\"\ncores = 1\nthreads = 2 }"));
        assert_eq!(
            (c.as_str(), n.as_str()),
            ("hardware_profile h.cpu", "threads")
        );
        let (c, _, n, _) = unknown(&hw(
            "price { currency = \"USD\"\nper_hour { amount = 1.0\ncurrency = \"USD\" } }",
        ));
        assert_eq!(
            (c.as_str(), n.as_str()),
            ("hardware_profile h.price", "currency")
        );
        let (c, _, n, _) = unknown(&hw(
            "price { per_hour { amount = 1.0\ncurrency = \"USD\"\nunit = \"h\" } }",
        ));
        assert_eq!(
            (c.as_str(), n.as_str()),
            ("hardware_profile h.price.per_hour", "unit")
        );

        let rp = |extra: &str| res("routing_policy", "r", extra);
        let (c, _, n, _) = unknown(&rp(
            "target { deployment = deployment.d.id\nweight = 1\nbias = 2 }",
        ));
        assert_eq!(
            (c.as_str(), n.as_str()),
            ("routing_policy r.target", "bias")
        );
        let (c, _, n, _) = unknown(&rp("failover { enabled = true\nretries = 2 }"));
        assert_eq!(
            (c.as_str(), n.as_str()),
            ("routing_policy r.failover", "retries")
        );

        let (c, _, n, _) = unknown(&res(
            "budget",
            "b",
            "period = \"monthly\"\nlimit { amount = 1.0\ncurrency = \"USD\"\ncap = 1 }",
        ));
        assert_eq!((c.as_str(), n.as_str()), ("budget b.limit", "cap"));
        let (c, _, n, _) = unknown(&res(
            "tenant",
            "t",
            "quota { max_concurrent = 1\nburst = 2 }",
        ));
        assert_eq!((c.as_str(), n.as_str()), ("tenant t.quota", "burst"));
    }

    #[test]
    fn unknown_attribute_in_runtime_blocks_is_rejected_with_suggestion() {
        let (c, k, n, sug) = unknown(&deployment_src(
            "llama_cpp",
            "llama_cpp {\ncontext_length = 1\nport = 1\nn_gpu_layers = 1\ncontex_length = 8192\n}",
        ));
        assert_eq!(
            (c.as_str(), k, n.as_str()),
            ("deployment d.llama_cpp", "attribute", "contex_length")
        );
        assert_eq!(sug.as_deref(), Some("context_length"));

        let (c, _, n, _) = unknown(&deployment_src(
            "llama_cpp",
            "llama_cpp {\ncontext_length = 1\nport = 1\nn_gpu_layers = 1\nspeculative {\nmodel = model.m.id\nctx_size = 4096\n}\n}",
        ));
        assert_eq!(
            (c.as_str(), n.as_str()),
            ("deployment d.llama_cpp.speculative", "ctx_size")
        );

        let (c, _, n, _) = unknown(&deployment_src(
            "llama_cpp",
            "llama_cpp {\ncontext_length = 1\nport = 1\nn_gpu_layers = 1\nmultimodal {\nprojektor = \"p\"\n}\n}",
        ));
        assert_eq!(
            (c.as_str(), n.as_str()),
            ("deployment d.llama_cpp.multimodal", "projektor")
        );

        let (c, _, n, sug) = unknown(&deployment_src(
            "vllm",
            "vllm {\ncontext_length = 1\nport = 1\ntensor_parallel_sizes = 2\n}",
        ));
        assert_eq!(
            (c.as_str(), n.as_str()),
            ("deployment d.vllm", "tensor_parallel_sizes")
        );
        assert_eq!(sug.as_deref(), Some("tensor_parallel_size"));

        // Top level of the deployment itself.
        let (c, _, n, _) = unknown(&deployment_src(
            "vllm",
            "vllm {\ncontext_length = 1\nport = 1\n}\nreplicas = 2",
        ));
        assert_eq!((c.as_str(), n.as_str()), ("deployment d", "replicas"));
    }

    #[test]
    fn unknown_block_is_rejected() {
        let (c, k, n, _) = unknown(&res("model", "m", &format!("{MODEL_BODY}\nextras {{}}")));
        assert_eq!((c.as_str(), k, n.as_str()), ("model m", "block", "extras"));
        // The wrong runtime's settings block is not read, so it is unknown.
        let (c, k, n, _) = unknown(&deployment_src(
            "llama_cpp",
            "llama_cpp {\ncontext_length = 1\nport = 1\nn_gpu_layers = 1\n}\nvllm {}",
        ));
        assert_eq!(
            (c.as_str(), k, n.as_str()),
            ("deployment d", "block", "vllm")
        );
        // `target`, not `targets`.
        let (c, k, n, sug) = unknown(&res("routing_policy", "r", "targets { weight = 1 }"));
        assert_eq!(
            (c.as_str(), k, n.as_str()),
            ("routing_policy r", "block", "targets")
        );
        assert_eq!(sug.as_deref(), Some("target"));
    }

    #[test]
    fn unknown_top_level_attribute_is_rejected() {
        let (c, k, n, _) = unknown("version = 1");
        assert_eq!(
            (c.as_str(), k, n.as_str()),
            ("top level", "attribute", "version")
        );
    }

    #[test]
    fn suggestion_is_omitted_when_nothing_is_close() {
        let (_, _, _, sug) = unknown(&res("tenant", "t", "completely_unrelated = 1"));
        assert_eq!(sug, None);
    }

    #[test]
    fn error_message_names_block_attribute_and_suggestion() {
        let err = parse_graph(&deployment_src(
            "vllm",
            "vllm {\ncontext_length = 1\nport = 1\nenforce_eagr = true\n}",
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "AMICU-SCHEMA-003: deployment d.vllm: unknown attribute `enforce_eagr`; \
             did you mean `enforce_eager`?"
        );
    }

    #[test]
    fn free_form_maps_accept_arbitrary_keys() {
        let src = format!(
            "{}{}{}{}",
            res(
                "tenant",
                "t",
                "labels {\nteam = \"x\"\nanything_goes = \"y\"\n}"
            ),
            deployment_src(
                "llama_cpp",
                "llama_cpp {\ncontext_length = 1\nport = 1\nn_gpu_layers = 1\nenv {\nFOO_BAR = \"1\"\n}\n}"
            ),
            deployment_src(
                "vllm",
                "vllm {\ncontext_length = 1\nport = 1\nenv {\nVLLM_X = \"1\"\n}\n}"
            ),
            res(
                "credential",
                "c",
                "env {\nHF_TOKEN = secret(\"vault://kv/hf\")\n}"
            ),
        );
        parse_graph(&src).expect("free-form maps are exempt");
    }

    #[test]
    fn misspelled_required_attribute_reports_the_near_miss() {
        let err = parse_graph(&deployment_src(
            "vllm",
            "vllm {\ncontex_length = 1\nport = 1\n}",
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid resource: deployment d.vllm: missing `context_length` \
             (found unknown `contex_length`; did you mean `context_length`?)"
        );
    }

    #[test]
    fn duplicate_singleton_block_is_rejected() {
        let err = parse_graph(&res(
            "hardware_profile",
            "h",
            "provider = \"aws\"\nmemory = \"1GiB\"\ncpu { model = \"a\"\ncores = 1 }\ncpu { model = \"b\"\ncores = 2\nbogus = 1 }",
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid resource: hardware_profile h: duplicate `cpu` block (only one is allowed)"
        );
    }

    #[test]
    fn labelled_nested_block_is_rejected() {
        let err = parse_graph(&res(
            "routing_policy",
            "r",
            "target \"x\" { deployment = deployment.d.id\nweight = 1 }",
        ))
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid resource: routing_policy r: `target` block takes no labels"
        );
    }

    #[test]
    fn short_keys_are_not_suggested_across_first_letter() {
        let (_, _, _, sug) = unknown(&res(
            "hardware_profile",
            "h",
            "provider = \"aws\"\nmemory = \"1GiB\"\ngpu { model = \"a\"\ncores = 1 }",
        ));
        assert_eq!(sug, None);
    }

    fn collect_md(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                collect_md(&p, out);
            } else if p.extension().is_some_and(|x| x == "md") {
                out.push(p);
            }
        }
    }

    #[test]
    fn skill_doc_hcl_examples_have_no_unknown_fields() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../amicu-cli/skills");
        let mut files = Vec::new();
        collect_md(&root, &mut files);
        assert!(!files.is_empty(), "no skill docs found at {root:?}");
        let mut checked = 0;
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            for chunk in text.split("```hcl\n").skip(1) {
                let Some((block, _)) = chunk.split_once("```") else {
                    continue;
                };
                if !block.contains("resource") {
                    continue;
                }
                checked += 1;
                // Partial snippets may fail for other reasons; only the
                // unknown-field error is asserted on.
                if let Err(e @ HclError::UnknownField { .. }) = parse_graph(block) {
                    panic!("{}: {e}", f.display());
                }
            }
        }
        assert!(checked > 0, "no hcl examples found");
    }
}
