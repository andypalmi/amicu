//! `amicu destroy [--target NAME]... [--auto-approve]` - tear deployments down.
//!
//! No second teardown path: this plans against an empty desired set (so the
//! planner emits a destroy action for everything in state) and hands the plan
//! to the same executor `apply` uses, which stops the instances, drops their
//! endpoints and rewrites state.

use std::fmt::Write as _;
use std::io::Write;

use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;

use crate::OutputFormat;
use crate::commands::apply::{Env, Op, execute, prompt_approval_from};
use crate::error::CliError;
use crate::lock::StateLock;
use crate::planner::{PlanError, PlanResult};
use crate::project::{LocalState, Project};
use crate::write_json;

/// Plan, confirm, destroy, and persist state.
#[allow(clippy::too_many_arguments)] // mirrors the CLI flags
pub(crate) fn run(
    project: &Project,
    targets: &[String],
    auto_approve: bool,
    lock_timeout: Option<std::time::Duration>,
    forget_unconfirmed: bool,
    purge: bool,
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
        &Env::real(forget_unconfirmed, purge),
    )
}

/// [`run`] with the approval answer read from `input`.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // run() plus the injected input and env
pub(crate) fn run_with(
    project: &Project,
    targets: &[String],
    auto_approve: bool,
    lock_timeout: Option<std::time::Duration>,
    output: OutputFormat,
    out: &mut dyn Write,
    input: &mut dyn std::io::BufRead,
    env: &Env,
) -> Result<u8, CliError> {
    // `--forget-unconfirmed` drops records WITHOUT confirming a stop, so it
    // only ever applies to targets the user names explicitly.
    if env.forget_unconfirmed && targets.is_empty() {
        return Err(CliError::Message(
            "--forget-unconfirmed requires at least one explicit --target; it is never applied \
             to everything at once"
                .to_owned(),
        ));
    }

    // Same locking discipline as `apply`: read -> refresh -> plan -> destroy ->
    // save under one state lock.
    let _lock = StateLock::acquire(&project.state_dir(), "destroy", lock_timeout)?;

    // Validate --target against the raw recorded state first, so a typo fails
    // before the Modal drift refresh makes any network call or prints drift.
    let raw = project.load_state()?;
    let raw_graph = raw.as_ref().map_or_else(Graph::new, |s| s.graph.clone());
    let raw_orphans = orphan_ids(raw.as_ref());
    if let Err(DestroyPlanError::UnknownTarget(msg)) =
        plan_destroy(&raw_graph, &raw_orphans, targets)
    {
        return Err(CliError::Message(msg));
    }

    let (prior_state, pruned) = crate::refresh::load_refreshed(project, env.modal, output, out)?;
    let empty = Graph::new();
    let prior_graph = prior_state.as_ref().map_or(&empty, |s| &s.graph);
    let orphans = orphan_ids(prior_state.as_ref());

    // Targets the refresh pruned (their Modal app is gone) are not "unknown":
    // they were in state, and no longer exist remotely.
    let gone: Vec<&str> = targets
        .iter()
        .map(String::as_str)
        .filter(|t| {
            !prior_graph.deployments.iter().any(|d| d.id.as_str() == *t)
                && !orphans.iter().any(|o| o.as_str() == *t)
        })
        .collect();
    if !gone.is_empty() {
        return Err(CliError::Message(format!(
            "deployment(s) already gone remotely: {}; the refresh dropped them from state, \
             so there is nothing left to stop",
            gone.join(", ")
        )));
    }

    let result = match plan_destroy(prior_graph, &orphans, targets) {
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
        // Files left by an earlier plain destroy are still tracked: list them,
        // or with `--purge` remove them.
        return Leftovers::of(prior_state.as_ref(), &result, env.purge, targets)?
            .finish(project, env, output, out);
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
        if env.forget_unconfirmed {
            // Exactly what may be forgotten without a confirmed stop.
            let mut lines = Vec::new();
            for r in crate::commands::apply::effective_stops(prior_state.as_ref())
                .iter()
                .filter(|r| {
                    r.backend.is_modal()
                        && r.app_id.is_none()
                        && targets.iter().any(|t| t == r.deployment.as_str())
                })
            {
                lines.push(format!("  {} (Modal app '{}')", r.deployment, r.identity));
            }
            if !lines.is_empty() {
                writeln!(
                    preview,
                    "--forget-unconfirmed: if Modal cannot find these apps they will be \
                     forgotten WITHOUT confirming they are stopped:\n{}",
                    lines.join("\n")
                )
                .map_err(|e| CliError::io("write failed", e))?;
            }
        }
        if !auto_approve {
            crate::commands::apply::require_interactive(env, "destroy")?;
        }
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
    // What the destroyed deployments leave behind, read from their records
    // BEFORE the stop removes those records.
    let leftovers = Leftovers::of(prior_state.as_ref(), &result, env.purge, targets)?;
    let (_, state) = execute(
        project,
        remaining,
        &result,
        prior_state.as_ref(),
        env,
        Op::Destroy,
    )?;

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
    leftovers.finish(project, env, output, out)
}

/// Secondary resources of destroyed deployments that a stop does not remove.
#[derive(Default)]
struct Leftovers {
    logs: Vec<String>,
    volumes: Vec<String>,
}

impl Leftovers {
    /// With `--purge`, every recorded log path is validated here, BEFORE
    /// anything is stopped: one that is not exactly the record's own log file
    /// in amicu's log directory refuses the whole command (nothing changed).
    fn of(
        prior: Option<&LocalState>,
        result: &PlanResult,
        purge: bool,
        targets: &[String],
    ) -> Result<Self, CliError> {
        let mut me = Self::default();
        // Files an earlier plain destroy left behind and state still tracks.
        for p in prior.map_or(&[][..], |s| &s.purge_pending[..]) {
            if !targets.is_empty() && !targets.iter().any(|t| t == p.deployment.as_str()) {
                continue;
            }
            if purge && let Err(why) = p.log_paths() {
                return Err(CliError::Message(format!(
                    "the recorded leftover logs of destroyed deployment '{}' include a file \
                     that --purge will not delete: {why}. Check the state file; nothing was \
                     changed",
                    p.deployment
                )));
            }
            for l in &p.logs {
                if !me.logs.contains(l) {
                    me.logs.push(l.clone());
                }
            }
        }
        for r in crate::commands::apply::effective_stops(prior)
            .iter()
            .filter(|r| result.items.iter().any(|i| i.deployment == r.deployment))
        {
            if purge && let Err(why) = r.log_paths() {
                return Err(CliError::Message(format!(
                    "{} has a recorded log file that --purge will not delete: {why}. Check the \
                     state file; nothing was changed",
                    r.describe()
                )));
            }
            for l in &r.logs {
                if !me.logs.contains(l) {
                    me.logs.push(l.clone());
                }
            }
            for v in &r.volumes {
                if !me.volumes.contains(v) {
                    me.volumes.push(v.clone());
                }
            }
        }
        Ok(me)
    }

    /// With `--purge`, delete the (already validated) log files. Modal
    /// volumes are shared by every project and app in the workspace, so they
    /// are NEVER deleted: they are listed, with the manual command. In JSON
    /// mode stdout stays the list of destroyed deployments and this note goes
    /// to stderr. The deployments are already stopped and out of state, so a
    /// failure is reported (exit 1) without touching that.
    fn finish(
        self,
        project: &Project,
        env: &Env,
        output: OutputFormat,
        out: &mut dyn Write,
    ) -> Result<u8, CliError> {
        let mut failures = Vec::new();
        let mut kept_logs = self.logs.clone();
        if env.purge {
            kept_logs.clear();
            for l in &self.logs {
                match std::fs::remove_file(l) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        failures.push(format!("log file {l}: {e}"));
                        kept_logs.push(l.clone());
                    }
                }
            }
        }
        if env.purge && kept_logs.len() < self.logs.len() {
            // Stop tracking exactly the files that are gone; one that could not
            // be removed stays recorded for the next `--purge`.
            let removed: Vec<String> = self
                .logs
                .iter()
                .filter(|l| !kept_logs.contains(l))
                .cloned()
                .collect();
            forget_purged(project, &removed)?;
        }
        let mut note = String::new();
        if !kept_logs.is_empty() {
            note.push_str("Left in place (remove with `amicu destroy --purge`):\n");
            for l in &kept_logs {
                let _ = writeln!(note, "  log file {l}");
            }
        }
        if !self.volumes.is_empty() {
            note.push_str(
                "Shared Modal cache volumes left in place (never deleted by amicu: other projects \
                 or apps in the workspace may use them; delete one only when you are sure none \
                 does):\n",
            );
            for v in &self.volumes {
                let _ = writeln!(note, "  modal volume delete {v}");
            }
        }
        if env.purge && failures.is_empty() && !self.logs.is_empty() {
            note.push_str("Purged the log files of the destroyed deployments.\n");
        }
        if !note.is_empty() {
            let r = if output.is_json() {
                std::io::stderr().write_all(note.as_bytes())
            } else {
                out.write_all(note.as_bytes())
            };
            r.map_err(|e| CliError::io("write failed", e))?;
        }
        if failures.is_empty() {
            return Ok(0);
        }
        Err(CliError::Message(format!(
            "the deployments were destroyed, but purge could not remove: {}. Remove them by \
             hand",
            failures.join("; ")
        )))
    }
}

/// Drop `removed` log files from state's purge-pending records.
fn forget_purged(project: &Project, removed: &[String]) -> Result<(), CliError> {
    let Some(mut state) = project.load_state()? else {
        return Ok(());
    };
    for p in &mut state.purge_pending {
        p.logs.retain(|l| !removed.contains(l));
    }
    state.purge_pending.retain(|p| !p.logs.is_empty());
    project.save_state(&state)
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

/// Deployments that have stop records but are no longer in the graph: their
/// instances may still be running, so destroy must act on them too.
fn orphan_ids(state: Option<&LocalState>) -> Vec<DeploymentId> {
    let Some(state) = state else {
        return Vec::new();
    };
    let mut out: Vec<DeploymentId> = Vec::new();
    for r in &state.stops {
        if !state.graph.deployments.iter().any(|d| d.id == r.deployment)
            && !out.contains(&r.deployment)
        {
            out.push(r.deployment.clone());
        }
    }
    out
}

/// Plan the destruction of everything in `prior` and of the `orphans` known
/// only from their stop records, or only `targets` when non-empty. Planned
/// against an empty desired graph (the planner emits a destroy for each prior
/// deployment), then narrowed to `targets`.
pub(crate) fn plan_destroy(
    prior: &Graph,
    orphans: &[DeploymentId],
    targets: &[String],
) -> Result<PlanResult, DestroyPlanError> {
    let known: Vec<&str> = prior
        .deployments
        .iter()
        .map(|d| d.id.as_str())
        .chain(orphans.iter().map(DeploymentId::as_str))
        .collect();
    let unknown: Vec<&str> = targets
        .iter()
        .map(String::as_str)
        .filter(|t| !known.contains(t))
        .collect();
    if !unknown.is_empty() {
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
    for id in orphans {
        let item = amicu_engine::planner::destroy_item(id);
        result.plan.actions.push(item.action.clone());
        result.items.push(item);
    }
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
