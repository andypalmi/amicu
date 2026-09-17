//! Guided-wizard presets: a typed catalog of starting points that turn a few
//! use-case answers into a concrete, editable [`Graph`].
//!
//! A [`Preset`] is a gallery card: a name, a tagline, who it fits, and the hard
//! [`PresetConstraint`]s it commits to, plus the requirement defaults and a
//! [`WorkloadEstimate`] shown before anything is generated. [`build_graph`]
//! parametrizes a preset's candidate template with the wizard's
//! [`WizardAnswers`] into a canonical [`Graph`] the user can then edit and
//! validate. Nothing here applies a change to a stack; the output is data.
//!
//! Local Dev Chat, Private EU Production, and Cost-Optimized Batch transcribe
//! the shapes in the `amicu-presets` skill's `preset-catalog.md`. The other
//! six (amicu-planning#149) transcribe the PRD's "Presets and Guided Stack
//! Design" outcome list: Internal Knowledge Chatbot, Customer Support
//! Assistant, Document Extraction Pipeline, Code Assistant Endpoint, API
//! Replacement / Migration, and Custom Stack. Treat every entry as a starting
//! point, not a fixed recipe.

use std::sync::LazyLock;

use amicu_core::estimate::{Confidence, EstimateRange, Provenance};
use amicu_core::graph::Graph;
use amicu_core::id::{DataClassId, GovernanceRuleId, UseCaseId};
use amicu_core::resource::{
    Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Budget, BudgetPeriod, DataClass,
    Deployment, GgufVariant, GovernanceRule, HardwareProfile, LlamaCppSettings, Modality, Model,
    ModelSource, Money, Quantization, Region, Residency, Runtime, Sensitivity, SourceRegistry,
    UseCase, VllmSettings,
};
use amicu_core::units::ByteSize;
use serde::{Deserialize, Serialize};

use crate::{Tier, WorkloadEstimate};

/// EU-resident providers a Private EU Production stack is allowed to place on.
/// Restricting to EU-only providers already guarantees EU residency, so no
/// region-code allowlist is pinned (a code allowlist would fight a user who
/// edits to a different genuine EU region on an allowed provider).
///
/// `pub(crate)` so [`crate::rank`] can reuse it to detect (and reject) a
/// candidate variant that would place on a non-EU-resident provider.
pub(crate) const EU_PROVIDERS: &[&str] = &["hetzner", "ovhcloud", "scaleway", "nebius"];

/// The largest concurrency the wizard accepts, guarding
/// [`gpus_for_concurrency`] against an absurd GPU count.
const MAX_CONCURRENCY: u32 = 100_000;

/// A snake_case preset identifier, e.g. `"local_dev_chat"`.
///
/// Ids are validated to be non-empty, lowercase-ASCII `snake_case` starting
/// with a letter, which also makes them round-trip-safe HCL identifiers.
/// Serializes transparently as the underlying string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PresetId(String);

impl PresetId {
    /// Construct a validated preset id. Returns [`PresetError::InvalidId`] if
    /// `id` is not lowercase `snake_case` starting with a letter.
    pub fn new(id: impl Into<String>) -> Result<Self, PresetError> {
        let id = id.into();
        if is_snake_case_ident(&id) {
            Ok(Self(id))
        } else {
            Err(PresetError::InvalidId(id))
        }
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Construct from a compile-time-known literal that is guaranteed valid, for
    /// the in-code [`catalog`]. Kept private so the validated [`PresetId::new`]
    /// is the only public entry point.
    fn from_known(id: &'static str) -> Self {
        debug_assert!(is_snake_case_ident(id), "catalog id must be snake_case");
        Self(id.to_string())
    }
}

impl std::fmt::Display for PresetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `s` is a non-empty lowercase `snake_case` identifier that starts
/// with a letter and contains only `a..=z`, `0..=9`, and `_`.
fn is_snake_case_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The serving runtime family a preset commits to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFamily {
    /// llama.cpp (GGUF weights, CPU / Apple Silicon friendly).
    LlamaCpp,
    /// vLLM (safetensors weights, NVIDIA GPUs).
    Vllm,
}

/// A hard constraint a preset commits to, shown on its gallery card. These are
/// the non-negotiable properties of the candidate stack, distinct from the soft
/// preferences a preset may express through its defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetConstraint {
    /// Data and compute must stay within EU jurisdictions.
    EuResidencyOnly,
    /// Weights must be quantized (GGUF `q4`/`q5`, or INT4/INT8 safetensors).
    QuantizationRequired,
    /// The stack runs on-device only; nothing leaves first-party infrastructure.
    LocalOnly,
    /// External (third-party) providers are forbidden for this workload's data.
    ExternalProvidersForbidden,
    /// The serving runtime is fixed to this family.
    RuntimeFamily(RuntimeFamily),
}

/// A guided-wizard preset: a gallery card plus the requirement defaults and
/// estimate shown before generation, and enough shape for [`build_graph`] to
/// emit a concrete [`Graph`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    /// Stable snake_case identifier.
    pub id: PresetId,
    /// Human-readable name, e.g. `"Local Dev Chat"`.
    pub name: String,
    /// One-line summary shown under the name.
    pub tagline: String,
    /// Who and what this preset fits, from the catalog's "Fit" line.
    pub fit: String,
    /// The hard constraints this preset commits to.
    pub constraints: Vec<PresetConstraint>,
    /// Requirement defaults pre-filled into the wizard.
    pub requirement_defaults: UseCase,
    /// The estimate shown on the card before any answers are collected.
    pub default_estimate: WorkloadEstimate,
    /// Minimum product tier required to use this preset.
    pub min_tier: Tier,
}

/// Which soft-constraint axis [`crate::rank::rank`] should weight most heavily
/// when scoring and ordering a preset's candidates. Distinct from
/// [`amicu_core::resource::Priority`] (a *use case's* Low/Medium/High
/// importance, carried through to the graph): this names the wizard's ranking
/// axis itself and never appears on a [`amicu_core::resource::UseCase`].
///
/// Absent (`None` on [`WizardAnswers::priority`]) means balanced weighting
/// across every axis - no single preference dominates the ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WizardPriority {
    Cost,
    Latency,
    Quality,
    Privacy,
    Simplicity,
}

/// The requirements the wizard collects, parametrizing a preset's candidate
/// template. UseCase-shaped fields plus concurrency, an optional budget, and
/// an optional ranking [`WizardPriority`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WizardAnswers {
    /// Context length (tokens) the runtime is configured for. Must be > 0.
    pub context_length: u32,
    /// Expected concurrent requests. Drives GPU count / `tensor_parallel_size`.
    /// Must be > 0.
    pub concurrency: u32,
    /// Whether the workload must stay within EU jurisdictions. Always effective
    /// for a preset carrying [`PresetConstraint::EuResidencyOnly`].
    #[serde(default)]
    pub eu_residency_required: bool,
    /// Optional monthly spend ceiling; when set, becomes a [`Budget`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Money>,
    /// Optional latency ceiling (ms) carried into the graph's use case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_latency_ms: Option<u32>,
    /// Which soft-constraint axis to weight most heavily when ranking
    /// candidates ([`crate::rank::rank`]). `None` means balanced weighting.
    /// Ignored by [`build_graph`], which builds a single graph and does no
    /// ranking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<WizardPriority>,
}

/// A failure building a graph from a preset and answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetError {
    /// A preset id was not a valid lowercase `snake_case` identifier.
    InvalidId(String),
    /// No candidate template is wired up for this preset id.
    UnknownPreset(PresetId),
    /// A wizard answer was outside the acceptable range.
    InvalidAnswer(String),
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId(id) => write!(
                f,
                "invalid preset id `{id}`: must be non-empty lowercase snake_case starting with a \
                 letter"
            ),
            Self::UnknownPreset(id) => write!(f, "no candidate template for preset `{id}`"),
            Self::InvalidAnswer(msg) => write!(f, "invalid wizard answer: {msg}"),
        }
    }
}

impl std::error::Error for PresetError {}

/// The in-code preset catalog. See [`catalog`].
static CATALOG: LazyLock<Vec<Preset>> = LazyLock::new(|| {
    vec![
        local_dev_chat(),
        private_eu_production(),
        cost_optimized_batch(),
        internal_knowledge_chatbot(),
        customer_support_assistant(),
        document_extraction_pipeline(),
        code_assistant_endpoint(),
        api_replacement_migration(),
        custom_stack(),
    ]
});

/// The hardcoded preset catalog: the three starting points transcribed from the
/// `amicu-presets` skill's `preset-catalog.md`, plus the remaining PRD-listed
/// presets added by amicu-planning#149.
#[must_use]
pub fn catalog() -> &'static [Preset] {
    &CATALOG
}

/// Build a concrete, editable [`Graph`] from a preset and the wizard's answers.
///
/// Parametrizes the preset's candidate template: context length flows into the
/// runtime settings, concurrency into the GPU count / `tensor_parallel_size`, an
/// EU-residency requirement into a governance rule allowing only EU providers
/// plus a data class pinned to EU residency, and a budget into a [`Budget`]. The
/// result is data; nothing is applied to a stack.
///
/// Residency enforcement rides on the provider allowlist and the data class's
/// EU `residency` (both read by the planner's residency checks). The data
/// class's `allow_external_providers = false` is an advisory intent flag that
/// the current planner does not yet read (a pre-existing engine gap); it records
/// the preset's stance but is not, on its own, the enforced mechanism.
///
/// # Errors
///
/// Returns [`PresetError::InvalidAnswer`] for a zero or over-large context
/// length or concurrency, or a non-finite / negative budget amount; or
/// [`PresetError::UnknownPreset`] if the preset id has no candidate template.
pub fn build_graph(preset: &Preset, answers: &WizardAnswers) -> Result<Graph, PresetError> {
    if answers.context_length == 0 {
        return Err(PresetError::InvalidAnswer(
            "context_length must be greater than 0".to_string(),
        ));
    }
    if answers.concurrency == 0 {
        return Err(PresetError::InvalidAnswer(
            "concurrency must be greater than 0".to_string(),
        ));
    }
    if answers.concurrency > MAX_CONCURRENCY {
        return Err(PresetError::InvalidAnswer(format!(
            "concurrency {} exceeds the maximum of {MAX_CONCURRENCY}",
            answers.concurrency
        )));
    }
    if let Some(budget) = &answers.budget
        && (!budget.amount.is_finite() || budget.amount < 0.0)
    {
        return Err(PresetError::InvalidAnswer(format!(
            "budget amount must be finite and non-negative, got {}",
            budget.amount
        )));
    }

    match preset.id.as_str() {
        "local_dev_chat" => Ok(build_local_dev_chat(preset, answers)),
        "private_eu_production" => Ok(build_private_eu_production(preset, answers)),
        "cost_optimized_batch" => Ok(build_cost_optimized_batch(preset, answers)),
        "internal_knowledge_chatbot" => Ok(build_internal_knowledge_chatbot(preset, answers)),
        "customer_support_assistant" => Ok(build_customer_support_assistant(preset, answers)),
        "document_extraction_pipeline" => Ok(build_document_extraction_pipeline(preset, answers)),
        "code_assistant_endpoint" => Ok(build_code_assistant_endpoint(preset, answers)),
        "api_replacement_migration" => Ok(build_api_replacement_migration(preset, answers)),
        "custom_stack" => Ok(build_custom_stack(preset, answers)),
        _ => Err(PresetError::UnknownPreset(preset.id.clone())),
    }
}

// --- catalog entries --------------------------------------------------------

fn local_dev_chat() -> Preset {
    Preset {
        id: PresetId::from_known("local_dev_chat"),
        name: "Local Dev Chat".to_string(),
        tagline: "On-device experimentation with no cloud spend.".to_string(),
        fit: "On-device experimentation, privacy-sensitive data, no cloud spend.".to_string(),
        constraints: vec![
            PresetConstraint::LocalOnly,
            PresetConstraint::QuantizationRequired,
            PresetConstraint::RuntimeFamily(RuntimeFamily::LlamaCpp),
        ],
        requirement_defaults: UseCase {
            id: UseCaseId::new("local_dev_chat"),
            description: Some("Local chat assistant for development.".to_string()),
            modalities: vec![Modality::Text],
            min_context_length: Some(8192),
            max_latency_ms: None,
            required_data_class: None,
            priority: None,
        },
        // Zero marginal cost; latency depends on the Mac; high confidence.
        default_estimate: estimate(
            Assumed {
                prompt: 512,
                generation: 256,
                concurrency: 1,
            },
            (120.0, 400.0),
            (12.0, 40.0),
            Confidence::High,
        ),
        min_tier: Tier::Community,
    }
}

fn private_eu_production() -> Preset {
    Preset {
        id: PresetId::from_known("private_eu_production"),
        name: "Private EU Production".to_string(),
        tagline: "Confidential data with EU residency and throughput.".to_string(),
        fit: "Confidential data with EU residency and throughput needs.".to_string(),
        constraints: vec![
            PresetConstraint::EuResidencyOnly,
            PresetConstraint::ExternalProvidersForbidden,
            PresetConstraint::RuntimeFamily(RuntimeFamily::Vllm),
        ],
        requirement_defaults: UseCase {
            id: UseCaseId::new("private_eu_production"),
            description: Some("Confidential production serving inside the EU.".to_string()),
            modalities: vec![Modality::Text],
            min_context_length: Some(8192),
            max_latency_ms: Some(2000),
            required_data_class: None,
            priority: None,
        },
        // Latency (TTFT/TPOT) planning figures from public benchmarks; medium
        // confidence pending concurrency data. No cost figure is asserted here:
        // a rental rate is provider-sourced at plan/apply time, not fabricated.
        default_estimate: estimate(
            Assumed {
                prompt: 1024,
                generation: 512,
                concurrency: 8,
            },
            (150.0, 600.0),
            (8.0, 25.0),
            Confidence::Medium,
        ),
        min_tier: Tier::Community,
    }
}

fn cost_optimized_batch() -> Preset {
    Preset {
        id: PresetId::from_known("cost_optimized_batch"),
        name: "Cost-Optimized Batch".to_string(),
        tagline: "Throughput-first, latency-tolerant, lowest cost per token.".to_string(),
        fit: "Throughput-first, latency-tolerant workloads.".to_string(),
        constraints: vec![
            PresetConstraint::QuantizationRequired,
            PresetConstraint::RuntimeFamily(RuntimeFamily::Vllm),
        ],
        requirement_defaults: UseCase {
            id: UseCaseId::new("cost_optimized_batch"),
            description: Some("High-throughput batch generation.".to_string()),
            modalities: vec![Modality::Text],
            min_context_length: Some(4096),
            max_latency_ms: None,
            required_data_class: None,
            priority: None,
        },
        // Lowest cost/token; quality risk from aggressive quantization.
        default_estimate: estimate(
            Assumed {
                prompt: 2048,
                generation: 512,
                concurrency: 16,
            },
            (400.0, 1500.0),
            (15.0, 60.0),
            Confidence::Estimated,
        ),
        min_tier: Tier::Community,
    }
}

fn internal_knowledge_chatbot() -> Preset {
    Preset {
        id: PresetId::from_known("internal_knowledge_chatbot"),
        name: "Internal Knowledge Chatbot".to_string(),
        tagline: "RAG-ready endpoint for chatting over company documents.".to_string(),
        fit: "Internal teams querying company documents; generous context for retrieved \
              passages and steady internal-production throughput."
            .to_string(),
        constraints: vec![PresetConstraint::RuntimeFamily(RuntimeFamily::Vllm)],
        requirement_defaults: UseCase {
            id: UseCaseId::new("internal_knowledge_chatbot"),
            description: Some("Internal RAG chat over company documents.".to_string()),
            modalities: vec![Modality::Text],
            min_context_length: Some(32768),
            max_latency_ms: Some(3000),
            required_data_class: None,
            priority: None,
        },
        // RAG prompts carry retrieved passages; medium confidence pending
        // concurrency data, as with the other production vLLM presets.
        default_estimate: estimate(
            Assumed {
                prompt: 4096,
                generation: 512,
                concurrency: 8,
            },
            (200.0, 700.0),
            (10.0, 30.0),
            Confidence::Medium,
        ),
        min_tier: Tier::Community,
    }
}

fn customer_support_assistant() -> Preset {
    Preset {
        id: PresetId::from_known("customer_support_assistant"),
        name: "Customer Support Assistant".to_string(),
        tagline: "Governed, customer-facing support endpoint with predictable latency.".to_string(),
        fit: "Customer-facing support chat needing quantized weights for cost control and a \
              tight latency ceiling."
            .to_string(),
        constraints: vec![
            PresetConstraint::QuantizationRequired,
            PresetConstraint::RuntimeFamily(RuntimeFamily::Vllm),
        ],
        requirement_defaults: UseCase {
            id: UseCaseId::new("customer_support_assistant"),
            description: Some("Customer-facing support assistant.".to_string()),
            modalities: vec![Modality::Text],
            min_context_length: Some(8192),
            max_latency_ms: Some(1200),
            required_data_class: None,
            priority: None,
        },
        default_estimate: estimate(
            Assumed {
                prompt: 768,
                generation: 256,
                concurrency: 16,
            },
            (150.0, 500.0),
            (8.0, 20.0),
            Confidence::Medium,
        ),
        min_tier: Tier::Community,
    }
}

fn document_extraction_pipeline() -> Preset {
    Preset {
        id: PresetId::from_known("document_extraction_pipeline"),
        name: "Document Extraction Pipeline".to_string(),
        tagline: "Structured extraction and classification over documents, cost-controlled."
            .to_string(),
        fit: "High-volume structured extraction and classification from documents (including \
              scans), optimized for token cost with a quality floor."
            .to_string(),
        constraints: vec![
            PresetConstraint::QuantizationRequired,
            PresetConstraint::RuntimeFamily(RuntimeFamily::Vllm),
        ],
        requirement_defaults: UseCase {
            id: UseCaseId::new("document_extraction_pipeline"),
            description: Some(
                "High-volume structured document extraction and classification.".to_string(),
            ),
            modalities: vec![Modality::Text, Modality::Vision],
            min_context_length: Some(8192),
            max_latency_ms: None,
            required_data_class: None,
            priority: None,
        },
        default_estimate: estimate(
            Assumed {
                prompt: 3072,
                generation: 256,
                concurrency: 16,
            },
            (300.0, 1000.0),
            (12.0, 45.0),
            Confidence::Estimated,
        ),
        min_tier: Tier::Community,
    }
}

fn code_assistant_endpoint() -> Preset {
    Preset {
        id: PresetId::from_known("code_assistant_endpoint"),
        name: "Code Assistant Endpoint".to_string(),
        tagline: "Private endpoint for a coding-specialized model, tuned for low latency."
            .to_string(),
        fit: "Private, tool-calling-capable coding assistant endpoint prioritizing low latency \
              for interactive completions."
            .to_string(),
        constraints: vec![PresetConstraint::RuntimeFamily(RuntimeFamily::Vllm)],
        requirement_defaults: UseCase {
            id: UseCaseId::new("code_assistant_endpoint"),
            description: Some("Private coding-model endpoint.".to_string()),
            modalities: vec![Modality::Text],
            min_context_length: Some(16384),
            max_latency_ms: Some(800),
            required_data_class: None,
            priority: None,
        },
        default_estimate: estimate(
            Assumed {
                prompt: 2048,
                generation: 256,
                concurrency: 8,
            },
            (100.0, 350.0),
            (6.0, 18.0),
            Confidence::Medium,
        ),
        min_tier: Tier::Community,
    }
}

fn api_replacement_migration() -> Preset {
    Preset {
        id: PresetId::from_known("api_replacement_migration"),
        name: "API Replacement / Migration".to_string(),
        tagline: "Self-hosted OpenAI-compatible endpoint to replace a third-party API.".to_string(),
        fit: "Teams migrating off an external OpenAI-compatible API who want a self-hosted \
              drop-in endpoint sized to match existing throughput."
            .to_string(),
        // vLLM serves an OpenAI-compatible HTTP surface natively, which is the
        // honest schema-expressible shape for "replace a third-party API": a
        // self-hosted endpoint, not a routing/proxy config. No `routing_policy`
        // resource is wired into this builder - see the PR description.
        constraints: vec![PresetConstraint::RuntimeFamily(RuntimeFamily::Vllm)],
        requirement_defaults: UseCase {
            id: UseCaseId::new("api_replacement_migration"),
            description: Some(
                "Self-hosted replacement for an external OpenAI-compatible API.".to_string(),
            ),
            modalities: vec![Modality::Text],
            min_context_length: Some(8192),
            max_latency_ms: Some(1500),
            required_data_class: None,
            priority: None,
        },
        default_estimate: estimate(
            Assumed {
                prompt: 1024,
                generation: 512,
                concurrency: 16,
            },
            (150.0, 500.0),
            (8.0, 22.0),
            Confidence::Estimated,
        ),
        min_tier: Tier::Community,
    }
}

fn custom_stack() -> Preset {
    Preset {
        id: PresetId::from_known("custom_stack"),
        name: "Custom Stack".to_string(),
        tagline: "Minimal starting graph, built entirely from your answers.".to_string(),
        fit: "A stack that doesn't fit another preset's shape: a minimal starting graph with no \
              preset opinions, expanded through guided requirement collection."
            .to_string(),
        // Deliberately no hard constraints: "custom stack" per the PRD is the
        // minimal, unopinionated starting point every other preset narrows.
        constraints: Vec::new(),
        requirement_defaults: UseCase {
            id: UseCaseId::new("custom_stack"),
            description: Some(
                "Custom stack, built from guided requirement collection.".to_string(),
            ),
            modalities: vec![Modality::Text],
            min_context_length: Some(4096),
            max_latency_ms: None,
            required_data_class: None,
            priority: None,
        },
        // No preset opinion to anchor an estimate on; a generic single-GPU
        // planning figure until the user's own answers narrow the shape.
        default_estimate: estimate(
            Assumed {
                prompt: 512,
                generation: 256,
                concurrency: 1,
            },
            (100.0, 400.0),
            (10.0, 35.0),
            Confidence::Estimated,
        ),
        min_tier: Tier::Community,
    }
}

/// The assumed prompt/generation/concurrency figures a card's estimate is
/// derived for.
#[derive(Clone, Copy)]
struct Assumed {
    prompt: u32,
    generation: u32,
    concurrency: u32,
}

/// A default estimate for a card: the stated assumptions and the TTFT/TPOT
/// ranges (each a `(low, high)` in ms), provenance a public-benchmark planning
/// figure.
fn estimate(
    assumed: Assumed,
    ttft_ms: (f64, f64),
    tpot_ms: (f64, f64),
    confidence: Confidence,
) -> WorkloadEstimate {
    WorkloadEstimate {
        assumed_prompt_tokens: assumed.prompt,
        assumed_generation_tokens: assumed.generation,
        assumed_concurrency: assumed.concurrency,
        expected_ttft_ms: EstimateRange::new(
            ttft_ms.0,
            ttft_ms.1,
            Provenance::PublicBenchmark,
            confidence,
        ),
        expected_tpot_ms: EstimateRange::new(
            tpot_ms.0,
            tpot_ms.1,
            Provenance::PublicBenchmark,
            confidence,
        ),
    }
}

// --- candidate templates ----------------------------------------------------

fn build_local_dev_chat(preset: &Preset, answers: &WizardAnswers) -> Graph {
    let mut graph = Graph::new();
    let prefix = preset.id.as_str();
    let model_id = format!("{prefix}_model");
    let hw_id = "local_apple".to_string();
    let dc_id = format!("{prefix}_data");

    graph.models.push(Model {
        id: model_id.as_str().into(),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "Qwen/Qwen2.5-7B-Instruct-GGUF".to_string(),
        },
        architecture: "qwen2".to_string(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Gguf,
            quantization: Some(Quantization::Q4KM),
        },
        context_length: Some(answers.context_length),
        // One declared GGUF variant matching the artifact's quantization, so
        // the runtime's memory-fit check (`amicu_engine::report_for`) has a
        // variant to resolve rather than erroring with "no GGUF variant".
        variants: vec![GgufVariant {
            quantization: Quantization::Q4KM,
            file: "qwen2.5-7b-instruct-q4_k_m.gguf".to_string(),
            size: None,
        }],
    });

    graph.hardware_profiles.push(HardwareProfile {
        id: hw_id.as_str().into(),
        provider: "local".to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::AppleSilicon,
            model: "m3_max".to_string(),
            count: 1,
            memory: ByteSize::from_bytes(48 * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(48 * GIB),
        interconnect: None,
        // Zero marginal cost: no rental price.
        price: None,
    });

    graph.deployments.push(Deployment {
        id: format!("{prefix}_deployment").as_str().into(),
        model: model_id.as_str().into(),
        hardware_profile: hw_id.as_str().into(),
        runtime: Runtime::LlamaCpp(LlamaCppSettings {
            context_length: answers.context_length,
            port: 8000,
            // Offload every layer to the accelerator.
            n_gpu_layers: -1,
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    });

    // Local-only: the data never leaves first-party infrastructure.
    graph.data_classes.push(DataClass {
        id: dc_id.as_str().into(),
        sensitivity: Sensitivity::Confidential,
        residency: eu_residency(answers),
        allow_external_providers: false,
        handling: Vec::new(),
    });

    push_use_case(&mut graph, &preset.requirement_defaults, &dc_id, answers);
    maybe_push_eu_governance(&mut graph, preset, answers, &dc_id);
    maybe_push_budget(&mut graph, prefix, answers);

    graph
}

fn build_private_eu_production(preset: &Preset, answers: &WizardAnswers) -> Graph {
    let mut graph = Graph::new();
    let prefix = preset.id.as_str();
    let model_id = format!("{prefix}_model");
    let hw_id = format!("{prefix}_gpu");
    let dc_id = format!("{prefix}_data");
    let gpu_count = gpus_for_concurrency(answers.concurrency);

    graph.models.push(Model {
        id: model_id.as_str().into(),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "meta-llama/Llama-3.1-8B-Instruct".to_string(),
        },
        architecture: "llama".to_string(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Safetensors,
            quantization: None,
        },
        context_length: Some(answers.context_length),
        variants: Vec::new(),
    });

    graph.hardware_profiles.push(HardwareProfile {
        id: hw_id.as_str().into(),
        provider: "hetzner".to_string(),
        // A real hetzner EU region (Falkenstein, DE): `resolve_residency`
        // resolves `fsn1` to Residency::Eu, so the data class's EU residency
        // check is a live comparison rather than a silent no-op. The bare code
        // round-trips through HCL; jurisdiction is resolved from the catalog.
        region: Some(Region::from_code("fsn1")),
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "h100".to_string(),
            count: gpu_count,
            memory: ByteSize::from_bytes(80 * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(180 * GIB),
        interconnect: Some(amicu_core::resource::Interconnect::Nvlink),
        // No price is baked into the emitted HCL: a rental rate is a provider
        // concern resolved from the provider catalog at plan/apply time, not a
        // figure the wizard fabricates into the user's downloaded config. See
        // `amicu_providers::cost::estimate_cost` for the provider-sourced path.
        price: None,
    });

    graph.deployments.push(Deployment {
        id: format!("{prefix}_deployment").as_str().into(),
        model: model_id.as_str().into(),
        hardware_profile: hw_id.as_str().into(),
        runtime: Runtime::Vllm(VllmSettings {
            context_length: answers.context_length,
            port: 8000,
            // Shard across the GPU count derived from concurrency.
            tensor_parallel_size: Some(gpu_count),
            gpu_memory_utilization: Some(0.9),
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    });

    // Confidential, EU-resident, first-party only.
    graph.data_classes.push(DataClass {
        id: dc_id.as_str().into(),
        sensitivity: Sensitivity::Confidential,
        residency: vec![Residency::Eu],
        allow_external_providers: false,
        handling: Vec::new(),
    });

    // EU is a hard constraint here, regardless of the answer flag.
    push_eu_governance(&mut graph, prefix, &dc_id);
    push_use_case(&mut graph, &preset.requirement_defaults, &dc_id, answers);
    maybe_push_budget(&mut graph, prefix, answers);

    graph
}

fn build_cost_optimized_batch(preset: &Preset, answers: &WizardAnswers) -> Graph {
    let mut graph = Graph::new();
    let prefix = preset.id.as_str();
    let model_id = format!("{prefix}_model");
    let hw_id = format!("{prefix}_gpu");
    let dc_id = format!("{prefix}_data");
    let gpu_count = gpus_for_concurrency(answers.concurrency);

    graph.models.push(Model {
        id: model_id.as_str().into(),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "TheBloke/Mistral-7B-Instruct-v0.2-AWQ".to_string(),
        },
        architecture: "mistral".to_string(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Safetensors,
            // Aggressive 4-bit quantization for cost per token.
            quantization: Some(Quantization::Int4),
        },
        context_length: Some(answers.context_length),
        variants: Vec::new(),
    });

    // When the wizard requires EU residency, `maybe_push_eu_governance` below
    // pins a governance rule allowing only `EU_PROVIDERS`. The default
    // "runpod" is not in that set, so the provider must switch to an EU one
    // to keep the resulting graph valid - otherwise a `true` answer here
    // would silently emit a graph `amicu_engine::report_for` rejects.
    let provider = if answers.eu_residency_required {
        "hetzner"
    } else {
        "runpod"
    };

    graph.hardware_profiles.push(HardwareProfile {
        id: hw_id.as_str().into(),
        provider: provider.to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            // Cheaper accelerator for throughput-first, latency-tolerant work.
            model: "a10g".to_string(),
            count: gpu_count,
            memory: ByteSize::from_bytes(24 * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(64 * GIB),
        interconnect: None,
        // No price is baked into the emitted HCL: a rental rate is a provider
        // concern resolved from the provider catalog at plan/apply time, not a
        // figure the wizard fabricates into the user's downloaded config. See
        // `amicu_providers::cost::estimate_cost` for the provider-sourced path.
        price: None,
    });

    graph.deployments.push(Deployment {
        id: format!("{prefix}_deployment").as_str().into(),
        model: model_id.as_str().into(),
        hardware_profile: hw_id.as_str().into(),
        runtime: Runtime::Vllm(VllmSettings {
            context_length: answers.context_length,
            port: 8000,
            // TODO(amicu-planning): mapping concurrency to tensor_parallel_size
            // is a documented approximation. For throughput-first batch work the
            // correct scaling model is replica count, not tensor parallelism;
            // switch this to replicas once `Deployment` gains a replicas field.
            tensor_parallel_size: Some(gpu_count),
            // Pack the KV cache hard: throughput over headroom.
            gpu_memory_utilization: Some(0.95),
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    });

    graph.data_classes.push(DataClass {
        id: dc_id.as_str().into(),
        sensitivity: Sensitivity::Internal,
        residency: eu_residency(answers),
        allow_external_providers: !answers.eu_residency_required,
        handling: Vec::new(),
    });

    push_use_case(&mut graph, &preset.requirement_defaults, &dc_id, answers);
    maybe_push_eu_governance(&mut graph, preset, answers, &dc_id);
    maybe_push_budget(&mut graph, prefix, answers);

    graph
}

fn build_internal_knowledge_chatbot(preset: &Preset, answers: &WizardAnswers) -> Graph {
    let mut graph = Graph::new();
    let prefix = preset.id.as_str();
    let model_id = format!("{prefix}_model");
    let hw_id = format!("{prefix}_gpu");
    let dc_id = format!("{prefix}_data");
    let gpu_count = gpus_for_concurrency(answers.concurrency);

    graph.models.push(Model {
        id: model_id.as_str().into(),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "Qwen/Qwen2.5-14B-Instruct".to_string(),
        },
        architecture: "qwen2".to_string(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Safetensors,
            quantization: None,
        },
        context_length: Some(answers.context_length),
        variants: Vec::new(),
    });

    // See `cost_optimized_batch`'s builder for why the provider switches when
    // EU residency is required: `maybe_push_eu_governance` below pins an
    // EU-only provider allowlist in that case, so the hardware must already be
    // on an EU-resident provider or `report_for` rejects the graph.
    let provider = if answers.eu_residency_required {
        "hetzner"
    } else {
        "runpod"
    };

    graph.hardware_profiles.push(HardwareProfile {
        id: hw_id.as_str().into(),
        provider: provider.to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "a100".to_string(),
            count: gpu_count,
            memory: ByteSize::from_bytes(80 * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(160 * GIB),
        interconnect: None,
        price: None,
    });

    graph.deployments.push(Deployment {
        id: format!("{prefix}_deployment").as_str().into(),
        model: model_id.as_str().into(),
        hardware_profile: hw_id.as_str().into(),
        runtime: Runtime::Vllm(VllmSettings {
            context_length: answers.context_length,
            port: 8000,
            tensor_parallel_size: Some(gpu_count),
            gpu_memory_utilization: Some(0.9),
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    });

    graph.data_classes.push(DataClass {
        id: dc_id.as_str().into(),
        sensitivity: Sensitivity::Internal,
        residency: eu_residency(answers),
        allow_external_providers: !answers.eu_residency_required,
        handling: Vec::new(),
    });

    push_use_case(&mut graph, &preset.requirement_defaults, &dc_id, answers);
    maybe_push_eu_governance(&mut graph, preset, answers, &dc_id);
    maybe_push_budget(&mut graph, prefix, answers);

    graph
}

fn build_customer_support_assistant(preset: &Preset, answers: &WizardAnswers) -> Graph {
    let mut graph = Graph::new();
    let prefix = preset.id.as_str();
    let model_id = format!("{prefix}_model");
    let hw_id = format!("{prefix}_gpu");
    let dc_id = format!("{prefix}_data");
    let gpu_count = gpus_for_concurrency(answers.concurrency);

    graph.models.push(Model {
        id: model_id.as_str().into(),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "meta-llama/Llama-3.1-8B-Instruct".to_string(),
        },
        architecture: "llama".to_string(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Safetensors,
            // QuantizationRequired is a hard constraint for this preset.
            quantization: Some(Quantization::Int8),
        },
        context_length: Some(answers.context_length),
        variants: Vec::new(),
    });

    let provider = if answers.eu_residency_required {
        "hetzner"
    } else {
        "runpod"
    };

    graph.hardware_profiles.push(HardwareProfile {
        id: hw_id.as_str().into(),
        provider: provider.to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "l40s".to_string(),
            count: gpu_count,
            memory: ByteSize::from_bytes(48 * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(96 * GIB),
        interconnect: None,
        price: None,
    });

    graph.deployments.push(Deployment {
        id: format!("{prefix}_deployment").as_str().into(),
        model: model_id.as_str().into(),
        hardware_profile: hw_id.as_str().into(),
        runtime: Runtime::Vllm(VllmSettings {
            context_length: answers.context_length,
            port: 8000,
            tensor_parallel_size: Some(gpu_count),
            gpu_memory_utilization: Some(0.9),
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    });

    graph.data_classes.push(DataClass {
        id: dc_id.as_str().into(),
        sensitivity: Sensitivity::Confidential,
        residency: eu_residency(answers),
        allow_external_providers: !answers.eu_residency_required,
        handling: Vec::new(),
    });

    push_use_case(&mut graph, &preset.requirement_defaults, &dc_id, answers);
    maybe_push_eu_governance(&mut graph, preset, answers, &dc_id);
    maybe_push_budget(&mut graph, prefix, answers);

    graph
}

fn build_document_extraction_pipeline(preset: &Preset, answers: &WizardAnswers) -> Graph {
    let mut graph = Graph::new();
    let prefix = preset.id.as_str();
    let model_id = format!("{prefix}_model");
    let hw_id = format!("{prefix}_gpu");
    let dc_id = format!("{prefix}_data");
    let gpu_count = gpus_for_concurrency(answers.concurrency);

    graph.models.push(Model {
        id: model_id.as_str().into(),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "Qwen/Qwen2-VL-7B-Instruct".to_string(),
        },
        architecture: "qwen2_vl".to_string(),
        modalities: vec![Modality::Text, Modality::Vision],
        artifact: Artifact {
            format: ArtifactFormat::Safetensors,
            // QuantizationRequired is a hard constraint for this preset.
            quantization: Some(Quantization::Int4),
        },
        context_length: Some(answers.context_length),
        variants: Vec::new(),
    });

    let provider = if answers.eu_residency_required {
        "hetzner"
    } else {
        "runpod"
    };

    graph.hardware_profiles.push(HardwareProfile {
        id: hw_id.as_str().into(),
        provider: provider.to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "a10g".to_string(),
            count: gpu_count,
            memory: ByteSize::from_bytes(24 * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(64 * GIB),
        interconnect: None,
        price: None,
    });

    graph.deployments.push(Deployment {
        id: format!("{prefix}_deployment").as_str().into(),
        model: model_id.as_str().into(),
        hardware_profile: hw_id.as_str().into(),
        runtime: Runtime::Vllm(VllmSettings {
            context_length: answers.context_length,
            port: 8000,
            tensor_parallel_size: Some(gpu_count),
            // Pack the KV cache hard: throughput over headroom, as with
            // cost_optimized_batch.
            gpu_memory_utilization: Some(0.95),
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    });

    graph.data_classes.push(DataClass {
        id: dc_id.as_str().into(),
        sensitivity: Sensitivity::Internal,
        residency: eu_residency(answers),
        allow_external_providers: !answers.eu_residency_required,
        handling: Vec::new(),
    });

    push_use_case(&mut graph, &preset.requirement_defaults, &dc_id, answers);
    maybe_push_eu_governance(&mut graph, preset, answers, &dc_id);
    maybe_push_budget(&mut graph, prefix, answers);

    graph
}

fn build_code_assistant_endpoint(preset: &Preset, answers: &WizardAnswers) -> Graph {
    let mut graph = Graph::new();
    let prefix = preset.id.as_str();
    let model_id = format!("{prefix}_model");
    let hw_id = format!("{prefix}_gpu");
    let dc_id = format!("{prefix}_data");
    let gpu_count = gpus_for_concurrency(answers.concurrency);

    graph.models.push(Model {
        id: model_id.as_str().into(),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "Qwen/Qwen2.5-Coder-14B-Instruct".to_string(),
        },
        architecture: "qwen2".to_string(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Safetensors,
            quantization: None,
        },
        context_length: Some(answers.context_length),
        variants: Vec::new(),
    });

    let provider = if answers.eu_residency_required {
        "hetzner"
    } else {
        "runpod"
    };

    graph.hardware_profiles.push(HardwareProfile {
        id: hw_id.as_str().into(),
        provider: provider.to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "a100".to_string(),
            count: gpu_count,
            memory: ByteSize::from_bytes(80 * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(160 * GIB),
        interconnect: None,
        price: None,
    });

    graph.deployments.push(Deployment {
        id: format!("{prefix}_deployment").as_str().into(),
        model: model_id.as_str().into(),
        hardware_profile: hw_id.as_str().into(),
        runtime: Runtime::Vllm(VllmSettings {
            context_length: answers.context_length,
            port: 8000,
            tensor_parallel_size: Some(gpu_count),
            gpu_memory_utilization: Some(0.9),
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    });

    graph.data_classes.push(DataClass {
        id: dc_id.as_str().into(),
        sensitivity: Sensitivity::Confidential,
        residency: eu_residency(answers),
        allow_external_providers: !answers.eu_residency_required,
        handling: Vec::new(),
    });

    push_use_case(&mut graph, &preset.requirement_defaults, &dc_id, answers);
    maybe_push_eu_governance(&mut graph, preset, answers, &dc_id);
    maybe_push_budget(&mut graph, prefix, answers);

    graph
}

fn build_api_replacement_migration(preset: &Preset, answers: &WizardAnswers) -> Graph {
    let mut graph = Graph::new();
    let prefix = preset.id.as_str();
    let model_id = format!("{prefix}_model");
    let hw_id = format!("{prefix}_gpu");
    let dc_id = format!("{prefix}_data");
    let gpu_count = gpus_for_concurrency(answers.concurrency);

    graph.models.push(Model {
        id: model_id.as_str().into(),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "meta-llama/Llama-3.1-8B-Instruct".to_string(),
        },
        architecture: "llama".to_string(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Safetensors,
            quantization: None,
        },
        context_length: Some(answers.context_length),
        variants: Vec::new(),
    });

    let provider = if answers.eu_residency_required {
        "hetzner"
    } else {
        "runpod"
    };

    graph.hardware_profiles.push(HardwareProfile {
        id: hw_id.as_str().into(),
        provider: provider.to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "l40s".to_string(),
            count: gpu_count,
            memory: ByteSize::from_bytes(48 * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(96 * GIB),
        interconnect: None,
        price: None,
    });

    graph.deployments.push(Deployment {
        id: format!("{prefix}_deployment").as_str().into(),
        model: model_id.as_str().into(),
        hardware_profile: hw_id.as_str().into(),
        runtime: Runtime::Vllm(VllmSettings {
            context_length: answers.context_length,
            port: 8000,
            tensor_parallel_size: Some(gpu_count),
            gpu_memory_utilization: Some(0.9),
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    });

    graph.data_classes.push(DataClass {
        id: dc_id.as_str().into(),
        sensitivity: Sensitivity::Internal,
        residency: eu_residency(answers),
        allow_external_providers: !answers.eu_residency_required,
        handling: Vec::new(),
    });

    push_use_case(&mut graph, &preset.requirement_defaults, &dc_id, answers);
    maybe_push_eu_governance(&mut graph, preset, answers, &dc_id);
    maybe_push_budget(&mut graph, prefix, answers);

    graph
}

fn build_custom_stack(preset: &Preset, answers: &WizardAnswers) -> Graph {
    let mut graph = Graph::new();
    let prefix = preset.id.as_str();
    let model_id = format!("{prefix}_model");
    let hw_id = format!("{prefix}_gpu");
    let dc_id = format!("{prefix}_data");
    let gpu_count = gpus_for_concurrency(answers.concurrency);

    graph.models.push(Model {
        id: model_id.as_str().into(),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "Qwen/Qwen2.5-7B-Instruct".to_string(),
        },
        architecture: "qwen2".to_string(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Safetensors,
            quantization: None,
        },
        context_length: Some(answers.context_length),
        variants: Vec::new(),
    });

    let provider = if answers.eu_residency_required {
        "hetzner"
    } else {
        "runpod"
    };

    graph.hardware_profiles.push(HardwareProfile {
        id: hw_id.as_str().into(),
        provider: provider.to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "a10g".to_string(),
            count: gpu_count,
            memory: ByteSize::from_bytes(24 * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(64 * GIB),
        interconnect: None,
        price: None,
    });

    graph.deployments.push(Deployment {
        id: format!("{prefix}_deployment").as_str().into(),
        model: model_id.as_str().into(),
        hardware_profile: hw_id.as_str().into(),
        runtime: Runtime::Vllm(VllmSettings {
            context_length: answers.context_length,
            port: 8000,
            tensor_parallel_size: Some(gpu_count),
            gpu_memory_utilization: Some(0.9),
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    });

    graph.data_classes.push(DataClass {
        id: dc_id.as_str().into(),
        sensitivity: Sensitivity::Internal,
        residency: eu_residency(answers),
        allow_external_providers: !answers.eu_residency_required,
        handling: Vec::new(),
    });

    push_use_case(&mut graph, &preset.requirement_defaults, &dc_id, answers);
    maybe_push_eu_governance(&mut graph, preset, answers, &dc_id);
    maybe_push_budget(&mut graph, prefix, answers);

    graph
}

// --- shared builder helpers -------------------------------------------------

/// One binary gibibyte. `pub(crate)` so [`crate::rank`]'s candidate variants
/// share the same unit.
pub(crate) const GIB: u64 = 1024 * 1024 * 1024;

/// GPUs to shard across for a concurrency level: one per 8 concurrent requests,
/// at least one. `pub(crate)` so [`crate::rank`]'s candidate variants derive
/// the same base GPU count before varying it.
pub(crate) fn gpus_for_concurrency(concurrency: u32) -> u32 {
    concurrency.div_ceil(8).max(1)
}

/// The residency list a data class should carry, given the answers: EU when the
/// wizard requires it, else unrestricted.
fn eu_residency(answers: &WizardAnswers) -> Vec<Residency> {
    if answers.eu_residency_required {
        vec![Residency::Eu]
    } else {
        Vec::new()
    }
}

/// Add the graph's use case, pointing it at the data class this build created so
/// the reference resolves, and carrying the answers' latency ceiling.
fn push_use_case(graph: &mut Graph, defaults: &UseCase, dc_id: &str, answers: &WizardAnswers) {
    let mut use_case = defaults.clone();
    use_case.required_data_class = Some(DataClassId::new(dc_id));
    use_case.min_context_length = Some(answers.context_length);
    if answers.max_latency_ms.is_some() {
        use_case.max_latency_ms = answers.max_latency_ms;
    }
    graph.use_cases.push(use_case);
}

/// Add an EU governance rule only when the answers require EU residency (for
/// presets where EU is optional).
fn maybe_push_eu_governance(
    graph: &mut Graph,
    preset: &Preset,
    answers: &WizardAnswers,
    dc_id: &str,
) {
    // A preset carrying EuResidencyOnly wires its own rule; this covers the
    // opt-in case for presets where EU is not a hard constraint.
    let is_hard = preset
        .constraints
        .contains(&PresetConstraint::EuResidencyOnly);
    // A LocalOnly preset's hardware provider is always the literal string
    // "local" - not a cloud provider `amicu-providers` has any residency
    // mapping for (`resolve_residency("local", ..)` is always `None`), and
    // never a member of `EU_PROVIDERS`. Pinning the EU-only-providers
    // governance rule here would therefore make *every* LocalOnly graph
    // invalid regardless of the answer, since `report_for`'s provider-allow
    // check is a flat string match with no on-device exemption. On-device
    // execution never leaves first-party infrastructure by construction
    // (stronger than "stays within the EU"), so the requirement is already
    // satisfied without a governance rule to enforce it.
    let is_local_only = preset.constraints.contains(&PresetConstraint::LocalOnly);
    if answers.eu_residency_required && !is_hard && !is_local_only {
        push_eu_governance(graph, preset.id.as_str(), dc_id);
    }
}

/// Add a governance rule that allows the data class only on EU-resident
/// providers. Deliberately pins no `allowed_regions`: the EU-only provider set
/// already guarantees EU residency, and a region-code allowlist would reject a
/// user who edits to a different genuine EU region on an allowed provider. The
/// paired data class carries EU `residency`, which the planner checks against
/// the region's resolved jurisdiction.
fn push_eu_governance(graph: &mut Graph, prefix: &str, dc_id: &str) {
    graph.governance_rules.push(GovernanceRule {
        id: GovernanceRuleId::new(format!("{prefix}_eu_residency")),
        description: Some("Allow this data only on EU-resident providers.".to_string()),
        effect: amicu_core::resource::RuleEffect::Allow,
        data_classes: vec![DataClassId::new(dc_id)],
        allowed_providers: EU_PROVIDERS.iter().map(|p| (*p).to_string()).collect(),
        allowed_regions: Vec::new(),
    });
}

/// Add a monthly budget from the answers' spend ceiling, if one was given.
fn maybe_push_budget(graph: &mut Graph, prefix: &str, answers: &WizardAnswers) {
    if let Some(limit) = &answers.budget {
        graph.budgets.push(Budget {
            id: format!("{prefix}_budget").as_str().into(),
            tenant: None,
            limit: limit.clone(),
            period: BudgetPeriod::Monthly,
            alert_thresholds: vec![0.8, 1.0],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers() -> WizardAnswers {
        WizardAnswers {
            context_length: 8192,
            concurrency: 16,
            eu_residency_required: false,
            budget: Some(Money::new(500.0, "EUR")),
            max_latency_ms: Some(1500),
            priority: None,
        }
    }

    /// Every preset id the catalog is expected to carry: the original three
    /// plus the six added by amicu-planning#149 to cover the PRD's preset
    /// list.
    const ALL_PRESET_IDS: [&str; 9] = [
        "local_dev_chat",
        "private_eu_production",
        "cost_optimized_batch",
        "internal_knowledge_chatbot",
        "customer_support_assistant",
        "document_extraction_pipeline",
        "code_assistant_endpoint",
        "api_replacement_migration",
        "custom_stack",
    ];

    #[test]
    fn catalog_is_non_empty_and_has_every_expected_id() {
        let ids: Vec<&str> = catalog().iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids.len(),
            ALL_PRESET_IDS.len(),
            "catalog size drifted from the expected preset list"
        );
        for expected in ALL_PRESET_IDS {
            assert!(ids.contains(&expected), "missing preset id {expected}");
        }
    }

    #[test]
    fn every_preset_min_tier_is_community() {
        for preset in catalog() {
            assert_eq!(preset.min_tier, Tier::Community, "{}", preset.id);
        }
    }

    #[test]
    fn preset_id_validates_snake_case() {
        assert!(PresetId::new("local_dev_chat").is_ok());
        assert!(PresetId::new("cost_optimized_batch").is_ok());
        for bad in [
            "",
            "1leading",
            "Has_Upper",
            "has.dot",
            "has space",
            "_leading",
        ] {
            assert!(
                matches!(PresetId::new(bad), Err(PresetError::InvalidId(_))),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn build_graph_rejects_zero_answers() {
        let preset = &catalog()[0];
        let mut a = answers();
        a.context_length = 0;
        assert!(matches!(
            build_graph(preset, &a),
            Err(PresetError::InvalidAnswer(_))
        ));
        let mut a = answers();
        a.concurrency = 0;
        assert!(matches!(
            build_graph(preset, &a),
            Err(PresetError::InvalidAnswer(_))
        ));
    }

    #[test]
    fn build_graph_produces_resolvable_graphs_for_every_preset() {
        for preset in catalog() {
            let graph = build_graph(preset, &answers()).expect("build");
            assert_eq!(graph.models.len(), 1, "{}", preset.id);
            assert_eq!(graph.hardware_profiles.len(), 1, "{}", preset.id);
            assert_eq!(graph.deployments.len(), 1, "{}", preset.id);
            assert!(!graph.data_classes.is_empty(), "{}", preset.id);
            assert!(!graph.budgets.is_empty(), "{}", preset.id);
            assert!(
                graph.missing_references().is_empty(),
                "{} has dangling references: {:?}",
                preset.id,
                graph.missing_references()
            );
        }
    }

    #[test]
    fn context_length_flows_into_runtime_settings() {
        let mut a = answers();
        a.context_length = 4096;
        for preset in catalog() {
            let graph = build_graph(preset, &a).expect("build");
            let ctx = match &graph.deployments[0].runtime {
                Runtime::LlamaCpp(s) => s.context_length,
                Runtime::Vllm(s) => s.context_length,
            };
            assert_eq!(ctx, 4096, "{}", preset.id);
        }
    }

    #[test]
    fn concurrency_drives_tensor_parallel_size() {
        let mut a = answers();
        a.concurrency = 16;
        let eu = catalog()
            .iter()
            .find(|p| p.id.as_str() == "private_eu_production")
            .expect("eu preset");
        let graph = build_graph(eu, &a).expect("build");
        match &graph.deployments[0].runtime {
            Runtime::Vllm(s) => assert_eq!(s.tensor_parallel_size, Some(2)),
            Runtime::LlamaCpp(_) => panic!("expected vllm"),
        }
    }

    #[test]
    fn eu_preset_data_class_forbids_external_providers() {
        let eu = catalog()
            .iter()
            .find(|p| p.id.as_str() == "private_eu_production")
            .expect("eu preset");
        let graph = build_graph(eu, &answers()).expect("build");
        assert!(
            graph
                .data_classes
                .iter()
                .any(|dc| !dc.allow_external_providers),
            "EU preset must have a data class forbidding external providers"
        );
        assert!(
            !graph.governance_rules.is_empty(),
            "EU preset must pin a governance rule"
        );
        let rule = &graph.governance_rules[0];
        // The rule allows only EU providers and pins NO region codes: an
        // EU-only provider set already guarantees residency, and a code
        // allowlist would fight a valid edit to another genuine EU region.
        assert!(
            rule.allowed_regions.is_empty(),
            "EU preset must not pin an allowed_regions code allowlist"
        );
        assert!(!rule.allowed_providers.is_empty());
        for provider in &rule.allowed_providers {
            assert!(
                EU_PROVIDERS.contains(&provider.as_str()),
                "provider {provider:?} is not in the EU-only allowlist"
            );
        }
    }

    #[test]
    fn build_graph_rejects_absurd_concurrency() {
        let eu = catalog()
            .iter()
            .find(|p| p.id.as_str() == "private_eu_production")
            .expect("eu preset");
        let mut a = answers();
        a.concurrency = MAX_CONCURRENCY + 1;
        assert!(matches!(
            build_graph(eu, &a),
            Err(PresetError::InvalidAnswer(_))
        ));
        // The cap itself is accepted.
        a.concurrency = MAX_CONCURRENCY;
        assert!(build_graph(eu, &a).is_ok());
    }

    #[test]
    fn build_graph_rejects_non_finite_or_negative_budget() {
        let preset = &catalog()[0];
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            let mut a = answers();
            a.budget = Some(Money::new(bad, "EUR"));
            assert!(
                matches!(build_graph(preset, &a), Err(PresetError::InvalidAnswer(_))),
                "budget amount {bad} should be rejected"
            );
        }
        // Zero is a valid ceiling.
        let mut a = answers();
        a.budget = Some(Money::new(0.0, "EUR"));
        assert!(build_graph(preset, &a).is_ok());
    }

    #[test]
    fn build_graph_produces_report_for_valid_graphs() {
        for preset in catalog() {
            let graph = build_graph(preset, &answers()).expect("build");
            let report = amicu_engine::report_for(&graph);
            assert!(
                report.valid,
                "{} should validate cleanly, got {:?}",
                preset.id, report.diagnostics
            );
        }
    }

    #[test]
    fn budget_is_omitted_when_no_ceiling_given() {
        let mut a = answers();
        a.budget = None;
        let graph = build_graph(&catalog()[0], &a).expect("build");
        assert!(graph.budgets.is_empty());
    }

    #[test]
    fn local_dev_chat_honors_eu_residency_required_without_an_invalid_graph() {
        // Regression: `maybe_push_eu_governance` used to pin an EU-only
        // provider allowlist even for LocalOnly presets, whose hardware
        // provider is always the literal "local" - never a member of
        // `EU_PROVIDERS` - which made `report_for` reject this graph outright
        // whenever the wizard answered `eu_residency_required = true`.
        let local = catalog()
            .iter()
            .find(|p| p.id.as_str() == "local_dev_chat")
            .expect("local_dev_chat in catalog");
        let mut a = answers();
        a.eu_residency_required = true;
        let graph = build_graph(local, &a).expect("build");
        let report = amicu_engine::report_for(&graph);
        assert!(
            report.valid,
            "local_dev_chat with eu_residency_required=true should validate cleanly, got {:?}",
            report.diagnostics
        );
        assert_eq!(graph.hardware_profiles[0].provider, "local");
    }

    #[test]
    fn cost_optimized_batch_honors_eu_residency_required_with_an_eu_provider() {
        // Regression: the hardware provider stayed hardcoded to "runpod"
        // (not EU-resident) even when the wizard set
        // `eu_residency_required = true`, which pins an EU-only provider
        // allowlist via `maybe_push_eu_governance` - so `report_for` rejected
        // the resulting graph.
        let batch = catalog()
            .iter()
            .find(|p| p.id.as_str() == "cost_optimized_batch")
            .expect("cost_optimized_batch in catalog");
        let mut a = answers();
        a.eu_residency_required = true;
        let graph = build_graph(batch, &a).expect("build");
        let report = amicu_engine::report_for(&graph);
        assert!(
            report.valid,
            "cost_optimized_batch with eu_residency_required=true should validate cleanly, got \
             {:?}",
            report.diagnostics
        );
        assert!(
            EU_PROVIDERS.contains(&graph.hardware_profiles[0].provider.as_str()),
            "provider {:?} should be EU-resident when eu_residency_required=true",
            graph.hardware_profiles[0].provider
        );
    }

    #[test]
    fn cost_optimized_batch_stays_on_runpod_when_eu_residency_is_not_required() {
        // The provider switch is conditional: the default (non-EU-required)
        // shape must not regress to the EU provider.
        let batch = catalog()
            .iter()
            .find(|p| p.id.as_str() == "cost_optimized_batch")
            .expect("cost_optimized_batch in catalog");
        let graph = build_graph(batch, &answers()).expect("build");
        assert_eq!(graph.hardware_profiles[0].provider, "runpod");
    }
}
