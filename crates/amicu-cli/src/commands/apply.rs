//! `amicu apply [--auto-approve]` — plan, approve, then start deployments.

use std::io::Write;

use amicu_core::graph::Graph;
use amicu_llamacpp::{LlamaCppAdapter, host_supervisor};
use amicu_runtime::{EndpointRegistry, RuntimeAdapter, RuntimeHandle};

use crate::OutputFormat;
use crate::catalog::build_catalog;
use crate::error::CliError;
use crate::planner::{PlanResult, build_plan};
use crate::project::{LocalState, Project};
use crate::secret::EnvSecretResolver;
use crate::write_json;

/// Recompute the plan, gate on approval, apply, and persist state.
pub(crate) fn run(
    project: &Project,
    auto_approve: bool,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let graph = project.load_graph()?;
    let prior_state = project.load_state()?;
    let prior_graph = prior_state.as_ref().map(|s| s.graph.clone());
    let result = build_plan(&graph, prior_graph.as_ref())?;

    if result.has_errors() {
        writeln!(
            out,
            "Cannot apply: configuration has errors. Run `amicu validate`."
        )
        .map_err(|e| CliError::io("write failed", e))?;
        return Ok(1);
    }

    if result.items.is_empty() {
        writeln!(out, "No changes to apply.").map_err(|e| CliError::io("write failed", e))?;
        return Ok(0);
    }

    // Safety rule: never apply without showing the plan and getting approval.
    crate::commands::plan::print_human(&result, out)?;

    if !auto_approve && !prompt_approval(out)? {
        writeln!(out, "Apply cancelled.").map_err(|e| CliError::io("write failed", e))?;
        return Ok(0);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| CliError::io("cannot start async runtime", e))?;
    let handles = rt.block_on(apply_all(&graph, &result, prior_state.as_ref()))?;

    project.save_state(&LocalState {
        version: LocalState::VERSION.to_owned(),
        graph,
        endpoints: handles.clone(),
    })?;

    report(&handles, output, out)
}

/// Start every actionable deployment, register endpoints, stop removed ones.
async fn apply_all(
    graph: &Graph,
    result: &PlanResult,
    prior: Option<&LocalState>,
) -> Result<Vec<RuntimeHandle>, CliError> {
    let adapter = LlamaCppAdapter::new(Box::new(build_catalog(graph)), host_supervisor());
    let resolver = EnvSecretResolver;
    let registry = EndpointRegistry::new();
    let mut handles = Vec::new();

    for item in &result.items {
        if let Some(launch) = &item.launch {
            let handle = adapter.apply(launch, &resolver).await?;
            registry.register(&handle);
            handles.push(handle);
        } else {
            // Destroy: stop the prior instance if we still hold its handle.
            if let Some(prev) = prior.and_then(|s| find_handle(s, &item.deployment)) {
                adapter.stop(prev).await?;
                registry.deregister(&item.deployment);
            }
        }
    }

    for policy in &graph.routing_policies {
        registry.register_policy(policy.clone());
    }

    Ok(handles)
}

fn find_handle<'a>(
    state: &'a LocalState,
    deployment: &amicu_core::id::DeploymentId,
) -> Option<&'a RuntimeHandle> {
    state.endpoints.iter().find(|h| &h.deployment == deployment)
}

fn report(
    handles: &[RuntimeHandle],
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    if output.is_json() {
        write_json(out, handles)?;
    } else {
        writeln!(out, "\nApplied {} deployment(s):", handles.len())
            .map_err(|e| CliError::io("write failed", e))?;
        for h in handles {
            writeln!(out, "  {} -> {}", h.deployment, h.endpoint)
                .map_err(|e| CliError::io("write failed", e))?;
        }
    }
    Ok(0)
}

fn prompt_approval(out: &mut dyn Write) -> Result<bool, CliError> {
    write!(out, "\nApply these changes? [y/N] ").map_err(|e| CliError::io("write failed", e))?;
    out.flush().map_err(|e| CliError::io("write failed", e))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| CliError::io("cannot read approval", e))?;
    Ok(approved(&line))
}

/// Whether a prompt response approves the apply.
pub(crate) fn approved(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
