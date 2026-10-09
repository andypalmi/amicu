//! `amicu apply [--auto-approve]` - plan, approve, then start deployments.

use std::io::Write;

use amicu_core::adapter::AdapterError;
use amicu_core::diagnostic::{DiagnosticReport, Severity};
use amicu_core::graph::Graph;
use amicu_core::resource::Runtime;
use amicu_llamacpp::{Catalog, LlamaCppAdapter, host_supervisor};
use amicu_modal::ModalSupervisor;
use amicu_modal::supervisor::app_name_for;
use amicu_runtime::{EndpointRegistry, RuntimeAdapter, RuntimeHandle};
use amicu_vllm::VllmAdapter;

use crate::OutputFormat;
use crate::catalog::build_catalog;
use crate::checks::report_for;
use crate::error::CliError;
use crate::planner::{PlanError, PlanResult, build_plan};
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

    // Surface structural errors through the same diagnostic path as `validate`,
    // before `build_plan` can hard-error on them.
    let diagnostics = report_for(&graph);
    if !diagnostics.valid {
        emit_report(&diagnostics, output, out)?;
        return Ok(1);
    }

    let prior_state = project.load_state()?;
    let prior_graph = prior_state.as_ref().map(|s| s.graph.clone());
    let result = match build_plan(&graph, prior_graph.as_ref()) {
        Ok(result) => result,
        // See `commands::plan::run`'s matching arm: `build_plan` hands back
        // every collected diagnostic instead of a bare adapter error.
        Err(PlanError(report)) => {
            emit_report(&report, output, out)?;
            return Ok(1);
        }
    };

    if result.has_errors() {
        emit_report(
            &DiagnosticReport::new(result.diagnostics.clone()),
            output,
            out,
        )?;
        return Ok(1);
    }

    if result.items.is_empty() {
        writeln!(out, "No changes to apply.").map_err(|e| CliError::io("write failed", e))?;
        return Ok(0);
    }

    // Safety rule: never apply without showing the plan and getting approval.
    // In JSON mode stdout must carry only the final handles JSON, so route the
    // preview and prompt to stderr instead.
    {
        let mut stderr = std::io::stderr();
        let preview: &mut dyn Write = if output.is_json() {
            &mut stderr
        } else {
            &mut *out
        };
        crate::commands::plan::print_human(&result, preview)?;
        if !auto_approve && !prompt_approval(preview)? {
            writeln!(preview, "Apply cancelled.").map_err(|e| CliError::io("write failed", e))?;
            return Ok(0);
        }
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

/// Hardware profile `provider` value that routes a deployment through
/// [`ModalSupervisor`] instead of the local host supervisor
/// (amicu-planning#158 PR 5).
const MODAL_PROVIDER: &str = "modal";

/// The per-deployment supervisor `apply_all` routes to, decided purely from
/// a deployment's runtime family and its hardware profile's `provider`.
/// Factored out of the dispatch loop so the routing decision itself is unit
/// tested (below) without executing any supervisor - no process spawn, no
/// network call, regardless of which variant a case selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    /// The existing local launchd/systemd path (`host_supervisor()`).
    Host,
    /// `provider = "modal"`, vLLM or llama.cpp: route through
    /// [`ModalSupervisor`] (the runtime picks the adapter and the image).
    Modal,
}

/// Computes [`Dispatch`] for one deployment's runtime family and hardware
/// profile provider.
fn dispatch_for(runtime: amicu_runtime::RuntimeKind, provider: Option<&str>) -> Dispatch {
    use amicu_runtime::RuntimeKind::{LlamaCpp, Vllm};
    match (runtime, provider) {
        (LlamaCpp | Vllm, Some(MODAL_PROVIDER)) => Dispatch::Modal,
        (LlamaCpp | Vllm, _) => Dispatch::Host,
    }
}

/// Builds the [`ModalSupervisor`] for one deployment from its hardware profile.
/// A llama.cpp deployment also gets its model's Hugging Face GGUF source: the
/// container fetches the GGUF itself with `-hf` (a non-HF model is rejected
/// when the supervisor starts).
fn modal_supervisor(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &amicu_core::id::DeploymentId,
    runtime: amicu_runtime::RuntimeKind,
) -> Result<ModalSupervisor, CliError> {
    let deployment = find_deployment(graph, id).ok_or_else(|| {
        CliError::Message(format!(
            "deployment '{id}' missing from its own graph while dispatching to Modal"
        ))
    })?;
    let hardware = catalog
        .resolve_hardware(&deployment.hardware_profile)
        .ok_or_else(|| {
            CliError::Message(format!(
                "deployment '{id}': hardware profile '{}' not found in the catalog",
                deployment.hardware_profile
            ))
        })?;
    let supervisor = ModalSupervisor::for_hardware(app_name_for(id.as_str()), &hardware)
        .map_err(|e| CliError::Adapter(AdapterError::from_kind(e)))?;
    Ok(match (runtime, catalog.resolve_model(&deployment.model)) {
        (amicu_runtime::RuntimeKind::LlamaCpp, Some(resolved)) => {
            supervisor.with_gguf_model(&resolved.model)
        }
        _ => supervisor,
    })
}

/// Start every actionable deployment, register endpoints, stop removed ones.
async fn apply_all(
    graph: &Graph,
    result: &PlanResult,
    prior: Option<&LocalState>,
) -> Result<Vec<RuntimeHandle>, CliError> {
    // Dispatch to the adapter matching each deployment's own runtime family
    // (amicu-planning#152) - previously `apply` only ever started/stopped
    // llama.cpp instances. amicu-planning#158 PR 5 adds a second axis: a
    // vLLM deployment whose hardware profile has `provider = "modal"` is
    // routed through `ModalSupervisor` instead of the local launchd/systemd
    // `host_supervisor()` below - built fresh per such deployment (it needs
    // that deployment's own GPU/region), never reused across iterations.
    // A llama.cpp deployment on Modal takes the same route through a
    // `LlamaCppAdapter` (amicu-planning#160).
    let catalog = build_catalog(graph);
    let llama_adapter = LlamaCppAdapter::new(Box::new(catalog.clone()), host_supervisor());
    let vllm_adapter = VllmAdapter::new(Box::new(catalog.clone()), amicu_vllm::host_supervisor());
    let resolver = EnvSecretResolver;
    let registry = EndpointRegistry::new();
    let mut handles = Vec::new();

    for item in &result.items {
        if let Some(launch) = &item.launch {
            let dispatch = dispatch_for(launch.runtime, item.action.provider.as_deref());
            let handle = match (launch.runtime, dispatch) {
                (runtime, Dispatch::Modal) => {
                    let supervisor = modal_supervisor(graph, &catalog, &item.deployment, runtime)?;
                    match runtime {
                        amicu_runtime::RuntimeKind::Vllm => {
                            VllmAdapter::new(Box::new(catalog.clone()), Box::new(supervisor))
                                .apply(launch, &resolver)
                                .await?
                        }
                        amicu_runtime::RuntimeKind::LlamaCpp => {
                            LlamaCppAdapter::new(Box::new(catalog.clone()), Box::new(supervisor))
                                .apply(launch, &resolver)
                                .await?
                        }
                    }
                }
                (amicu_runtime::RuntimeKind::LlamaCpp, Dispatch::Host) => {
                    llama_adapter.apply(launch, &resolver).await?
                }
                (amicu_runtime::RuntimeKind::Vllm, Dispatch::Host) => {
                    vllm_adapter.apply(launch, &resolver).await?
                }
            };
            registry.register(&handle);
            handles.push(handle);
        } else {
            // Destroy: stop the prior instance if we still hold its handle.
            // Both `host_supervisor()` implementations manage the launchd/
            // systemd unit purely by the handle's own id/endpoint, so either
            // adapter stops it identically - dispatch on the prior graph's
            // declared runtime anyway, to keep this symmetric with the apply
            // path and future-proof against a supervisor that stops being
            // runtime-agnostic. A prior Modal vLLM deployment needs its own
            // `ModalSupervisor` too: `stop` only needs the handle's stored
            // Modal app id, not the hardware profile, so no GPU/region is
            // resolved here.
            if let Some(prev) = prior.and_then(|s| find_handle(s, &item.deployment)) {
                let prior_deployment =
                    prior.and_then(|s| find_deployment(&s.graph, &item.deployment));
                let prior_provider = prior.zip(prior_deployment).and_then(|(s, d)| {
                    s.graph
                        .hardware_profiles
                        .iter()
                        .find(|h| h.id == d.hardware_profile)
                        .map(|h| h.provider.as_str())
                });
                let prior_runtime_kind = prior_deployment
                    .map_or(amicu_runtime::RuntimeKind::LlamaCpp, |d| {
                        amicu_runtime::RuntimeKind::of(&d.runtime)
                    });
                match dispatch_for(prior_runtime_kind, prior_provider) {
                    Dispatch::Modal => {
                        let supervisor =
                            ModalSupervisor::new(app_name_for(item.deployment.as_str()));
                        // `stop` only needs the stored app id, so either adapter
                        // stops a Modal deployment identically.
                        match prior_runtime_kind {
                            amicu_runtime::RuntimeKind::Vllm => {
                                VllmAdapter::new(Box::new(catalog.clone()), Box::new(supervisor))
                                    .stop(prev)
                                    .await?;
                            }
                            amicu_runtime::RuntimeKind::LlamaCpp => {
                                LlamaCppAdapter::new(
                                    Box::new(catalog.clone()),
                                    Box::new(supervisor),
                                )
                                .stop(prev)
                                .await?;
                            }
                        }
                    }
                    Dispatch::Host => match prior_deployment.map(|d| &d.runtime) {
                        Some(Runtime::Vllm(_)) => vllm_adapter.stop(prev).await?,
                        _ => llama_adapter.stop(prev).await?,
                    },
                }
                registry.deregister(&item.deployment);
            }
        }
    }

    for policy in &graph.routing_policies {
        registry.register_policy(policy.clone());
    }

    Ok(handles)
}

fn emit_report(
    report: &DiagnosticReport,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    if output.is_json() {
        write_json(out, report)?;
    } else {
        writeln!(
            out,
            "Cannot apply: configuration has errors. Run `amicu validate`."
        )
        .map_err(|e| CliError::io("write failed", e))?;
        for d in &report.diagnostics {
            if d.severity == Severity::Error {
                writeln!(out, "  error [{}] {}", d.code, d.message)
                    .map_err(|e| CliError::io("write failed", e))?;
            }
        }
    }
    Ok(())
}

fn find_handle<'a>(
    state: &'a LocalState,
    deployment: &amicu_core::id::DeploymentId,
) -> Option<&'a RuntimeHandle> {
    state.endpoints.iter().find(|h| &h.deployment == deployment)
}

fn find_deployment<'a>(
    graph: &'a Graph,
    deployment: &amicu_core::id::DeploymentId,
) -> Option<&'a amicu_core::resource::Deployment> {
    graph.deployments.iter().find(|d| &d.id == deployment)
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

#[cfg(test)]
mod dispatch_tests {
    use super::{Dispatch, dispatch_for};
    use amicu_runtime::RuntimeKind;

    // amicu-planning#158 PR 5: `dispatch_for` is the whole of `apply_all`'s
    // provider-routing decision, factored out precisely so this exhausts it
    // without executing any supervisor (no process spawn, no network call).

    #[test]
    fn vllm_on_modal_selects_the_modal_supervisor() {
        assert_eq!(
            dispatch_for(RuntimeKind::Vllm, Some("modal")),
            Dispatch::Modal
        );
    }

    #[test]
    fn vllm_on_a_non_modal_provider_still_selects_the_host_supervisor() {
        for provider in [Some("runpod"), Some("local_apple"), Some("on_prem"), None] {
            assert_eq!(
                dispatch_for(RuntimeKind::Vllm, provider),
                Dispatch::Host,
                "{provider:?}"
            );
        }
    }

    #[test]
    fn llama_cpp_on_modal_selects_the_modal_supervisor() {
        assert_eq!(
            dispatch_for(RuntimeKind::LlamaCpp, Some("modal")),
            Dispatch::Modal
        );
    }

    #[test]
    fn llama_cpp_on_a_non_modal_provider_still_selects_the_host_supervisor() {
        for provider in [Some("runpod"), Some("local_apple"), Some("on_prem"), None] {
            assert_eq!(
                dispatch_for(RuntimeKind::LlamaCpp, provider),
                Dispatch::Host,
                "{provider:?}"
            );
        }
    }
}
