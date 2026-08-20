//! Tuning-recommendation engine: measured metrics + estimates → advisory advice.
//!
//! Given a measured [`StackMetrics`](amicu_telemetry::StackMetrics) surface and
//! a [`WorkloadEstimate`], [`diagnose`] assigns a [`BottleneckClass`] and emits
//! advisory [`Recommendation`]s. Three properties hold by construction:
//! - **Advisory only** — recommendations are descriptive data; nothing in this
//!   crate applies a change to a stack (see [`Recommendation`]).
//! - **Explainable** — each carries the [`Evidence`] that triggered it, the
//!   [`RejectedAlternative`]s it ruled out, and its assumptions, including any
//!   measured-vs-estimated divergence so a wrong call is attributable.
//! - **Tier-gated** — basic monitoring is [`Tier::Community`]; the advisory
//!   recommendations are [`Tier::Pro`]. [`TuningReport::for_tier`] enforces the
//!   boundary.

mod bottleneck;
mod engine;
mod estimate;
mod recommendation;
mod tier;

pub use bottleneck::BottleneckClass;
pub use engine::{Thresholds, TuningReport, diagnose};
pub use estimate::WorkloadEstimate;
pub use recommendation::{
    ChangeDirection, Comparison, Evidence, Recommendation, RejectedAlternative, SuggestedChange,
};
pub use tier::Tier;
