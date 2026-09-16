//! HTTP surface for the stateless guided-wizard preset endpoints:
//! `GET /presets`, `GET /presets/{id}`, and `POST /presets/{id}/emit`.
//!
//! All three are pure functions of their inputs and read no server state. The
//! gallery lists the [`catalog`](amicu_recommend::catalog); the detail route
//! seeds the wizard form with a preset's requirement defaults and the estimate
//! shown before generation; and the emit route turns a preset plus the wizard's
//! [`WizardAnswers`] into HCL text.
//!
//! The emit path is deterministic: it is `preset + answers -> Graph -> HCL` and
//! nothing more. It does not run the advisory diagnosis engine and synthesizes
//! no metrics; the `estimate` it returns is the preset's own `default_estimate`,
//! carried through for display. The HCL is produced only by
//! [`amicu_hcl::format_graph`], the one supported emitter.
//!
//! Access is gated by [`auth_gate`](crate::engine::auth_gate) at the same
//! Community minimum as the other engine routes: the guided wizard is a
//! free-tier feature, so a Community token reaches all three (`200`, never
//! `403`). An unknown preset id is a `404`; an out-of-range wizard answer is a
//! `400`.

use amicu_recommend::{Preset, PresetError, WizardAnswers, WizardPriority, build_graph, catalog};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use crate::dto::{MoneyDto, TierDto, WorkloadEstimateDto};
use crate::engine::EngineApiState;

// ---------------------------------------------------------------------------
// Response DTOs
// ---------------------------------------------------------------------------

/// The serving runtime family a preset commits to. Mirrors
/// [`amicu_recommend::RuntimeFamily`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFamilyDto {
    LlamaCpp,
    Vllm,
}

impl From<amicu_recommend::RuntimeFamily> for RuntimeFamilyDto {
    fn from(f: amicu_recommend::RuntimeFamily) -> Self {
        match f {
            amicu_recommend::RuntimeFamily::LlamaCpp => RuntimeFamilyDto::LlamaCpp,
            amicu_recommend::RuntimeFamily::Vllm => RuntimeFamilyDto::Vllm,
        }
    }
}

/// A hard constraint a preset commits to, shown on its gallery card. Mirrors
/// [`amicu_recommend::PresetConstraint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PresetConstraintDto {
    EuResidencyOnly,
    QuantizationRequired,
    LocalOnly,
    ExternalProvidersForbidden,
    RuntimeFamily(RuntimeFamilyDto),
}

impl From<amicu_recommend::PresetConstraint> for PresetConstraintDto {
    fn from(c: amicu_recommend::PresetConstraint) -> Self {
        use amicu_recommend::PresetConstraint as C;
        match c {
            C::EuResidencyOnly => PresetConstraintDto::EuResidencyOnly,
            C::QuantizationRequired => PresetConstraintDto::QuantizationRequired,
            C::LocalOnly => PresetConstraintDto::LocalOnly,
            C::ExternalProvidersForbidden => PresetConstraintDto::ExternalProvidersForbidden,
            C::RuntimeFamily(f) => PresetConstraintDto::RuntimeFamily(f.into()),
        }
    }
}

/// An input or output modality a use case requires. Mirrors
/// [`amicu_core::resource::Modality`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModalityDto {
    Text,
    Vision,
    Audio,
    Embedding,
}

impl From<amicu_core::resource::Modality> for ModalityDto {
    fn from(m: amicu_core::resource::Modality) -> Self {
        use amicu_core::resource::Modality as M;
        match m {
            M::Text => ModalityDto::Text,
            M::Vision => ModalityDto::Vision,
            M::Audio => ModalityDto::Audio,
            M::Embedding => ModalityDto::Embedding,
        }
    }
}

/// Relative importance of a use case. Mirrors
/// [`amicu_core::resource::Priority`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PriorityDto {
    Low,
    Medium,
    High,
}

impl From<amicu_core::resource::Priority> for PriorityDto {
    fn from(p: amicu_core::resource::Priority) -> Self {
        use amicu_core::resource::Priority as P;
        match p {
            P::Low => PriorityDto::Low,
            P::Medium => PriorityDto::Medium,
            P::High => PriorityDto::High,
        }
    }
}

/// Which soft-constraint axis to weight most heavily when ranking candidates.
/// Mirrors [`amicu_recommend::WizardPriority`]. Distinct from [`PriorityDto`]
/// (a use case's Low/Medium/High importance).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WizardPriorityDto {
    Cost,
    Latency,
    Quality,
    Privacy,
    Simplicity,
}

impl From<WizardPriorityDto> for WizardPriority {
    fn from(p: WizardPriorityDto) -> Self {
        match p {
            WizardPriorityDto::Cost => WizardPriority::Cost,
            WizardPriorityDto::Latency => WizardPriority::Latency,
            WizardPriorityDto::Quality => WizardPriority::Quality,
            WizardPriorityDto::Privacy => WizardPriority::Privacy,
            WizardPriorityDto::Simplicity => WizardPriority::Simplicity,
        }
    }
}

impl From<WizardPriority> for WizardPriorityDto {
    fn from(p: WizardPriority) -> Self {
        match p {
            WizardPriority::Cost => WizardPriorityDto::Cost,
            WizardPriority::Latency => WizardPriorityDto::Latency,
            WizardPriority::Quality => WizardPriorityDto::Quality,
            WizardPriority::Privacy => WizardPriorityDto::Privacy,
            WizardPriority::Simplicity => WizardPriorityDto::Simplicity,
        }
    }
}

/// A preset's requirement defaults, pre-filled into the wizard form. Mirrors
/// [`amicu_core::resource::UseCase`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct UseCaseDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modalities: Vec<ModalityDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_context_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_latency_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_data_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<PriorityDto>,
}

impl From<&amicu_core::resource::UseCase> for UseCaseDto {
    fn from(u: &amicu_core::resource::UseCase) -> Self {
        UseCaseDto {
            id: u.id.as_str().to_owned(),
            description: u.description.clone(),
            modalities: u.modalities.iter().copied().map(Into::into).collect(),
            min_context_length: u.min_context_length,
            max_latency_ms: u.max_latency_ms,
            required_data_class: u
                .required_data_class
                .as_ref()
                .map(|d| d.as_str().to_owned()),
            priority: u.priority.map(Into::into),
        }
    }
}

/// A gallery card: the identity, marketing copy, hard constraints, and minimum
/// tier a preset commits to. Returned by `GET /presets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PresetSummaryDto {
    /// Stable snake_case identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// One-line summary shown under the name.
    pub tagline: String,
    /// Who and what this preset fits.
    pub fit: String,
    /// The hard constraints this preset commits to.
    pub constraints: Vec<PresetConstraintDto>,
    /// Minimum product tier required to use this preset.
    pub min_tier: TierDto,
}

impl From<&Preset> for PresetSummaryDto {
    fn from(p: &Preset) -> Self {
        PresetSummaryDto {
            id: p.id.as_str().to_owned(),
            name: p.name.clone(),
            tagline: p.tagline.clone(),
            fit: p.fit.clone(),
            constraints: p.constraints.iter().cloned().map(Into::into).collect(),
            min_tier: p.min_tier.into(),
        }
    }
}

/// The full detail for one preset, used to seed the wizard form: the gallery
/// summary plus the requirement defaults and the estimate shown before any
/// answers are collected. Returned by `GET /presets/{id}`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PresetDetailDto {
    /// The gallery-card summary.
    pub summary: PresetSummaryDto,
    /// Requirement defaults pre-filled into the wizard.
    pub requirement_defaults: UseCaseDto,
    /// The estimate shown before any answers are collected.
    pub default_estimate: WorkloadEstimateDto,
}

impl From<&Preset> for PresetDetailDto {
    fn from(p: &Preset) -> Self {
        PresetDetailDto {
            summary: PresetSummaryDto::from(p),
            requirement_defaults: UseCaseDto::from(&p.requirement_defaults),
            default_estimate: WorkloadEstimateDto::from(p.default_estimate.clone()),
        }
    }
}

/// Response for `POST /presets/{id}/emit`: the generated HCL text plus the
/// preset's estimate for display. The estimate is the preset's own
/// `default_estimate`, carried through unchanged; the emit path synthesizes no
/// metrics.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EmitResponseDto {
    /// The Amicu HCL configuration text produced from the preset and answers.
    pub hcl: String,
    /// The preset's estimate, shown alongside the generated config.
    pub estimate: WorkloadEstimateDto,
}

// ---------------------------------------------------------------------------
// Request DTO (wizard answers for emit)
// ---------------------------------------------------------------------------

/// Request body for `POST /presets/{id}/emit`: the requirements the wizard
/// collected. Mirrors [`amicu_recommend::WizardAnswers`].
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct WizardAnswersDto {
    /// Context length (tokens) the runtime is configured for. Must be > 0.
    pub context_length: u32,
    /// Expected concurrent requests. Drives GPU count. Must be > 0.
    pub concurrency: u32,
    /// Whether the workload must stay within EU jurisdictions.
    #[serde(default)]
    pub eu_residency_required: bool,
    /// Optional monthly spend ceiling; when set, becomes a budget.
    #[serde(default)]
    pub budget: Option<MoneyDto>,
    /// Optional latency ceiling (ms) carried into the graph's use case.
    #[serde(default)]
    pub max_latency_ms: Option<u32>,
    /// Which soft-constraint axis to weight most heavily when ranking
    /// candidates via `POST /recommend/candidates`. Ignored by
    /// `POST /presets/{id}/emit`, which builds a single graph and does no
    /// ranking. Omitted or `null` means balanced weighting.
    #[serde(default)]
    pub priority: Option<WizardPriorityDto>,
}

impl From<WizardAnswersDto> for WizardAnswers {
    fn from(a: WizardAnswersDto) -> Self {
        WizardAnswers {
            context_length: a.context_length,
            concurrency: a.concurrency,
            eu_residency_required: a.eu_residency_required,
            budget: a.budget.map(Into::into),
            max_latency_ms: a.max_latency_ms,
            priority: a.priority.map(Into::into),
        }
    }
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Error surfaced by the preset handlers.
pub(crate) enum PresetHttpError {
    /// No preset exists for the requested id (not found).
    NotFound(String),
    /// A wizard answer was outside the acceptable range (bad request).
    BadAnswer(String),
    /// The graph could not be rendered to HCL (internal).
    Emit(amicu_hcl::HclError),
}

impl From<PresetError> for PresetHttpError {
    fn from(e: PresetError) -> Self {
        match e {
            // An unknown template for the resolved preset is a not-found, the
            // same class the id lookup would produce.
            PresetError::UnknownPreset(id) => PresetHttpError::NotFound(id.to_string()),
            // A malformed id or an out-of-range answer is the client's own bad
            // request; echoing the message is safe (it names only the field).
            PresetError::InvalidId(msg) | PresetError::InvalidAnswer(msg) => {
                PresetHttpError::BadAnswer(msg)
            }
        }
    }
}

impl From<amicu_hcl::HclError> for PresetHttpError {
    fn from(e: amicu_hcl::HclError) -> Self {
        PresetHttpError::Emit(e)
    }
}

impl IntoResponse for PresetHttpError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            PresetHttpError::NotFound(id) => {
                (StatusCode::NOT_FOUND, format!("no preset with id `{id}`"))
            }
            PresetHttpError::BadAnswer(msg) => (StatusCode::BAD_REQUEST, msg),
            // A render failure is internal detail. Log the real cause and return
            // a fixed, generic body so nothing about the internals leaks.
            PresetHttpError::Emit(e) => {
                eprintln!("amicu-api: preset HCL emit failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_owned(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// The catalog entry whose id matches `id`, if any.
///
/// `pub(crate)` so [`crate::recommend_wizard`]'s questions/candidates
/// handlers resolve a preset id the same way these handlers do.
pub(crate) fn find_preset(id: &str) -> Option<&'static Preset> {
    catalog().iter().find(|p| p.id.as_str() == id)
}

/// List the guided-wizard presets for the gallery.
///
/// Returns every catalog entry as a gallery-card summary. Stateless: the
/// catalog is a fixed, in-code list.
#[utoipa::path(
    get,
    path = "/presets",
    tag = "preset",
    responses(
        (status = 200, description = "The preset gallery.", body = Vec<PresetSummaryDto>),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn list_presets_handler(
    State(_state): State<EngineApiState>,
) -> Json<Vec<PresetSummaryDto>> {
    Json(catalog().iter().map(PresetSummaryDto::from).collect())
}

/// Fetch one preset's detail to seed the wizard form.
///
/// Returns the gallery summary plus the requirement defaults and the estimate
/// shown before any answers are collected. A `404` if the id is unknown.
#[utoipa::path(
    get,
    path = "/presets/{id}",
    tag = "preset",
    params(("id" = String, Path, description = "The snake_case preset id.")),
    responses(
        (status = 200, description = "The preset detail.", body = PresetDetailDto),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 404, description = "No preset exists with this id."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn get_preset_handler(
    State(_state): State<EngineApiState>,
    Path(id): Path<String>,
) -> Result<Json<PresetDetailDto>, PresetHttpError> {
    let preset = find_preset(&id).ok_or(PresetHttpError::NotFound(id))?;
    Ok(Json(PresetDetailDto::from(preset)))
}

/// Emit HCL for a preset from the wizard's answers.
///
/// Deterministic: builds the preset's candidate [`Graph`](amicu_core::graph::Graph)
/// from the answers and renders it to HCL. No diagnosis is run and no metrics
/// are synthesized; the returned `estimate` is the preset's own
/// `default_estimate`, for display. A `404` if the id is unknown, a `400` if a
/// wizard answer is out of range.
#[utoipa::path(
    post,
    path = "/presets/{id}/emit",
    tag = "preset",
    params(("id" = String, Path, description = "The snake_case preset id.")),
    request_body = WizardAnswersDto,
    responses(
        (status = 200, description = "The generated HCL plus the preset's estimate.", body = EmitResponseDto),
        (status = 400, description = "A wizard answer is out of range."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 404, description = "No preset exists with this id."),
        (status = 422, description = "The JSON body is well-formed but a field has the wrong type or is missing."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn emit_preset_handler(
    State(_state): State<EngineApiState>,
    Path(id): Path<String>,
    Json(body): Json<WizardAnswersDto>,
) -> Result<Json<EmitResponseDto>, PresetHttpError> {
    let preset = find_preset(&id).ok_or(PresetHttpError::NotFound(id))?;
    let answers = WizardAnswers::from(body);
    let graph = build_graph(preset, &answers)?;
    let hcl = amicu_hcl::format_graph(&graph)?;
    Ok(Json(EmitResponseDto {
        hcl,
        estimate: WorkloadEstimateDto::from(preset.default_estimate.clone()),
    }))
}
