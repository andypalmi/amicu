//! `amicu destroy [--target NAME]... [--auto-approve]` - tear deployments down.
//!
//! No second teardown path: this plans against an empty desired set (so the
//! planner emits a destroy action for everything in state) and hands the plan
//! to the same executor `apply` uses, which stops the instances, drops their
//! endpoints and rewrites state.

use std::io::Write;

use amicu_core::graph::Graph;

use crate::OutputFormat;
use crate::commands::apply::{HostSupervisorFactory, execute, prompt_approval_from};
use crate::error::CliError;
use crate::lock::StateLock;
use crate::planner::{PlanError, PlanResult};
use crate::project::Project;
use crate::write_json;

/// Plan, confirm, destroy, and persist state.
pub(crate) fn run(
    project: &Project,
    targets: &[String],
    auto_approve: bool,
    lock_timeout: Option<std::time::Duration>,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    run_with(
        project,
        targets,
        auto_approve,
        lock_timeout,
        output,
        out,
        &mut std::io::stdin().lock(),
        &amicu_llamacpp::host_supervisor,
    )
}

/// [`run`] with the approval answer read from `input`.
#[allow(clippy::too_many_arguments)] // run() plus the injected input and host
pub(crate) fn run_with(
    project: &Project,
    targets: &[String],
    auto_approve: bool,
    lock_timeout: Option<std::time::Duration>,
    output: OutputFormat,
    out: &mut dyn Write,
    input: &mut dyn std::io::BufRead,
    host: &HostSupervisorFactory,
) -> Result<u8, CliError> {
    // Same locking discipline as `apply`: read -> refresh -> plan -> destroy ->
    // save under one state lock.
    let _lock = StateLock::acquire(&project.state_dir(), "destroy", lock_timeout)?;

    // Validate --target against the raw recorded state first, so a typo fails
    // before the Modal drift refresh makes any network call or prints drift.
    let raw = project.load_state()?;
    let raw_graph = raw.as_ref().map_or_else(Graph::new, |s| s.graph.clone());
    if let Err(DestroyPlanError::UnknownTarget(msg)) = plan_destroy(&raw_graph, targets) {
        return Err(CliError::Message(msg));
    }

    let (prior_state, pruned) =
        crate::refresh::load_refreshed(project, &crate::refresh::ModalLookup, output, out)?;
    let empty = Graph::new();
    let prior_graph = prior_state.as_ref().map_or(&empty, |s| &s.graph);

    // Targets the refresh pruned (their Modal app is gone) are not "unknown":
    // they were in state, and no longer exist remotely.
    let gone: Vec<&str> = targets
        .iter()
        .map(String::as_str)
        .filter(|t| !prior_graph.deployments.iter().any(|d| d.id.as_str() == *t))
        .collect();
    if !gone.is_empty() {
        return Err(CliError::Message(format!(
            "deployment(s) already gone remotely: {}; the refresh dropped them from state, \
             so there is nothing left to stop",
            gone.join(", ")
        )));
    }

    let result = match plan_destroy(prior_graph, targets) {
        Ok(result) => result,
        Err(DestroyPlanError::UnknownTarget(msg)) => return Err(CliError::Message(msg)),
        Err(DestroyPlanError::Plan(PlanError(report))) => {
            let io = |r: std::io::Result<()>| r.map_err(|e| CliError::io("write failed", e));
            io(writeln!(out, "Cannot plan destroy:"))?;
            for d in &report.diagnostics {
                io(writeln!(out, "  {}: {}", d.code, d.message))?;
            }
            return Ok(1);
        }
    };

    if result.items.is_empty() {
        // The refresh may have dropped apps that are gone from Modal; persist
        // that so the drift note does not repeat forever.
        if pruned && let Some(state) = &prior_state {
            project.save_state(state)?;
        }
        if output.is_json() {
            write_json(out, &Vec::<&str>::new())?;
        } else {
            writeln!(out, "Nothing to destroy.").map_err(|e| CliError::io("write failed", e))?;
        }
        return Ok(0);
    }

    // Same safety rule as `apply`: show the plan and get approval first; in
    // JSON mode stdout carries only the final result, so preview goes to stderr.
    let cancelled = {
        let mut stderr = std::io::stderr();
        let preview: &mut dyn Write = if output.is_json() {
            &mut stderr
        } else {
            &mut *out
        };
        crate::commands::plan::print_human(&result, prior_graph, preview)?;
        if !auto_approve && !prompt_approval_from(preview, "Destroy these deployments?", input)? {
            writeln!(preview, "Destroy cancelled.").map_err(|e| CliError::io("write failed", e))?;
            true
        } else {
            false
        }
    };
    if cancelled {
        if output.is_json() {
            write_json(out, &Vec::<&str>::new())?;
        }
        return Ok(0);
    }

    let remaining = remaining_graph(prior_graph, &result);
    let (_, state) = execute(project, remaining, &result, prior_state.as_ref(), host)?;

    let destroyed: Vec<&str> = result.items.iter().map(|i| i.deployment.as_str()).collect();
    if output.is_json() {
        write_json(out, &destroyed)?;
    } else {
        let io = |r: std::io::Result<()>| r.map_err(|e| CliError::io("write failed", e));
        io(writeln!(
            out,
            "\nDestroyed {} deployment(s):",
            destroyed.len()
        ))?;
        for id in &destroyed {
            io(writeln!(out, "  {id}"))?;
        }
        if !state.graph.deployments.is_empty() {
            io(writeln!(
                out,
                "{} deployment(s) remain in state.",
                state.graph.deployments.len()
            ))?;
        }
    }
    Ok(0)
}

/// Why [`plan_destroy`] produced no plan.
#[derive(Debug)]
pub(crate) enum DestroyPlanError {
    /// A `--target` names no deployment in state; nothing was planned.
    UnknownTarget(String),
    /// The planner itself failed.
    Plan(PlanError),
}

impl From<PlanError> for DestroyPlanError {
    fn from(e: PlanError) -> Self {
        Self::Plan(e)
    }
}

/// Plan the destruction of everything in `prior`, or only `targets` when
/// non-empty. Planned against an empty desired graph (the planner emits a
/// destroy for each prior deployment), then narrowed to `targets`.
pub(crate) fn plan_destroy(
    prior: &Graph,
    targets: &[String],
) -> Result<PlanResult, DestroyPlanError> {
    let unknown: Vec<&str> = targets
        .iter()
        .map(String::as_str)
        .filter(|t| !prior.deployments.iter().any(|d| d.id.as_str() == *t))
        .collect();
    if !unknown.is_empty() {
        let known: Vec<&str> = prior.deployments.iter().map(|d| d.id.as_str()).collect();
        let known = if known.is_empty() {
            "(none)".to_owned()
        } else {
            known.join(", ")
        };
        return Err(DestroyPlanError::UnknownTarget(format!(
            "unknown --target deployment(s): {}; deployments in state: {known}. \
             Nothing was changed",
            unknown.join(", ")
        )));
    }
    let mut result = amicu_engine::planner::build_plan_in(&Graph::new(), Some(prior), None)?;
    if !targets.is_empty() {
        result
            .items
            .retain(|i| targets.iter().any(|t| t == i.deployment.as_str()));
        result.plan.actions = result.items.iter().map(|i| i.action.clone()).collect();
    }
    Ok(result)
}

/// The state graph after destroying `result`'s deployments: `prior` without
/// them (an empty graph when everything is destroyed).
fn remaining_graph(prior: &Graph, result: &PlanResult) -> Graph {
    let mut graph = prior.clone();
    graph
        .deployments
        .retain(|d| !result.items.iter().any(|i| i.deployment == d.id));
    if graph.deployments.is_empty() {
        return Graph::new();
    }
    graph
}
