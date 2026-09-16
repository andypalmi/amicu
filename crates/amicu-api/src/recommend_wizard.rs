//! HTTP surface for the guided-wizard multi-candidate ranking endpoints:
//! `GET /recommend/questions` and `POST /recommend/candidates`.
//!
//! Distinct from [`crate::recommend`]'s `POST /recommend`, which is the
//! advisory *tuning-diagnosis* endpoint over a measured metrics + estimate
//! snapshot: these two are stateless, deterministic functions of a preset id
//! (plus wizard answers for the candidates route).
//! [`amicu_recommend::questions_for`] returns the preset's ≤6 typed core
//! questions; [`amicu_recommend::rank`] returns 2-4 scored,
//! hard-constraint-filtered [`amicu_recommend::Candidate`]s. Neither reads
//! server state, runs the diagnosis engine, or synthesizes metrics.
//!
//! `POST /recommend/candidates` is not `POST /recommend/{id}` (mirroring
//! `/presets/{id}/emit`) because the illustrative RFC path collides with the
//! already-shipped tuning-diagnosis `POST /recommend`; the preset id instead
//! travels in the request body.
//!
//! Access is gated by [`auth_gate`](crate::engine::auth_gate) at the same
//! Community minimum as the other engine routes: the guided wizard is a
//! free-tier feature, so a Community token reaches both (`200`, never `403`).
//! An unknown preset id is a `404`; an out-of-range wizard answer is a `400`.

use amicu_recommend::{
    Candidate, CandidateArchetype, CandidateScore, Question, QuestionKind, WizardAnswers,
    questions_for, rank,
};
use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::dto::{ConfidenceDto, RejectedAlternativeDto, WorkloadEstimateDto};
use crate::engine::EngineApiState;
use crate::preset::{PresetHttpError, WizardAnswersDto, find_preset};

// ---------------------------------------------------------------------------
// Question DTOs
// ---------------------------------------------------------------------------

/// The kind of answer a [`QuestionDto`] expects. Mirrors
/// [`amicu_recommend::QuestionKind`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionKindDto {
    Text,
    Number {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
    },
    Choice {
        options: Vec<String>,
    },
    Boolean,
    Money,
}

impl From<QuestionKind> for QuestionKindDto {
    fn from(k: QuestionKind) -> Self {
        match k {
            QuestionKind::Text => QuestionKindDto::Text,
            QuestionKind::Number { min, max, unit } => QuestionKindDto::Number { min, max, unit },
            QuestionKind::Choice { options } => QuestionKindDto::Choice { options },
            QuestionKind::Boolean => QuestionKindDto::Boolean,
            QuestionKind::Money => QuestionKindDto::Money,
        }
    }
}

/// One wizard question. `id` names the [`WizardAnswersDto`] field this
/// answer feeds. Mirrors [`amicu_recommend::Question`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct QuestionDto {
    pub id: String,
    pub prompt: String,
    pub kind: QuestionKindDto,
    pub required: bool,
    /// Whether this answer, if given, becomes a hard (filtering) constraint
    /// rather than only a soft ranking preference.
    pub hard: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl From<Question> for QuestionDto {
    fn from(q: Question) -> Self {
        QuestionDto {
            id: q.id,
            prompt: q.prompt,
            kind: q.kind.into(),
            required: q.required,
            hard: q.hard,
            help: q.help,
        }
    }
}

/// Query parameters for `GET /recommend/questions`.
#[derive(Debug, Deserialize)]
pub(crate) struct QuestionsQuery {
    /// The snake_case preset id.
    preset: String,
}

// ---------------------------------------------------------------------------
// Candidate DTOs
// ---------------------------------------------------------------------------

/// The shape a ranked candidate represents. Mirrors
/// [`amicu_recommend::CandidateArchetype`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CandidateArchetypeDto {
    Simplest,
    Balanced,
    CostOptimized,
    PerformanceOptimized,
    LocalDevelopment,
}

impl From<CandidateArchetype> for CandidateArchetypeDto {
    fn from(a: CandidateArchetype) -> Self {
        match a {
            CandidateArchetype::Simplest => CandidateArchetypeDto::Simplest,
            CandidateArchetype::Balanced => CandidateArchetypeDto::Balanced,
            CandidateArchetype::CostOptimized => CandidateArchetypeDto::CostOptimized,
            CandidateArchetype::PerformanceOptimized => CandidateArchetypeDto::PerformanceOptimized,
            CandidateArchetype::LocalDevelopment => CandidateArchetypeDto::LocalDevelopment,
        }
    }
}

/// Per-axis soft-constraint scores plus the weighted total a candidate was
/// ordered by. Mirrors [`amicu_recommend::CandidateScore`].
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct CandidateScoreDto {
    pub cost: f64,
    pub latency: f64,
    pub quality: f64,
    pub privacy: f64,
    pub simplicity: f64,
    pub total: f64,
}

impl From<CandidateScore> for CandidateScoreDto {
    fn from(s: CandidateScore) -> Self {
        CandidateScoreDto {
            cost: s.cost,
            latency: s.latency,
            quality: s.quality,
            privacy: s.privacy,
            simplicity: s.simplicity,
            total: s.total,
        }
    }
}

/// One ranked candidate: the graph as canonical JSON, the same graph rendered
/// to HCL (the input the `#35` customize flow edits), its score, rationale,
/// rejected alternatives, estimate, and confidence.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CandidateDto {
    pub archetype: CandidateArchetypeDto,
    /// The candidate's graph as canonical JSON - the same shape `amicu-hcl`
    /// parses/serializes and `amicu-engine` validates. Opaque JSON here
    /// (`amicu_core::graph::Graph`'s resource types are not part of this
    /// crate's typed OpenAPI schema surface); `hcl` is the primary editable
    /// artifact.
    #[schema(value_type = Object)]
    pub graph: serde_json::Value,
    /// The candidate's graph rendered to Amicu HCL.
    pub hcl: String,
    pub score: CandidateScoreDto,
    /// Human-readable reasons this shape was proposed.
    pub rationale: Vec<String>,
    /// Alternatives considered for this archetype and ruled out - including
    /// any that would have violated a hard constraint and was therefore never
    /// itself proposed as a candidate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<RejectedAlternativeDto>,
    pub estimate: WorkloadEstimateDto,
    pub confidence: ConfidenceDto,
}

impl CandidateDto {
    /// Convert a ranked [`Candidate`], rendering its graph to HCL.
    ///
    /// Not a public `TryFrom` impl: its error type ([`PresetHttpError`]) is
    /// crate-private, so this stays an inherent `pub(crate)` function instead
    /// of leaking that type through a public trait impl.
    ///
    /// # Errors
    /// Returns [`PresetHttpError::Emit`] if the graph fails to render to HCL.
    pub(crate) fn from_candidate(c: Candidate) -> Result<Self, PresetHttpError> {
        let hcl = amicu_hcl::format_graph(&c.graph)?;
        let graph = serde_json::to_value(&c.graph).expect("a Graph always serializes to JSON");
        Ok(CandidateDto {
            archetype: c.archetype.into(),
            graph,
            hcl,
            score: c.score.into(),
            rationale: c.rationale,
            rejected: c.rejected.into_iter().map(Into::into).collect(),
            estimate: c.estimate.into(),
            confidence: c.confidence.into(),
        })
    }
}

/// Response for `POST /recommend/candidates`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CandidatesResponseDto {
    /// 2-4 candidates in ranked order (highest-scoring first), or exactly 1
    /// when the preset defines only one candidate shape.
    pub candidates: Vec<CandidateDto>,
}

/// Request body for `POST /recommend/candidates`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CandidatesRequestDto {
    /// The snake_case preset id.
    pub preset_id: String,
    /// The wizard's answers to the preset's core questions.
    pub answers: WizardAnswersDto,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// List the core wizard questions for a preset.
///
/// Returns the ≤6 typed questions [`questions_for`] derives for the preset,
/// so a client renders its form purely from engine-provided data - no
/// question text or option list is authored in TypeScript. A `404` if the
/// preset id is unknown.
#[utoipa::path(
    get,
    path = "/recommend/questions",
    tag = "wizard",
    params(("preset" = String, Query, description = "The snake_case preset id.")),
    responses(
        (status = 200, description = "The preset's core wizard questions.", body = Vec<QuestionDto>),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 404, description = "No preset exists with this id."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn questions_handler(
    State(_state): State<EngineApiState>,
    Query(q): Query<QuestionsQuery>,
) -> Result<Json<Vec<QuestionDto>>, PresetHttpError> {
    let preset =
        find_preset(&q.preset).ok_or_else(|| PresetHttpError::NotFound(q.preset.clone()))?;
    Ok(Json(
        questions_for(preset).into_iter().map(Into::into).collect(),
    ))
}

/// Rank a preset's candidates from the wizard's answers.
///
/// Generates a small fixed set of graph variants for the preset, filters out
/// any that would violate one of the preset's hard constraints (never
/// proposed - the exclusion is recorded as a `rejected` alternative on the
/// candidate that stands in its place), scores the survivors against the
/// wizard's `priority` (balanced when omitted), and returns 2-4 in ranked
/// order. Deterministic: the same request always returns byte-identical
/// output. A `404` if the preset id is unknown, a `400` if a wizard answer is
/// out of range.
#[utoipa::path(
    post,
    path = "/recommend/candidates",
    tag = "wizard",
    request_body = CandidatesRequestDto,
    responses(
        (status = 200, description = "2-4 ranked, hard-constraint-filtered candidates.", body = CandidatesResponseDto),
        (status = 400, description = "A wizard answer is out of range."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 404, description = "No preset exists with this id."),
        (status = 422, description = "The JSON body is well-formed but a field has the wrong type or is missing."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn candidates_handler(
    State(_state): State<EngineApiState>,
    Json(body): Json<CandidatesRequestDto>,
) -> Result<Json<CandidatesResponseDto>, PresetHttpError> {
    let preset = find_preset(&body.preset_id)
        .ok_or_else(|| PresetHttpError::NotFound(body.preset_id.clone()))?;
    let answers = WizardAnswers::from(body.answers);
    let candidates = rank(preset, &answers)?
        .into_iter()
        .map(CandidateDto::from_candidate)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(CandidatesResponseDto { candidates }))
}
