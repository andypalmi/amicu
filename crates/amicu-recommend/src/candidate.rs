//! Ranked wizard candidates: 2-4 distinct, hard-constraint-compliant stack
//! shapes generated from a preset's free knobs, scored against the wizard's
//! soft [`crate::WizardPriority`] preference and carrying their own rationale,
//! rejected alternatives, and estimate.
//!
//! A [`Candidate`] wraps a concrete [`Graph`] - the same shared plan object
//! `amicu-plan`/`amicu-hcl` render and validate - with the *why*: which
//! [`CandidateArchetype`] it represents, how it scored on each
//! [`SoftConstraint`] axis, the human-readable rationale, and any alternative
//! that was considered and ruled out. See [`crate::rank::rank`] for how the
//! list is generated, filtered, and ordered.

use amicu_core::estimate::Confidence;
use amicu_core::graph::Graph;
use serde::{Deserialize, Serialize};

use crate::estimate::WorkloadEstimate;
use crate::recommendation::RejectedAlternative;

/// The shape a ranked candidate represents, from the PRD's guided-wizard
/// archetypes (Simplest, Balanced, Cost-optimized, Performance/quality-
/// optimized, Local-development).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateArchetype {
    Simplest,
    Balanced,
    CostOptimized,
    PerformanceOptimized,
    LocalDevelopment,
}

/// A soft-constraint axis a candidate is scored against. Distinct from a
/// [`crate::PresetConstraint`] (a hard, filtering constraint that excludes a
/// candidate outright): a soft constraint only ever influences ranking order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoftConstraint {
    Cost,
    Latency,
    Quality,
    Privacy,
    Simplicity,
}

/// Per-axis scores in `[0.0, 1.0]` (higher is better) plus the weighted
/// `total` [`crate::rank::rank`] orders candidates by.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CandidateScore {
    pub cost: f64,
    pub latency: f64,
    pub quality: f64,
    pub privacy: f64,
    pub simplicity: f64,
    /// The priority-weighted sum of the five axes above.
    pub total: f64,
}

impl CandidateScore {
    /// This score's value on one [`SoftConstraint`] axis.
    #[must_use]
    pub fn axis(&self, constraint: SoftConstraint) -> f64 {
        match constraint {
            SoftConstraint::Cost => self.cost,
            SoftConstraint::Latency => self.latency,
            SoftConstraint::Quality => self.quality,
            SoftConstraint::Privacy => self.privacy,
            SoftConstraint::Simplicity => self.simplicity,
        }
    }
}

/// One ranked candidate: a concrete graph plus the why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Which archetype this candidate represents.
    pub archetype: CandidateArchetype,
    /// The concrete, editable graph - the same object `amicu-hcl` renders and
    /// `amicu-engine` validates.
    pub graph: Graph,
    pub score: CandidateScore,
    /// Human-readable reasons this shape was proposed, and how it relates to
    /// the wizard's priority (if one was given).
    pub rationale: Vec<String>,
    /// Alternatives considered for this archetype and ruled out - including
    /// any that would have violated a hard constraint and was therefore never
    /// itself proposed as a candidate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<RejectedAlternative>,
    pub estimate: WorkloadEstimate,
    pub confidence: Confidence,
}
