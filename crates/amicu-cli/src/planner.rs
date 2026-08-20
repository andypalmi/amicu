//! Plan construction.
//!
//! The implementation now lives in `amicu-engine` so the API and CLI share one
//! parse → validate → plan surface; this is a thin re-export kept for the
//! `crate::planner` call sites (`plan`, `apply`).

pub(crate) use amicu_engine::planner::{PlanResult, build_plan};
