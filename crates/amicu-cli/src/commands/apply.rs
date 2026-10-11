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
use crate::lock::StateLock;
use crate::planner::{PlanError, PlanResult, build_plan};
use crate::project::{LocalState, Project};
use crate::secret::EnvSecretResolver;
use crate::write_json;

/// Recompute the plan, gate on approval, apply, and persist state.
pub(crate) fn run(
    project: &Project,
    auto_approve: bool,
    lock_timeout: Option<std::time::Duration>,
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

    // Hold the state lock for the whole read -> refresh -> plan -> apply ->
    // save section, so a concurrent apply or `modal stop` cannot interleave
    // (amicu-planning#178). Released when `_lock` drops on return.
    let _lock = StateLock::acquire(&project.state_dir(), "apply", lock_timeout)?;

    let (prior_state, pruned) =
        crate::refresh::load_refreshed(project, &crate::refresh::ModalLookup, output, out)?;
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
        // The refresh may have dropped apps that are gone from Modal and from
        // the config; persist that so the drift note does not repeat forever.
        if pruned && let Some(state) = &prior_state {
            project.save_state(state)?;
        }
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
        crate::commands::plan::print_human(&result, &graph, preview)?;
        if !auto_approve && !prompt_approval(preview)? {
            writeln!(preview, "Apply cancelled.").map_err(|e| CliError::io("write failed", e))?;
            return Ok(0);
        }
    }

    let rt = crate::commands::build_runtime()?;
    let handles = rt.block_on(apply_all(&graph, &result, prior_state.as_ref()))?;

    let prior_endpoints = prior_state.as_ref().map_or(&[][..], |s| &s.endpoints[..]);
    let endpoints = merge_endpoints(prior_endpoints, &graph, &result, &handles);
    project.save_state(&LocalState {
        version: LocalState::VERSION.to_owned(),
        graph,
        endpoints,
    })?;

    report(&handles, output, out)
}

/// The endpoints to record after an apply: the prior (refreshed) endpoints of
/// deployments that are still in `graph` and were not destroyed, plus this
/// run's `handles` (which replace any prior entry for the same deployment).
/// Pure so it is unit tested without a network (amicu-planning#176).
fn merge_endpoints(
    prior: &[RuntimeHandle],
    graph: &Graph,
    result: &PlanResult,
    handles: &[RuntimeHandle],
) -> Vec<RuntimeHandle> {
    let destroyed = |h: &RuntimeHandle| {
        result
            .items
            .iter()
            .any(|i| i.launch.is_none() && i.deployment == h.deployment)
    };
    let mut merged: Vec<RuntimeHandle> = prior
        .iter()
        .filter(|h| {
            find_deployment(graph, &h.deployment).is_some()
                && !destroyed(h)
                && !handles.iter().any(|n| n.deployment == h.deployment)
        })
        .cloned()
        .collect();
    merged.extend(handles.iter().cloned());
    merged
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
    let supervisor = supervisor.with_scaling(deployment.scaling.as_ref());
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

#[cfg(test)]
mod merge_tests {
    use super::merge_endpoints;
    use crate::planner::build_plan;
    use amicu_runtime::RuntimeHandle;

    const CONFIG: &str = r#"resource "model" "m" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-7B-Instruct"
  }
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "4GiB"
  }
}

resource "hardware_profile" "hw" {
  provider = "local_apple"
  memory   = "16GiB"
}

resource "deployment" "alpha" {
  model    = model.m.id
  hardware = hardware_profile.hw.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}

resource "deployment" "beta" {
  model    = model.m.id
  hardware = hardware_profile.hw.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8001
    n_gpu_layers   = -1
  }
}
"#;

    fn handle(graph: &amicu_core::graph::Graph, name: &str, endpoint: &str) -> RuntimeHandle {
        RuntimeHandle {
            id: format!("unit-{name}"),
            deployment: graph
                .deployments
                .iter()
                .find(|d| d.id.as_str() == name)
                .expect("deployment in fixture")
                .id
                .clone(),
            endpoint: endpoint.to_owned(),
        }
    }

    #[test]
    fn unchanged_deployment_keeps_its_endpoint_and_changed_one_is_replaced() {
        let prior_graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let prior = vec![
            handle(&prior_graph, "alpha", "http://old-alpha"),
            handle(&prior_graph, "beta", "http://old-beta"),
        ];
        // Only beta changes (port).
        let graph = amicu_hcl::parse_graph(&CONFIG.replace("8001", "9001")).unwrap();
        let result = build_plan(&graph, Some(&prior_graph)).unwrap();
        assert_eq!(result.items.len(), 1, "only beta is actionable");

        let new_beta = handle(&graph, "beta", "http://new-beta");
        let merged = merge_endpoints(&prior, &graph, &result, std::slice::from_ref(&new_beta));
        let ends: Vec<_> = merged
            .iter()
            .map(|h| (h.deployment.as_str(), h.endpoint.as_str()))
            .collect();
        assert_eq!(
            ends,
            vec![("alpha", "http://old-alpha"), ("beta", "http://new-beta")]
        );
    }

    #[test]
    fn destroyed_deployment_endpoint_is_dropped() {
        let prior_graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let prior = vec![
            handle(&prior_graph, "alpha", "http://old-alpha"),
            handle(&prior_graph, "beta", "http://old-beta"),
        ];
        // Remove beta from the config.
        let idx = CONFIG.find("resource \"deployment\" \"beta\"").unwrap();
        let graph = amicu_hcl::parse_graph(&CONFIG[..idx]).unwrap();
        let result = build_plan(&graph, Some(&prior_graph)).unwrap();
        assert_eq!(result.items.len(), 1, "beta is destroyed");

        let merged = merge_endpoints(&prior, &graph, &result, &[]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].deployment.as_str(), "alpha");
    }
}
