//! Multi-candidate ranking: [`rank`] turns a preset plus [`WizardAnswers`]
//! into 2-4 distinct, hard-constraint-compliant [`Candidate`]s, ordered by a
//! soft-constraint score weighted by [`WizardPriority`].
//!
//! The flow, per the PRD's guided-wizard contract:
//! 1. Build the preset's base graph ([`build_graph`]), which also validates
//!    `answers` (a bad answer surfaces as [`PresetError::InvalidAnswer`]
//!    exactly as it does for `/presets/{id}/emit`).
//! 2. Generate a small, fixed set of graph variants by varying the preset's
//!    free knobs (hardware tier/accelerator, quantization, GPU count) inside
//!    the hard constraints the preset already commits to.
//! 3. Filter: [`enforce_hard_constraints`] runs [`hard_violation`] against
//!    *every* generated variant (not just the deliberately-illegal EU decoy
//!    below) - a variant that would violate one of the preset's
//!    [`PresetConstraint`]s is never proposed as its own candidate. Its
//!    rejection reason is folded into a surviving candidate, so the
//!    "considered and ruled out" trail is not lost. If every variant
//!    violates a constraint, `rank` returns [`PresetError::InvalidAnswer`]
//!    rather than a candidate list with nothing compliant in it.
//! 4. Score the surviving variants against the five [`SoftConstraint`] axes,
//!    weighted by `answers.priority` (balanced when absent), and order by the
//!    weighted total.
//!
//! Deterministic: every score is a fixed value declared on the variant
//! (never a clock, random source, or `HashMap` iteration order), and the
//! final sort is stable, so the same `(preset, answers)` always produces a
//! byte-identical ordered candidate list.

use amicu_core::estimate::Confidence;
use amicu_core::graph::Graph;
use amicu_core::resource::{AcceleratorKind, Quantization, Runtime};
use amicu_core::units::ByteSize;

use crate::candidate::{Candidate, CandidateArchetype, CandidateScore};
use crate::preset::{EU_PROVIDERS, GIB, Preset, PresetConstraint, RuntimeFamily};
use crate::recommendation::RejectedAlternative;
use crate::{PresetError, WizardAnswers, WizardPriority, build_graph};

/// The largest number of candidates returned, regardless of how many distinct
/// variants a preset defines.
const MAX_CANDIDATES: usize = 4;

/// Generate the ranked candidate list for `preset` given the wizard's
/// `answers`.
///
/// # Errors
/// Returns [`PresetError::InvalidAnswer`] or [`PresetError::UnknownPreset`]
/// under exactly the same conditions as [`build_graph`] (which this calls
/// first to validate `answers` and derive the base graph every variant is
/// built from).
pub fn rank(preset: &Preset, answers: &WizardAnswers) -> Result<Vec<Candidate>, PresetError> {
    let base = build_graph(preset, answers)?;
    let specs = variant_specs(preset, &base);
    let mut specs = enforce_hard_constraints(preset, specs)?;
    // Distinct archetypes only, cap at MAX_CANDIDATES, preserve declared order
    // (highest-priority variant per preset is declared first) before scoring
    // re-sorts by weighted total.
    specs.dedup_by(|a, b| a.graph == b.graph);
    specs.truncate(MAX_CANDIDATES);

    let weights = axis_weights(answers.priority);
    let confidence = candidate_confidence(preset);

    let mut candidates: Vec<Candidate> = specs
        .into_iter()
        .map(|spec| spec.into_candidate(preset, &weights, confidence))
        .collect();

    // Highest weighted total first; stable sort keeps the declared order
    // (deterministic) among any exact ties.
    candidates.sort_by(|a, b| b.score.total.total_cmp(&a.score.total));
    Ok(candidates)
}

/// The confidence carried by every candidate for `preset`: reused from the
/// preset's own card-level estimate rather than inventing a new figure - the
/// ranking is a heuristic reordering of hand-specified variants, not a fresh
/// measurement.
fn candidate_confidence(preset: &Preset) -> Confidence {
    preset.default_estimate.expected_ttft_ms.confidence()
}

/// Per-axis weights `[cost, latency, quality, privacy, simplicity]` for a
/// wizard priority. Balanced (equal weighting) when `priority` is `None`,
/// per decision #4: default = balanced behaviour.
fn axis_weights(priority: Option<WizardPriority>) -> [f64; 5] {
    match priority {
        None => [0.2, 0.2, 0.2, 0.2, 0.2],
        Some(WizardPriority::Cost) => [0.6, 0.1, 0.1, 0.1, 0.1],
        Some(WizardPriority::Latency) => [0.1, 0.6, 0.1, 0.1, 0.1],
        Some(WizardPriority::Quality) => [0.1, 0.1, 0.6, 0.1, 0.1],
        Some(WizardPriority::Privacy) => [0.1, 0.1, 0.1, 0.6, 0.1],
        Some(WizardPriority::Simplicity) => [0.1, 0.1, 0.1, 0.1, 0.6],
    }
}

/// One candidate-in-waiting: an archetype, its graph, fixed axis scores, and
/// the rationale/rejected trail attached at construction time.
struct VariantSpec {
    archetype: CandidateArchetype,
    graph: Graph,
    rationale: Vec<String>,
    rejected: Vec<RejectedAlternative>,
    axes: [f64; 5],
}

impl VariantSpec {
    fn into_candidate(
        self,
        preset: &Preset,
        weights: &[f64; 5],
        confidence: Confidence,
    ) -> Candidate {
        let total = self
            .axes
            .iter()
            .zip(weights.iter())
            .map(|(axis, weight)| axis * weight)
            .sum();
        Candidate {
            archetype: self.archetype,
            graph: self.graph,
            score: CandidateScore {
                cost: self.axes[0],
                latency: self.axes[1],
                quality: self.axes[2],
                privacy: self.axes[3],
                simplicity: self.axes[4],
                total,
            },
            rationale: self.rationale,
            rejected: self.rejected,
            estimate: preset.default_estimate.clone(),
            confidence,
        }
    }
}

/// Run [`hard_violation`] against every generated variant: a variant whose
/// graph violates one of `preset`'s hard [`PresetConstraint`]s is removed and
/// never itself returned as a candidate. Its rejection reason is folded into
/// the first surviving variant (declared-first, i.e. the preset's own
/// preferred archetype), so the "considered and ruled out" trail is never
/// silently dropped - it lands somewhere a caller can see it, alongside any
/// rejection a variant already carried from its own construction (e.g. the
/// EU decoy in `private_eu_production_variants`).
///
/// # Errors
/// Returns [`PresetError::InvalidAnswer`] if every generated variant
/// violates a hard constraint, leaving nothing compliant to return.
fn enforce_hard_constraints(
    preset: &Preset,
    specs: Vec<VariantSpec>,
) -> Result<Vec<VariantSpec>, PresetError> {
    let mut survivors = Vec::with_capacity(specs.len());
    let mut violations = Vec::new();
    for spec in specs {
        match hard_violation(preset, &spec.graph) {
            Some(reason) => violations.push((spec.archetype, reason)),
            None => survivors.push(spec),
        }
    }

    if survivors.is_empty() {
        return Err(PresetError::InvalidAnswer(format!(
            "no candidate for preset `{}` satisfies its own hard constraints with these answers",
            preset.id
        )));
    }

    if let Some(first) = survivors.first_mut() {
        first
            .rejected
            .extend(violations.into_iter().map(|(archetype, reason)| {
                RejectedAlternative::new(format!("{archetype:?} variant"), reason)
            }));
    }

    Ok(survivors)
}

/// Whether `graph` violates one of `preset`'s hard [`PresetConstraint`]s.
/// `Some(reason)` names the first violation found; `None` means `graph`
/// satisfies every constraint the preset commits to. This is the filter that
/// guarantees a hard-constraint-violating stack is never proposed.
fn hard_violation(preset: &Preset, graph: &Graph) -> Option<String> {
    for constraint in &preset.constraints {
        match constraint {
            PresetConstraint::EuResidencyOnly | PresetConstraint::ExternalProvidersForbidden => {
                for hw in &graph.hardware_profiles {
                    if !EU_PROVIDERS.contains(&hw.provider.as_str()) {
                        return Some(format!(
                            "provider `{}` is not on the EU-resident allowlist required by this \
                             preset",
                            hw.provider
                        ));
                    }
                }
            }
            PresetConstraint::LocalOnly => {
                for hw in &graph.hardware_profiles {
                    if hw.provider != "local" {
                        return Some(format!(
                            "provider `{}` is not local-only, required by this preset",
                            hw.provider
                        ));
                    }
                }
            }
            PresetConstraint::QuantizationRequired => {
                for model in &graph.models {
                    if model.artifact.quantization.is_none() {
                        return Some(format!(
                            "model `{}` is not quantized, required by this preset",
                            model.id
                        ));
                    }
                }
            }
            PresetConstraint::RuntimeFamily(family) => {
                for deployment in &graph.deployments {
                    let matches = matches!(
                        (family, &deployment.runtime),
                        (RuntimeFamily::LlamaCpp, Runtime::LlamaCpp(_))
                            | (RuntimeFamily::Vllm, Runtime::Vllm(_))
                    );
                    if !matches {
                        return Some(format!(
                            "deployment `{}` does not use the {family:?} runtime family required \
                             by this preset",
                            deployment.id
                        ));
                    }
                }
            }
        }
    }
    None
}

/// Set the (sole) model's GGUF quantization and its declared variant to
/// match, so the runtime's memory-fit check has a variant to resolve.
fn set_gguf_quantization(graph: &mut Graph, quantization: Quantization, file: &str) {
    if let Some(model) = graph.models.first_mut() {
        model.artifact.quantization = Some(quantization);
        if let Some(variant) = model.variants.first_mut() {
            variant.quantization = quantization;
            variant.file = file.to_string();
        }
    }
}

/// The fixed variant set for `preset`, built from `base` (the preset's own
/// [`build_graph`] output) by varying its free knobs. Order is the declared
/// preference order before scoring re-sorts.
fn variant_specs(preset: &Preset, base: &Graph) -> Vec<VariantSpec> {
    match preset.id.as_str() {
        "local_dev_chat" => local_dev_chat_variants(base),
        "private_eu_production" => private_eu_production_variants(preset, base),
        "cost_optimized_batch" => cost_optimized_batch_variants(base),
        "internal_knowledge_chatbot" => internal_knowledge_chatbot_variants(base),
        "customer_support_assistant" => customer_support_assistant_variants(base),
        "document_extraction_pipeline" => document_extraction_pipeline_variants(base),
        "code_assistant_endpoint" => code_assistant_endpoint_variants(base),
        "api_replacement_migration" => api_replacement_migration_variants(base),
        "custom_stack" => custom_stack_variants(base),
        // A preset with no curated variant set has, by construction, exactly
        // one shape: its own base graph. Per decision #5 this is a valid,
        // explicitly-explained single candidate rather than an error.
        _ => vec![VariantSpec {
            archetype: CandidateArchetype::Balanced,
            graph: base.clone(),
            rationale: vec![format!(
                "`{}` defines only one candidate shape; no free knob is varied.",
                preset.id
            )],
            rejected: Vec::new(),
            axes: [0.5, 0.5, 0.5, 0.5, 0.5],
        }],
    }
}

// --- local_dev_chat ----------------------------------------------------------

fn local_dev_chat_variants(base: &Graph) -> Vec<VariantSpec> {
    let mut simplest = base.clone();
    set_gguf_quantization(&mut simplest, Quantization::Q4KM, "Qwen3.5-9B-Q4_K_M.gguf");

    let mut higher_quality = base.clone();
    set_gguf_quantization(
        &mut higher_quality,
        Quantization::Q6K,
        "Qwen3.5-9B-Q6_K.gguf",
    );
    if let Some(hw) = higher_quality.hardware_profiles.first_mut() {
        hw.memory = ByteSize::from_bytes(64 * GIB);
        if let Some(accel) = hw.accelerator.as_mut() {
            accel.memory = ByteSize::from_bytes(64 * GIB);
        }
    }

    vec![
        VariantSpec {
            archetype: CandidateArchetype::Simplest,
            graph: simplest,
            rationale: vec![
                "Q4_K_M quantization: the smallest download and fastest local start.".to_string(),
                "No cloud spend - runs entirely on-device.".to_string(),
            ],
            rejected: Vec::new(),
            axes: [1.0, 0.9, 0.6, 1.0, 1.0],
        },
        VariantSpec {
            archetype: CandidateArchetype::PerformanceOptimized,
            graph: higher_quality,
            rationale: vec![
                "Q6_K quantization: noticeably higher answer quality for a larger download and \
                 more memory headroom."
                    .to_string(),
                "Still fully local and zero marginal cost.".to_string(),
            ],
            rejected: Vec::new(),
            axes: [1.0, 0.6, 0.9, 1.0, 0.7],
        },
    ]
}

// --- private_eu_production ----------------------------------------------------

fn private_eu_production_variants(preset: &Preset, base: &Graph) -> Vec<VariantSpec> {
    // The decoy: a hypothetically cheaper non-EU external provider. Hard
    // constraints (EuResidencyOnly, ExternalProvidersForbidden) mean this can
    // never be proposed as its own candidate; its rejection reason is folded
    // into the CostOptimized candidate that stands in its place.
    let mut decoy = base.clone();
    if let Some(hw) = decoy.hardware_profiles.first_mut() {
        hw.provider = "runpod".to_string();
        hw.region = None;
        if let Some(accel) = hw.accelerator.as_mut() {
            accel.kind = AcceleratorKind::Gpu;
            accel.model = "a10g".to_string();
        }
    }
    let decoy_reason = hard_violation(preset, &decoy)
        .unwrap_or_else(|| "excluded: violates this preset's hard constraints".to_string());

    let mut cost_optimized = base.clone();
    if let Some(hw) = cost_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.model = "l40s".to_string();
        accel.memory = ByteSize::from_bytes(48 * GIB);
    }

    let mut performance_optimized = base.clone();
    if let Some(hw) = performance_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.count += 1;
    }
    if let Some(deployment) = performance_optimized.deployments.first_mut()
        && let Runtime::Vllm(settings) = &mut deployment.runtime
    {
        settings.tensor_parallel_size = performance_optimized
            .hardware_profiles
            .first()
            .and_then(|hw| hw.accelerator.as_ref())
            .map(|a| a.count);
        settings.gpu_memory_utilization = Some(0.85);
    }

    vec![
        VariantSpec {
            archetype: CandidateArchetype::CostOptimized,
            graph: cost_optimized,
            rationale: vec![
                "L40S accelerators on the same EU-only provider allowlist: lower hourly rate for \
                 somewhat less raw throughput."
                    .to_string(),
            ],
            rejected: vec![RejectedAlternative::new(
                "run on a cheaper external GPU rental (non-EU)",
                decoy_reason,
            )],
            axes: [0.9, 0.6, 0.7, 1.0, 0.7],
        },
        VariantSpec {
            archetype: CandidateArchetype::Balanced,
            graph: base.clone(),
            rationale: vec![
                "H100 accelerators sized to the requested concurrency: the preset's default \
                 balance of cost, latency, and quality."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.5, 0.8, 0.9, 1.0, 0.8],
        },
        VariantSpec {
            archetype: CandidateArchetype::PerformanceOptimized,
            graph: performance_optimized,
            rationale: vec![
                "One extra H100 beyond the concurrency-derived minimum, with more conservative KV \
                 cache headroom, for lower tail latency under load."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.3, 0.95, 0.9, 1.0, 0.6],
        },
    ]
}

// --- cost_optimized_batch ------------------------------------------------------

fn cost_optimized_batch_variants(base: &Graph) -> Vec<VariantSpec> {
    let cost_optimized = base.clone();

    let mut quality_optimized = base.clone();
    set_safetensors_quantization(&mut quality_optimized, Quantization::Int8);
    if let Some(hw) = quality_optimized.hardware_profiles.first_mut() {
        hw.memory = ByteSize::from_bytes(80 * GIB);
        if let Some(accel) = hw.accelerator.as_mut() {
            accel.memory = ByteSize::from_bytes(40 * GIB);
        }
    }

    vec![
        VariantSpec {
            archetype: CandidateArchetype::CostOptimized,
            graph: cost_optimized,
            rationale: vec![
                "INT4 quantization on A10G accelerators: the lowest cost per token, for \
                 throughput-first, latency-tolerant batch work."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [1.0, 0.5, 0.5, 0.5, 0.8],
        },
        VariantSpec {
            archetype: CandidateArchetype::PerformanceOptimized,
            graph: quality_optimized,
            rationale: vec![
                "INT8 quantization: less aggressive than the cost-optimized default, trading some \
                 cost efficiency for noticeably better output quality."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.6, 0.6, 0.8, 0.5, 0.7],
        },
    ]
}

// --- internal_knowledge_chatbot ----------------------------------------------

fn internal_knowledge_chatbot_variants(base: &Graph) -> Vec<VariantSpec> {
    let mut cost_optimized = base.clone();
    set_safetensors_quantization(&mut cost_optimized, Quantization::Int8);
    if let Some(hw) = cost_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.model = "l40s".to_string();
        accel.memory = ByteSize::from_bytes(48 * GIB);
    }

    let mut performance_optimized = base.clone();
    if let Some(hw) = performance_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.count += 1;
    }
    if let Some(deployment) = performance_optimized.deployments.first_mut()
        && let Runtime::Vllm(settings) = &mut deployment.runtime
    {
        settings.tensor_parallel_size = performance_optimized
            .hardware_profiles
            .first()
            .and_then(|hw| hw.accelerator.as_ref())
            .map(|a| a.count);
    }

    vec![
        VariantSpec {
            archetype: CandidateArchetype::Balanced,
            graph: base.clone(),
            rationale: vec![
                "A100 accelerators sized to the requested concurrency: the preset's default \
                 balance of cost, latency, and quality for RAG-style prompts."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.5, 0.7, 0.8, 0.6, 0.8],
        },
        VariantSpec {
            archetype: CandidateArchetype::CostOptimized,
            graph: cost_optimized,
            rationale: vec![
                "L40S accelerators with INT8 weights: lower hourly rate for a RAG workload that \
                 is not latency-critical."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.85, 0.5, 0.6, 0.6, 0.7],
        },
        VariantSpec {
            archetype: CandidateArchetype::PerformanceOptimized,
            graph: performance_optimized,
            rationale: vec![
                "One extra A100 beyond the concurrency-derived minimum, for lower tail latency \
                 when retrieved-context prompts run long."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.3, 0.85, 0.85, 0.6, 0.6],
        },
    ]
}

// --- customer_support_assistant ----------------------------------------------

fn customer_support_assistant_variants(base: &Graph) -> Vec<VariantSpec> {
    let mut cost_optimized = base.clone();
    set_safetensors_quantization(&mut cost_optimized, Quantization::Int4);
    if let Some(hw) = cost_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.model = "a10g".to_string();
        accel.memory = ByteSize::from_bytes(24 * GIB);
    }

    let mut performance_optimized = base.clone();
    if let Some(hw) = performance_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.count += 1;
    }
    if let Some(deployment) = performance_optimized.deployments.first_mut()
        && let Runtime::Vllm(settings) = &mut deployment.runtime
    {
        settings.tensor_parallel_size = performance_optimized
            .hardware_profiles
            .first()
            .and_then(|hw| hw.accelerator.as_ref())
            .map(|a| a.count);
        settings.gpu_memory_utilization = Some(0.85);
    }

    vec![
        VariantSpec {
            archetype: CandidateArchetype::CostOptimized,
            graph: cost_optimized,
            rationale: vec![
                "INT4 weights on A10G accelerators: lowest cost per token for support traffic."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.9, 0.6, 0.5, 0.6, 0.8],
        },
        VariantSpec {
            archetype: CandidateArchetype::Balanced,
            graph: base.clone(),
            rationale: vec![
                "INT8 weights on L40S accelerators: the preset's default balance of cost, \
                 latency, and answer quality."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.6, 0.7, 0.75, 0.6, 0.8],
        },
        VariantSpec {
            archetype: CandidateArchetype::PerformanceOptimized,
            graph: performance_optimized,
            rationale: vec![
                "One extra L40S beyond the concurrency-derived minimum, for lower tail latency \
                 under peak support load."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.4, 0.9, 0.8, 0.6, 0.6],
        },
    ]
}

// --- document_extraction_pipeline --------------------------------------------

fn document_extraction_pipeline_variants(base: &Graph) -> Vec<VariantSpec> {
    let cost_optimized = base.clone();

    let mut quality_optimized = base.clone();
    set_safetensors_quantization(&mut quality_optimized, Quantization::Int8);
    if let Some(hw) = quality_optimized.hardware_profiles.first_mut() {
        hw.memory = ByteSize::from_bytes(80 * GIB);
        if let Some(accel) = hw.accelerator.as_mut() {
            accel.model = "l40s".to_string();
            accel.memory = ByteSize::from_bytes(48 * GIB);
        }
    }

    vec![
        VariantSpec {
            archetype: CandidateArchetype::CostOptimized,
            graph: cost_optimized,
            rationale: vec![
                "INT4 weights on A10G accelerators: the lowest cost per extracted page."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [1.0, 0.5, 0.5, 0.5, 0.8],
        },
        VariantSpec {
            archetype: CandidateArchetype::PerformanceOptimized,
            graph: quality_optimized,
            rationale: vec![
                "INT8 weights on L40S accelerators: less aggressive quantization for higher \
                 extraction accuracy on dense or scanned pages."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.6, 0.6, 0.85, 0.5, 0.7],
        },
    ]
}

// --- code_assistant_endpoint --------------------------------------------------

fn code_assistant_endpoint_variants(base: &Graph) -> Vec<VariantSpec> {
    let mut cost_optimized = base.clone();
    set_safetensors_quantization(&mut cost_optimized, Quantization::Int8);
    if let Some(hw) = cost_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.model = "l40s".to_string();
        accel.memory = ByteSize::from_bytes(48 * GIB);
    }

    let mut performance_optimized = base.clone();
    if let Some(hw) = performance_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.count += 1;
    }
    if let Some(deployment) = performance_optimized.deployments.first_mut()
        && let Runtime::Vllm(settings) = &mut deployment.runtime
    {
        settings.tensor_parallel_size = performance_optimized
            .hardware_profiles
            .first()
            .and_then(|hw| hw.accelerator.as_ref())
            .map(|a| a.count);
        settings.gpu_memory_utilization = Some(0.85);
    }

    vec![
        VariantSpec {
            archetype: CandidateArchetype::Balanced,
            graph: base.clone(),
            rationale: vec![
                "Unquantized weights on A100 accelerators: the preset's default balance for \
                 interactive coding completions."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.4, 0.8, 0.9, 0.6, 0.7],
        },
        VariantSpec {
            archetype: CandidateArchetype::CostOptimized,
            graph: cost_optimized,
            rationale: vec![
                "INT8 weights on L40S accelerators: lower cost with a small quality trade-off \
                 for code completions."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.8, 0.6, 0.7, 0.6, 0.7],
        },
        VariantSpec {
            archetype: CandidateArchetype::PerformanceOptimized,
            graph: performance_optimized,
            rationale: vec![
                "One extra A100 beyond the concurrency-derived minimum, for lower tail latency \
                 on interactive completions."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.2, 0.95, 0.9, 0.6, 0.5],
        },
    ]
}

// --- api_replacement_migration ------------------------------------------------

fn api_replacement_migration_variants(base: &Graph) -> Vec<VariantSpec> {
    let mut cost_optimized = base.clone();
    set_safetensors_quantization(&mut cost_optimized, Quantization::Int4);
    if let Some(hw) = cost_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.model = "a10g".to_string();
        accel.memory = ByteSize::from_bytes(24 * GIB);
    }

    let mut performance_optimized = base.clone();
    if let Some(hw) = performance_optimized.hardware_profiles.first_mut()
        && let Some(accel) = hw.accelerator.as_mut()
    {
        accel.count += 1;
    }
    if let Some(deployment) = performance_optimized.deployments.first_mut()
        && let Runtime::Vllm(settings) = &mut deployment.runtime
    {
        settings.tensor_parallel_size = performance_optimized
            .hardware_profiles
            .first()
            .and_then(|hw| hw.accelerator.as_ref())
            .map(|a| a.count);
    }

    vec![
        VariantSpec {
            archetype: CandidateArchetype::CostOptimized,
            graph: cost_optimized,
            rationale: vec![
                "INT4 weights on A10G accelerators: the cheapest drop-in replacement for \
                 cost-sensitive migrations."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.9, 0.5, 0.5, 0.5, 0.7],
        },
        VariantSpec {
            archetype: CandidateArchetype::Balanced,
            graph: base.clone(),
            rationale: vec![
                "Unquantized weights on L40S accelerators: the closest quality match to a \
                 full-precision third-party API."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.5, 0.7, 0.8, 0.5, 0.7],
        },
        VariantSpec {
            archetype: CandidateArchetype::PerformanceOptimized,
            graph: performance_optimized,
            rationale: vec![
                "One extra L40S beyond the concurrency-derived minimum, to absorb migrated \
                 traffic spikes without added tail latency."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.3, 0.9, 0.8, 0.5, 0.6],
        },
    ]
}

// --- custom_stack ---------------------------------------------------------------

fn custom_stack_variants(base: &Graph) -> Vec<VariantSpec> {
    let mut cost_optimized = base.clone();
    set_safetensors_quantization(&mut cost_optimized, Quantization::Int4);

    let mut performance_optimized = base.clone();
    if let Some(hw) = performance_optimized.hardware_profiles.first_mut() {
        hw.memory = ByteSize::from_bytes(160 * GIB);
        if let Some(accel) = hw.accelerator.as_mut() {
            accel.model = "a100".to_string();
            accel.memory = ByteSize::from_bytes(80 * GIB);
        }
    }

    vec![
        VariantSpec {
            archetype: CandidateArchetype::Balanced,
            graph: base.clone(),
            rationale: vec![
                "A10G accelerators, unquantized weights: a reasonable general-purpose starting \
                 point with no preset opinion baked in."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.5, 0.5, 0.6, 0.5, 0.8],
        },
        VariantSpec {
            archetype: CandidateArchetype::CostOptimized,
            graph: cost_optimized,
            rationale: vec![
                "INT4 weights on the same hardware: lower cost per token if quality headroom \
                 allows it."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.8, 0.5, 0.5, 0.5, 0.7],
        },
        VariantSpec {
            archetype: CandidateArchetype::PerformanceOptimized,
            graph: performance_optimized,
            rationale: vec![
                "A100 accelerators: more headroom for a heavier model or longer context once \
                 the real shape is known."
                    .to_string(),
            ],
            rejected: Vec::new(),
            axes: [0.2, 0.7, 0.85, 0.5, 0.5],
        },
    ]
}

/// Set quantization on a safetensors (vLLM) model, without touching a GGUF
/// `variants` list (the cost-optimized-batch and private-EU-production
/// presets are both vLLM/safetensors, which carries no `variants` vec).
fn set_safetensors_quantization(graph: &mut Graph, quantization: Quantization) {
    if let Some(model) = graph.models.first_mut() {
        model.artifact.quantization = Some(quantization);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;
    use amicu_core::resource::Money;

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

    fn preset(id: &str) -> &'static Preset {
        catalog()
            .iter()
            .find(|p| p.id.as_str() == id)
            .unwrap_or_else(|| panic!("{id} in catalog"))
    }

    #[test]
    fn rank_is_deterministic() {
        for preset in catalog() {
            let a = rank(preset, &answers()).expect("rank");
            let b = rank(preset, &answers()).expect("rank");
            assert_eq!(a, b, "{} ranking is not deterministic", preset.id);
        }
    }

    #[test]
    fn cardinality_is_two_to_four_for_every_catalog_preset() {
        for preset in catalog() {
            let candidates = rank(preset, &answers()).expect("rank");
            assert!(
                (2..=4).contains(&candidates.len()),
                "{} produced {} candidates",
                preset.id,
                candidates.len()
            );
        }
    }

    #[test]
    fn candidates_are_distinct_graphs() {
        for preset in catalog() {
            let candidates = rank(preset, &answers()).expect("rank");
            for (i, a) in candidates.iter().enumerate() {
                for b in &candidates[i + 1..] {
                    assert_ne!(a.graph, b.graph, "{} produced duplicate graphs", preset.id);
                }
            }
        }
    }

    #[test]
    fn every_candidate_graph_passes_report_for() {
        for preset in catalog() {
            let candidates = rank(preset, &answers()).expect("rank");
            for candidate in candidates {
                let report = amicu_engine::report_for(&candidate.graph);
                assert!(
                    report.valid,
                    "{} {:?} candidate failed report_for: {:?}",
                    preset.id, candidate.archetype, report.diagnostics
                );
            }
        }
    }

    #[test]
    fn eu_preset_never_yields_an_external_provider_candidate() {
        let eu = preset("private_eu_production");
        let candidates = rank(eu, &answers()).expect("rank");
        for candidate in &candidates {
            for hw in &candidate.graph.hardware_profiles {
                assert!(
                    EU_PROVIDERS.contains(&hw.provider.as_str()),
                    "provider {:?} is not EU-resident",
                    hw.provider
                );
            }
        }
        // The hard-constraint filter left a trail: some candidate documents
        // the non-EU alternative it ruled out.
        assert!(
            candidates.iter().any(|c| !c.rejected.is_empty()),
            "expected at least one candidate to carry a rejected alternative"
        );
    }

    #[test]
    fn rejected_alternative_names_the_hard_constraint_reason() {
        let eu = preset("private_eu_production");
        let candidates = rank(eu, &answers()).expect("rank");
        let with_rejection = candidates
            .iter()
            .find(|c| !c.rejected.is_empty())
            .expect("a candidate with a rejected alternative");
        let reason = &with_rejection.rejected[0].reason;
        assert!(
            reason.contains("EU-resident") || reason.contains("provider"),
            "reason should explain the hard-constraint violation, got {reason:?}"
        );
    }

    #[test]
    fn priority_reorders_candidates() {
        let eu = preset("private_eu_production");
        let mut cost_first = answers();
        cost_first.priority = Some(WizardPriority::Cost);
        let cost_ranked = rank(eu, &cost_first).expect("rank");
        assert_eq!(cost_ranked[0].archetype, CandidateArchetype::CostOptimized);

        let mut latency_first = answers();
        latency_first.priority = Some(WizardPriority::Latency);
        let latency_ranked = rank(eu, &latency_first).expect("rank");
        assert_eq!(
            latency_ranked[0].archetype,
            CandidateArchetype::PerformanceOptimized
        );
    }

    #[test]
    fn rank_propagates_invalid_answer_errors() {
        let preset = &catalog()[0];
        let mut a = answers();
        a.concurrency = 0;
        assert!(matches!(
            rank(preset, &a),
            Err(PresetError::InvalidAnswer(_))
        ));
    }

    #[test]
    fn local_dev_chat_returns_two_local_candidates() {
        let local = preset("local_dev_chat");
        let candidates = rank(local, &answers()).expect("rank");
        assert_eq!(candidates.len(), 2);
        for candidate in &candidates {
            for hw in &candidate.graph.hardware_profiles {
                assert_eq!(hw.provider, "local");
            }
        }
    }

    #[test]
    fn every_candidate_satisfies_every_hard_constraint_of_its_preset() {
        // Generalizes `eu_preset_never_yields_an_external_provider_candidate`
        // to every PresetConstraint kind (RuntimeFamily, QuantizationRequired,
        // LocalOnly, ExternalProvidersForbidden, EuResidencyOnly), not just
        // the provider allowlist, and to every catalog preset.
        for preset in catalog() {
            let candidates = rank(preset, &answers()).expect("rank");
            for candidate in &candidates {
                assert!(
                    hard_violation(preset, &candidate.graph).is_none(),
                    "{} {:?} candidate violates a hard constraint: {:?}",
                    preset.id,
                    candidate.archetype,
                    hard_violation(preset, &candidate.graph)
                );
            }
        }
    }

    #[test]
    fn enforce_hard_constraints_filters_a_deliberately_violating_variant() {
        // Mutation-style: construct a variant that violates
        // cost_optimized_batch's QuantizationRequired constraint alongside a
        // compliant one, and confirm the enforcement step removes only the
        // violator - never silently - folding its reason into the survivor.
        let batch = preset("cost_optimized_batch");
        let base = build_graph(batch, &answers()).expect("base graph");

        let mut violating = base.clone();
        if let Some(model) = violating.models.first_mut() {
            model.artifact.quantization = None;
        }
        assert!(
            hard_violation(batch, &violating).is_some(),
            "the mutated graph should actually violate QuantizationRequired"
        );

        let specs = vec![
            VariantSpec {
                archetype: CandidateArchetype::Balanced,
                graph: base.clone(),
                rationale: vec!["ok".to_string()],
                rejected: Vec::new(),
                axes: [0.5, 0.5, 0.5, 0.5, 0.5],
            },
            VariantSpec {
                archetype: CandidateArchetype::CostOptimized,
                graph: violating,
                rationale: vec!["deliberately unquantized".to_string()],
                rejected: Vec::new(),
                axes: [0.9, 0.5, 0.5, 0.5, 0.5],
            },
        ];

        let survivors = enforce_hard_constraints(batch, specs).expect("one survivor");
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].archetype, CandidateArchetype::Balanced);
        assert!(
            survivors[0]
                .rejected
                .iter()
                .any(|r| r.reason.contains("not quantized")),
            "the violation reason should be folded into the survivor, got {:?}",
            survivors[0].rejected
        );
    }

    #[test]
    fn enforce_hard_constraints_errors_when_every_variant_violates() {
        let batch = preset("cost_optimized_batch");
        let base = build_graph(batch, &answers()).expect("base graph");
        let mut violating = base;
        if let Some(model) = violating.models.first_mut() {
            model.artifact.quantization = None;
        }
        let specs = vec![VariantSpec {
            archetype: CandidateArchetype::Balanced,
            graph: violating,
            rationale: vec!["deliberately unquantized".to_string()],
            rejected: Vec::new(),
            axes: [0.5, 0.5, 0.5, 0.5, 0.5],
        }];
        assert!(matches!(
            enforce_hard_constraints(batch, specs),
            Err(PresetError::InvalidAnswer(_))
        ));
    }

    #[test]
    fn every_preset_is_compliant_across_a_representative_answer_grid() {
        // Table-driven compliance sweep: every catalog
        // preset, crossed with a representative grid of context length,
        // concurrency, EU residency, and priority, must ask <= 6 questions,
        // rank into 1-4 hard-constraint-compliant candidates that each carry
        // a rationale and pass `report_for`, and rank deterministically. A
        // future preset that regresses any of these fails here automatically
        // - no per-preset test needs to be hand-written.
        const CONTEXT_LENGTHS: [u32; 2] = [4096, 32768];
        const CONCURRENCIES: [u32; 2] = [1, 64];
        const RESIDENCY: [bool; 2] = [false, true];
        const PRIORITIES: [Option<WizardPriority>; 3] = [
            None,
            Some(WizardPriority::Cost),
            Some(WizardPriority::Quality),
        ];

        for preset in catalog() {
            let question_count = crate::questions_for(preset).len();
            assert!(
                (1..=6).contains(&question_count),
                "{} asks {question_count} questions, expected 1-6",
                preset.id
            );

            for context_length in CONTEXT_LENGTHS {
                for concurrency in CONCURRENCIES {
                    for eu_residency_required in RESIDENCY {
                        for priority in PRIORITIES {
                            let a = WizardAnswers {
                                context_length,
                                concurrency,
                                eu_residency_required,
                                budget: Some(Money::new(1000.0, "EUR")),
                                max_latency_ms: Some(2000),
                                priority,
                            };

                            let candidates = rank(preset, &a).unwrap_or_else(|e| {
                                panic!("{} rank failed for {a:?}: {e}", preset.id)
                            });

                            assert!(
                                (1..=MAX_CANDIDATES).contains(&candidates.len()),
                                "{} produced {} candidates for {a:?}",
                                preset.id,
                                candidates.len()
                            );

                            for candidate in &candidates {
                                assert!(
                                    hard_violation(preset, &candidate.graph).is_none(),
                                    "{} {:?} candidate violates a hard constraint for {a:?}",
                                    preset.id,
                                    candidate.archetype
                                );
                                assert!(
                                    !candidate.rationale.is_empty(),
                                    "{} {:?} candidate has no rationale",
                                    preset.id,
                                    candidate.archetype
                                );
                                let report = amicu_engine::report_for(&candidate.graph);
                                assert!(
                                    report.valid,
                                    "{} {:?} candidate failed report_for for {a:?}: {:?}",
                                    preset.id, candidate.archetype, report.diagnostics
                                );
                            }

                            let again = rank(preset, &a).expect("rerun rank");
                            assert_eq!(
                                candidates, again,
                                "{} ranking is not deterministic for {a:?}",
                                preset.id
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_candidate_passes_report_for_across_priorities_and_residency() {
        // Cross-product regression for the eu_residency_required=true bug:
        // every preset, every priority (including balanced/None), and both
        // residency answers must always yield only report_for-valid
        // candidates.
        const PRIORITIES: [Option<WizardPriority>; 6] = [
            None,
            Some(WizardPriority::Cost),
            Some(WizardPriority::Latency),
            Some(WizardPriority::Quality),
            Some(WizardPriority::Privacy),
            Some(WizardPriority::Simplicity),
        ];
        for preset in catalog() {
            for priority in PRIORITIES {
                for eu_residency_required in [false, true] {
                    let mut a = answers();
                    a.priority = priority;
                    a.eu_residency_required = eu_residency_required;
                    let candidates = rank(preset, &a).expect("rank");
                    for candidate in candidates {
                        let report = amicu_engine::report_for(&candidate.graph);
                        assert!(
                            report.valid,
                            "{} priority={priority:?} eu_residency_required={eu_residency_required} \
                             {:?} candidate failed report_for: {:?}",
                            preset.id, candidate.archetype, report.diagnostics
                        );
                    }
                }
            }
        }
    }
}
