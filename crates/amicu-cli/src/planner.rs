//! Plan construction.
//!
//! The implementation now lives in `amicu-engine` so the API and CLI share one
//! parse → validate → plan surface; this is a thin re-export kept for the
//! `crate::planner` call sites (`plan`, `apply`).

use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{Deployment, HardwareProfile};
use amicu_engine::fingerprint::{EnvProbe, Fact, Fingerprint};
use amicu_llamacpp::CommandRunner;
use std::collections::BTreeMap;

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
    recorded: &BTreeMap<DeploymentId, Fingerprint>,
) -> Result<PlanResult, PlanError> {
    build_project_plan_with(project, graph, prior, recorded, &HostEnvProbe)
}

/// [`build_project_plan`] with the environment probe injected (tests).
pub(crate) fn build_project_plan_with(
    project: &Project,
    graph: &Graph,
    prior: Option<&Graph>,
    recorded: &BTreeMap<DeploymentId, Fingerprint>,
    probe: &dyn EnvProbe,
) -> Result<PlanResult, PlanError> {
    // Model paths stay relative to the project: `apply` makes them absolute at
    // start, so no plan or state carries an absolute project path.
    let _ = project;
    amicu_engine::planner::build_plan_with(
        graph,
        prior,
        &amicu_engine::planner::PlanInputs {
            project_root: None,
            recorded,
            probe,
        },
    )
}

/// Reads the image and binary a deployment would run with from the real
/// environment (`AMICU_MODAL_*_IMAGE`, `PATH`).
pub(crate) struct HostEnvProbe;

impl HostEnvProbe {
    fn is_modal(hardware: Option<&HardwareProfile>) -> bool {
        hardware.is_some_and(|h| h.provider == "modal")
    }
}

impl EnvProbe for HostEnvProbe {
    fn image(&self, d: &Deployment, hardware: Option<&HardwareProfile>) -> Fact {
        if !Self::is_modal(hardware) {
            return Fact::NotApplicable;
        }
        let kind = amicu_runtime::RuntimeKind::of(&d.runtime);
        Fact::Known(
            std::env::var(amicu_modal::supervisor::image_env_var(kind))
                .unwrap_or_else(|_| amicu_modal::supervisor::default_image(kind).to_owned()),
        )
    }

    fn binary(&self, d: &Deployment, hardware: Option<&HardwareProfile>) -> Fact {
        if Self::is_modal(hardware) {
            return Fact::NotApplicable;
        }
        let name = match amicu_runtime::RuntimeKind::of(&d.runtime) {
            amicu_runtime::RuntimeKind::LlamaCpp => "llama-server",
            amicu_runtime::RuntimeKind::Vllm => "vllm",
        };
        match amicu_llamacpp::HostRunner.which(name) {
            Some(p) => Fact::Known(
                std::path::absolute(p)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
            None => Fact::Unknown,
        }
    }
}
