//! The stable `amicu plan` JSON contract.
//!
//! [`Plan`] is the single object the CLI renders, and that the API and UI
//! consume. It carries a stable [`Plan::version`] string and derives a JSON
//! Schema (via `schemars`) so the contract can be validated and published.
//!
//! The contract is deliberately self-contained: resource references,
//! artifacts, hardware, provider and region are plain strings and small
//! structs rather than shared domain types, so the wire shape stays stable
//! independent of internal domain modelling.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A full plan: the set of actions Amicu would take, with the context needed
/// to review them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct Plan {
    /// Contract version. Currently always `"v1"`.
    pub version: String,
    /// The planned actions, in application order.
    pub actions: Vec<PlannedAction>,
    /// Aggregate policy outcome across all actions, if evaluated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicyOutcome>,
}

impl Plan {
    /// The current contract version string.
    pub const VERSION: &'static str = "v1";
}

/// The kind of change a [`PlannedAction`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    /// Create a resource that does not yet exist.
    Create,
    /// Update an existing resource in place.
    Update,
    /// Destroy and recreate a resource.
    Replace,
    /// Destroy an existing resource.
    Destroy,
}

/// A single planned change to one resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PlannedAction {
    /// The kind of change.
    pub kind: ActionKind,
    /// The target resource address (e.g. `deployment.support_chat_local`).
    pub address: String,
    /// The selected artifact and its download characteristics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Artifact>,
    /// Runtime and material launch settings, as opaque key/value pairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime: Vec<Setting>,
    /// Target hardware descriptor (e.g. `gpu.a100_80gb`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware: Option<String>,
    /// Target provider (e.g. `runpod`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Target region (e.g. `eu-central`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Estimated cost and performance ranges with provenance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_performance: Option<CostPerformance>,
    /// Names of secrets this action requires.
    ///
    /// These are secret NAMES only. Secret values never appear in a plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_secrets: Vec<String>,
    /// Manual action the user must take before this can be applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_action: Option<String>,
}

/// A selected model artifact and its download characteristics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct Artifact {
    /// Artifact name / identifier.
    pub name: String,
    /// Artifact format (e.g. `gguf`, `safetensors`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Quantization label (e.g. `Q4_K_M`), if quantized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    /// Estimated download size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_download_size_bytes: Option<u64>,
}

/// An opaque runtime or launch setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct Setting {
    /// Setting key.
    pub key: String,
    /// Setting value, rendered as a string.
    pub value: String,
}

/// Estimated cost and performance, with the basis for the estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CostPerformance {
    /// Estimated cost range.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Range>,
    /// Estimated performance range (units described in `assumptions`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance: Option<Range>,
    /// Assumptions the estimate rests on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<String>,
    /// Where the estimate came from (e.g. `benchmark`, `heuristic`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    /// Confidence in the estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
}

/// An inclusive numeric range with an optional unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct Range {
    /// Lower bound.
    pub min: f64,
    /// Upper bound.
    pub max: f64,
    /// Unit label (e.g. `usd_per_hour`, `tokens_per_second`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// Confidence level attached to an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Low confidence.
    Low,
    /// Medium confidence.
    Medium,
    /// High confidence.
    High,
}

/// Governance / policy outcome for a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct PolicyOutcome {
    /// Whether the plan is permitted under evaluated policy.
    pub allowed: bool,
    /// Risks surfaced by policy evaluation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<String>,
    /// Reasons the plan was denied, if `allowed` is false.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denials: Vec<String>,
}
