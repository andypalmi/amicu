//! Amicu core: IDs, units, canonical resource domain types, the
//! versioned canonical resource graph, and the diagnostic contract.
//!
//! Rust types are the source of truth; canonical JSON is their
//! serialization. See `GRAPH_VERSIONING.md` for the schema version and
//! migration policy.

pub mod adapter;
pub mod diagnostic;
pub mod estimate;
pub mod graph;
pub mod id;
pub mod resource;
pub mod secret;
pub mod units;

pub use estimate::{Assumption, Confidence, EstimateRange, Provenance};
pub use graph::{GRAPH_SCHEMA_VERSION, Graph};
pub use units::ByteSize;

#[cfg(test)]
mod tests {
    use crate::graph::{GRAPH_SCHEMA_VERSION, Graph};
    use crate::id::{
        DeploymentId, HardwareProfileId, ModelId, ResourceKind, RoutingPolicyId, TenantId,
        UseCaseId,
    };
    use crate::resource::{
        Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Budget, BudgetPeriod, DataClass,
        Deployment, GgufVariant, GovernanceRule, HardwareProfile, LlamaCppSettings, Modality,
        Model, ModelSource, Money, OptimizeFor, Price, Priority, Quantization, Quota, Region,
        Residency, RoutingPolicy, RoutingTarget, RuleEffect, Runtime, Sensitivity, SourceRegistry,
        SpeculativeSettings, Tenant, UseCase,
    };
    use crate::units::ByteSize;

    fn sample_model() -> Model {
        Model {
            id: ModelId::new("support_chat"),
            source: ModelSource {
                registry: SourceRegistry::HuggingFace,
                reference: "TheBloke/Mistral-7B-Instruct-GGUF".into(),
            },
            architecture: "llama".into(),
            modalities: vec![Modality::Text],
            artifact: Artifact {
                format: ArtifactFormat::Gguf,
                quantization: Some(Quantization::Q4KM),
            },
            context_length: Some(32_768),
            variants: vec![GgufVariant {
                quantization: Quantization::Q4KM,
                file: "mistral-7b-instruct.Q4_K_M.gguf".into(),
                size: Some(ByteSize::from_bytes(4 * 1024 * 1024 * 1024)),
                sha256: None,
            }],
            n_layers: None,
            n_kv_heads: None,
            n_kv_layers: None,
            head_dim: None,
        }
    }

    fn sample_hardware() -> HardwareProfile {
        HardwareProfile {
            id: HardwareProfileId::new("a100_80gb"),
            provider: "aws".into(),
            region: Some(Region::from_code("us-east-1")),
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::Gpu,
                model: "a100".into(),
                count: 1,
                memory: "80GiB".parse().unwrap(),
                memory_bandwidth_gbps: None,
            }),
            cpu: None,
            memory: "128GiB".parse().unwrap(),
            interconnect: None,
            price: Some(Price {
                per_hour: Money::new(1.5, "USD"),
            }),
        }
    }

    fn sample_deployment() -> Deployment {
        Deployment {
            scaling: None,
            memory_check: crate::resource::MemoryCheck::default(),
            api_key: None,
            id: DeploymentId::new("support_chat_prod"),
            model: ModelId::new("support_chat"),
            hardware_profile: HardwareProfileId::new("a100_80gb"),
            runtime: Runtime::LlamaCpp(LlamaCppSettings {
                context_length: 8192,
                port: 8080,
                n_gpu_layers: -1,
                ..Default::default()
            }),
        }
    }

    /// Small graph mirroring the PRD example: model + hardware + deployment.
    fn sample_graph() -> Graph {
        Graph {
            models: vec![sample_model()],
            hardware_profiles: vec![sample_hardware()],
            deployments: vec![sample_deployment()],
            ..Graph::new()
        }
    }

    #[test]
    fn graph_new_has_current_version() {
        assert_eq!(Graph::new().schema_version, GRAPH_SCHEMA_VERSION);
    }

    #[test]
    fn valid_graph_has_no_missing_references() {
        assert_eq!(sample_graph().missing_references(), Vec::new());
    }

    #[test]
    fn dangling_deployment_reference_is_detected() {
        let mut graph = sample_graph();
        graph.deployments[0].model = ModelId::new("does_not_exist");
        let missing = graph.missing_references();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].to_id, "does_not_exist");
    }

    // --- the speculative draft model is a real
    // second model edge, not a path string ------------------------------

    /// `sample_graph()` whose deployment drafts with `draft`.
    fn graph_with_speculative_draft(draft: &str) -> Graph {
        let mut graph = sample_graph();
        let Runtime::LlamaCpp(s) = &mut graph.deployments[0].runtime else {
            unreachable!("sample_deployment builds a llama_cpp runtime");
        };
        s.speculative = Some(SpeculativeSettings {
            model: ModelId::new(draft),
            n_max: Some(8),
            n_min: None,
            p_min: None,
            n_gpu_layers: None,
        });
        graph
    }

    #[test]
    fn dangling_speculative_draft_reference_is_detected() {
        let graph = graph_with_speculative_draft("does_not_exist");
        let missing = graph.missing_references();
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert_eq!(missing[0].to_id, "does_not_exist");
        assert_eq!(missing[0].to_kind, ResourceKind::Model);
        assert_eq!(missing[0].from_kind, ResourceKind::Deployment);
        assert_eq!(missing[0].from_id, "support_chat_prod");
    }

    #[test]
    fn resolvable_speculative_draft_reference_is_not_missing() {
        let mut graph = graph_with_speculative_draft("support_chat_draft");
        let mut draft = sample_model();
        draft.id = ModelId::new("support_chat_draft");
        graph.models.push(draft);
        assert!(
            graph.missing_references().is_empty(),
            "{:?}",
            graph.missing_references()
        );
    }

    #[test]
    fn a_deployment_without_a_speculative_block_adds_no_edge() {
        // Absent block = today's behaviour: only the model + hardware edges.
        assert_eq!(sample_graph().missing_references(), Vec::new());
        let mut graph = sample_graph();
        graph.hardware_profiles.clear();
        assert_eq!(graph.missing_references().len(), 1);
    }

    #[test]
    fn graph_serde_round_trip() {
        let graph = sample_graph();
        let json = serde_json::to_string_pretty(&graph).unwrap();
        let back: Graph = serde_json::from_str(&json).unwrap();
        assert_eq!(graph, back);
        assert!(json.contains("\"runtime\": \"llama_cpp\""));
    }

    fn assert_json_round_trip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(value, &back);
    }

    #[test]
    fn every_resource_kind_round_trips() {
        assert_json_round_trip(&sample_model());
        assert_json_round_trip(&sample_hardware());
        assert_json_round_trip(&sample_deployment());

        assert_json_round_trip(&RoutingPolicy {
            id: RoutingPolicyId::new("chat"),
            aliases: vec!["support".into()],
            targets: vec![RoutingTarget {
                deployment: DeploymentId::new("support_chat_prod"),
                weight: 100,
            }],
            failover: None,
            optimize_for: Some(OptimizeFor::Latency),
        });

        assert_json_round_trip(&DataClass {
            id: crate::id::DataClassId::new("pii"),
            sensitivity: Sensitivity::Restricted,
            residency: vec![Residency::Eu],
            allow_external_providers: false,
            handling: vec!["no_logging".into()],
        });

        assert_json_round_trip(&GovernanceRule {
            id: crate::id::GovernanceRuleId::new("eu_only"),
            description: Some("Keep PII in-region".into()),
            effect: RuleEffect::Deny,
            data_classes: vec![crate::id::DataClassId::new("pii")],
            allowed_providers: vec![],
            allowed_regions: vec!["eu-west-1".into()],
        });

        assert_json_round_trip(&Budget {
            id: crate::id::BudgetId::new("team_a"),
            tenant: Some(TenantId::new("team_a")),
            limit: Money::new(1000.0, "USD"),
            period: BudgetPeriod::Monthly,
            alert_thresholds: vec![0.5, 0.9],
        });

        assert_json_round_trip(&Tenant {
            id: TenantId::new("team_a"),
            display_name: Some("Team A".into()),
            quota: Some(Quota {
                max_requests_per_minute: Some(600),
                max_concurrent: Some(20),
            }),
            labels: [("cost_center".to_string(), "cc-42".to_string())]
                .into_iter()
                .collect(),
        });

        assert_json_round_trip(&UseCase {
            id: UseCaseId::new("customer_support"),
            description: None,
            modalities: vec![Modality::Text],
            min_context_length: Some(8192),
            max_latency_ms: Some(2000),
            required_data_class: Some(crate::id::DataClassId::new("pii")),
            priority: Some(Priority::High),
        });
    }
}
