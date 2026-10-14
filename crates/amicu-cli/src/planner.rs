//! Plan construction.
//!
//! The implementation now lives in `amicu-engine` so the API and CLI share one
//! parse → validate → plan surface; this is a thin re-export kept for the
//! `crate::planner` call sites (`plan`, `apply`).

use amicu_core::graph::Graph;

use crate::project::Project;

#[cfg(test)]
pub(crate) use amicu_engine::planner::build_plan;
pub(crate) use amicu_engine::planner::{PlanError, PlanItem, PlanResult};

/// [`build_plan`] for `project`: local GGUF paths resolve to absolute paths
/// under the project root, so the plan shows (and the service runs with) a
/// path that works regardless of the service's working directory.
pub(crate) fn build_project_plan(
    project: &Project,
    graph: &Graph,
    prior: Option<&Graph>,
) -> Result<PlanResult, PlanError> {
    // Model paths stay relative to the project: `apply` makes them absolute at
    // start, so no plan or state carries an absolute project path.
    let _ = project;
    amicu_engine::planner::build_plan_in(graph, prior, None)
}
