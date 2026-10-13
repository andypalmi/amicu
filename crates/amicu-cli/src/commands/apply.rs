//! `amicu apply [--auto-approve]` - plan, approve, then start deployments.

use std::io::Write;

use amicu_core::adapter::AdapterError;
use amicu_core::diagnostic::{DiagnosticReport, Severity};
use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{Deployment, Runtime, is_local_provider};
use amicu_core::secret::{SecretResolver, SecretValue};
use amicu_llamacpp::{Catalog, LlamaCppAdapter, host_supervisor};
use amicu_modal::ModalSupervisor;
use amicu_modal::supervisor::app_name_for;
use amicu_runtime::exposure::{CheckTiming, EndpointCheck, PROBE_PATH};
use amicu_runtime::{
    EndpointExposure, EndpointRegistry, GatewayAuth, LaunchPlan, RuntimeAdapter, RuntimeHandle,
};
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
    let mut progress = Progress::default();
    if let Err(err) = rt.block_on(apply_all(
        &graph,
        &result,
        prior_state.as_ref(),
        &mut progress,
    )) {
        return Err(record_partial(
            project,
            &graph,
            prior_state.as_ref(),
            &progress,
            err,
        ));
    }
    let handles = progress.started;

    let prior_endpoints = prior_state.as_ref().map_or(&[][..], |s| &s.endpoints[..]);
    let destroyed: Vec<DeploymentId> = result
        .items
        .iter()
        .filter(|i| i.launch.is_none())
        .map(|i| i.deployment.clone())
        .collect();
    let endpoints = merge_endpoints(prior_endpoints, &graph, &destroyed, &handles);
    let state = LocalState {
        version: LocalState::VERSION.to_owned(),
        graph,
        endpoints,
    };
    project.save_state(&state)?;

    report(&handles, &state.graph, output, out)
}

/// The endpoints to record after an apply: the prior (refreshed) endpoints of
/// deployments that are still in `graph` and were not destroyed, plus this
/// run's `handles` (which replace any prior entry for the same deployment).
/// Pure so it is unit tested without a network (amicu-planning#176).
fn merge_endpoints(
    prior: &[RuntimeHandle],
    graph: &Graph,
    destroyed: &[DeploymentId],
    handles: &[RuntimeHandle],
) -> Vec<RuntimeHandle> {
    let destroyed = |h: &RuntimeHandle| destroyed.contains(&h.deployment);
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

/// What `apply_all` has already done to the real world when it returns, so a
/// failure part-way through can still be recorded in state
/// (amicu-planning#203).
#[derive(Default)]
struct Progress {
    /// Deployments started and verified, in plan order.
    started: Vec<RuntimeHandle>,
    /// Deployments stopped because they were removed from the config.
    stopped: Vec<DeploymentId>,
    /// The deployment being started when `apply_all` failed, if any. Its
    /// state entry is left as it was and may no longer match reality.
    in_flight: Option<DeploymentId>,
}

/// The graph to persist after a partial apply: the prior graph, plus each
/// deployment that was started (with the model and hardware profile it was
/// started from), minus each one that was stopped. Deployments that were not
/// reached keep their prior definition, so the next `plan` still sees them
/// as pending.
fn partial_graph(prior: Option<&Graph>, graph: &Graph, progress: &Progress) -> Graph {
    let mut out = prior.cloned().unwrap_or_else(Graph::new);
    for h in &progress.started {
        let Some(d) = find_deployment(graph, &h.deployment) else {
            continue;
        };
        out.deployments.retain(|x| x.id != d.id);
        out.deployments.push(d.clone());
        // A model or hardware profile still referenced by a deployment that
        // was not (re)started keeps its prior definition: change detection
        // compares the resolved closure, so overwriting it would make that
        // deployment look unchanged and it would never be re-applied.
        let shared = |uses: &dyn Fn(&Deployment) -> bool| {
            prior.is_some_and(|p| {
                p.deployments.iter().any(|x| {
                    uses(x)
                        && !progress.stopped.contains(&x.id)
                        && !progress.started.iter().any(|s| s.deployment == x.id)
                })
            })
        };
        if let Some(m) = graph.models.iter().find(|m| m.id == d.model)
            && !(shared(&|x| x.model == m.id) && out.models.iter().any(|x| x.id == m.id))
        {
            out.models.retain(|x| x.id != m.id);
            out.models.push(m.clone());
        }
        if let Some(hw) = graph
            .hardware_profiles
            .iter()
            .find(|x| x.id == d.hardware_profile)
            && !(shared(&|x| x.hardware_profile == hw.id)
                && out.hardware_profiles.iter().any(|x| x.id == hw.id))
        {
            out.hardware_profiles.retain(|x| x.id != hw.id);
            out.hardware_profiles.push(hw.clone());
        }
    }
    out.deployments
        .retain(|d| !progress.stopped.contains(&d.id));
    out
}

/// Persist what a failed apply did get done, and return `err` annotated with
/// which deployments were started and recorded (amicu-planning#203). Without
/// this they would keep running (and billing) with no state entry, invisible
/// to `destroy` and re-created by the next `apply`.
fn record_partial(
    project: &Project,
    graph: &Graph,
    prior: Option<&LocalState>,
    progress: &Progress,
    err: CliError,
) -> CliError {
    if progress.started.is_empty() && progress.stopped.is_empty() {
        return err;
    }
    let partial = partial_graph(prior.map(|s| &s.graph), graph, progress);
    let prior_endpoints = prior.map_or(&[][..], |s| &s.endpoints[..]);
    let state = LocalState {
        version: LocalState::VERSION.to_owned(),
        endpoints: merge_endpoints(
            prior_endpoints,
            &partial,
            &progress.stopped,
            &progress.started,
        ),
        graph: partial,
    };
    let names = |ids: Vec<&str>| ids.join(", ");
    let started = names(
        progress
            .started
            .iter()
            .map(|h| h.deployment.as_str())
            .collect(),
    );
    let stopped = names(progress.stopped.iter().map(DeploymentId::as_str).collect());
    let mut done = Vec::new();
    if !started.is_empty() {
        done.push(format!("started: {started}"));
    }
    if !stopped.is_empty() {
        done.push(format!("stopped: {stopped}"));
    }
    let mut done = done.join("; ");
    if let Some(id) = &progress.in_flight {
        let note = format!(
            "; '{id}' failed mid-start, its state entry was left unchanged and may be out of \
             sync with what is running - check it directly (`launchctl list` / `systemctl \
             --user list-units` / `modal app list`) before re-running"
        );
        done.push_str(&note);
    }
    match project.save_state(&state) {
        Ok(()) => CliError::Message(format!(
            "{err}\n\napply stopped part-way; already done and recorded in state ({done}). \
             Fix the error and re-run `amicu apply` to finish the rest"
        )),
        Err(save_err) => CliError::Message(format!(
            "{err}\n\napply stopped part-way ({done}) and recording that in state FAILED: \
             {save_err}. Those deployments may still be running; stop them by hand"
        )),
    }
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

/// Providers `apply` can deploy to today, for the error message.
const SUPPORTED_PROVIDERS: &str = "local, local_apple, local_cpu, on_prem, modal";

/// Computes [`Dispatch`] for one deployment's runtime family and hardware
/// profile provider. `None` when `apply` has no path for that provider.
fn dispatch_for(runtime: amicu_runtime::RuntimeKind, provider: Option<&str>) -> Option<Dispatch> {
    use amicu_runtime::RuntimeKind::{LlamaCpp, Vllm};
    match (runtime, provider) {
        (LlamaCpp | Vllm, Some(MODAL_PROVIDER)) => Some(Dispatch::Modal),
        // A profile with no provider has always meant the local host.
        (LlamaCpp | Vllm, None) => Some(Dispatch::Host),
        (LlamaCpp | Vllm, Some(p)) if is_local_provider(p) => Some(Dispatch::Host),
        (LlamaCpp | Vllm, Some(_)) => None,
    }
}

/// [`dispatch_for`] as an apply-time error naming the unsupported provider.
fn require_dispatch(
    runtime: amicu_runtime::RuntimeKind,
    provider: Option<&str>,
    deployment: &amicu_core::id::DeploymentId,
) -> Result<Dispatch, CliError> {
    dispatch_for(runtime, provider).ok_or_else(|| {
        CliError::Message(format!(
            "deployment '{deployment}': provider \"{}\" is not supported by apply yet; \
             supported: {SUPPORTED_PROVIDERS}",
            provider.unwrap_or_default()
        ))
    })
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

/// Reject unsupported providers, and off-machine endpoints with no API key,
/// before anything is started or stopped. `amicu validate`/`plan` already
/// report the latter (`AMICU-POLICY-007`); this is the backstop for callers
/// that reach `apply` without them.
fn preflight(result: &PlanResult) -> Result<(), CliError> {
    for item in &result.items {
        if let Some(launch) = &item.launch {
            require_dispatch(
                launch.runtime,
                item.action.provider.as_deref(),
                &item.deployment,
            )?;
            if Deployment::requires_api_key(item.action.provider.as_deref().unwrap_or_default())
                && launch.api_key.is_none()
            {
                return Err(CliError::Message(format!(
                    "deployment '{}': provider \"{}\" serves the endpoint off-machine, so it \
                     needs an `api_key` (AMICU-POLICY-007); nothing was started",
                    item.deployment,
                    item.action.provider.as_deref().unwrap_or_default()
                )));
            }
        }
    }
    let any_modal = result.items.iter().any(|i| {
        i.launch.as_ref().is_some_and(|l| {
            dispatch_for(l.runtime, i.action.provider.as_deref()) == Some(Dispatch::Modal)
        })
    });
    if any_modal {
        require_modal_proxy_token(&amicu_modal::proxy_auth::TokenConfig::from_env())?;
    }
    Ok(())
}

/// Modal deployments are verified through the proxy; refuse to deploy without
/// the proxy token, since an anonymous-only check proves nothing about the
/// runtime's own key.
fn require_modal_proxy_token(cfg: &amicu_modal::proxy_auth::TokenConfig) -> Result<(), CliError> {
    use amicu_modal::proxy_auth::{TOKEN_ID_ENV_VAR, TOKEN_SECRET_ENV_VAR, TokenConfig};
    if matches!(cfg, TokenConfig::Complete(_)) {
        return Ok(());
    }
    Err(CliError::Message(format!(
        "Modal deployments need {TOKEN_ID_ENV_VAR} and {TOKEN_SECRET_ENV_VAR} set to valid values \
         so apply can verify the endpoint's API key through Modal's proxy; set them from your \
         keychain or shell environment (create a Proxy Auth Token in the Modal dashboard) - \
         nothing was started"
    )))
}

/// Start every actionable deployment, register endpoints, stop removed ones.
async fn apply_all(
    graph: &Graph,
    result: &PlanResult,
    prior: Option<&LocalState>,
    progress: &mut Progress,
) -> Result<(), CliError> {
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
    preflight(result)?;

    for item in &result.items {
        if let Some(launch) = &item.launch {
            let dispatch = require_dispatch(
                launch.runtime,
                item.action.provider.as_deref(),
                &item.deployment,
            )?;
            // Modal builds a per-deployment adapter; the host adapters are shared.
            let modal_adapter: Box<dyn RuntimeAdapter>;
            let adapter: &dyn RuntimeAdapter = match (launch.runtime, dispatch) {
                (runtime, Dispatch::Modal) => {
                    let supervisor = modal_supervisor(graph, &catalog, &item.deployment, runtime)?;
                    modal_adapter = match runtime {
                        amicu_runtime::RuntimeKind::Vllm => Box::new(VllmAdapter::new(
                            Box::new(catalog.clone()),
                            Box::new(supervisor),
                        )),
                        amicu_runtime::RuntimeKind::LlamaCpp => Box::new(LlamaCppAdapter::new(
                            Box::new(catalog.clone()),
                            Box::new(supervisor),
                        )),
                    };
                    modal_adapter.as_ref()
                }
                (amicu_runtime::RuntimeKind::LlamaCpp, Dispatch::Host) => &llama_adapter,
                (amicu_runtime::RuntimeKind::Vllm, Dispatch::Host) => &vllm_adapter,
            };
            progress.in_flight = Some(item.deployment.clone());
            let handle = adapter.apply(launch, &resolver).await?;
            // Shared post-deploy check, for every adapter: an endpoint that is
            // not protected as declared is torn down and apply fails.
            secure_endpoint(adapter, &handle, launch, dispatch, &resolver).await?;
            registry.register(&handle);
            progress.started.push(handle);
            progress.in_flight = None;
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
                // A prior provider this version cannot apply (state written
                // when such providers fell back to the host) is still stopped
                // through the host supervisor so nothing is left running.
                match dispatch_for(prior_runtime_kind, prior_provider).unwrap_or(Dispatch::Host) {
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
            progress.stopped.push(item.deployment.clone());
        }
    }

    for policy in &graph.routing_policies {
        registry.register_policy(policy.clone());
    }

    Ok(())
}

/// The shared post-deploy exposure check (amicu-planning#191), run on the
/// handle every adapter returns, whatever the provider.
///
/// - Loopback endpoint, no `api_key`: nothing to protect, skipped.
/// - Anything else must be protected by its key: an unauthenticated request
///   to `/v1/models` must be refused, and the declared key must work (for
///   Modal, with the proxy-token headers). A `2xx` for the anonymous request
///   means the endpoint is open, so it is stopped through the adapter that
///   started it and apply fails. A failed stop is reported as such, with the
///   command to run by hand.
async fn secure_endpoint(
    adapter: &dyn RuntimeAdapter,
    handle: &RuntimeHandle,
    launch: &LaunchPlan,
    dispatch: Dispatch,
    resolver: &dyn SecretResolver,
) -> Result<(), CliError> {
    let verdict = verify_handle(handle, launch, resolver).await;
    let Err(detail) = verdict else {
        return Ok(());
    };
    let hint = match dispatch {
        Dispatch::Modal => format!("`modal app stop {}`", handle.id),
        Dispatch::Host => format!(
            "`systemctl --user stop {id}` on Linux, or `launchctl unload \
             ~/Library/LaunchAgents/{id}.plist` on macOS",
            id = handle.id
        ),
    };
    let message = amicu_runtime::exposure::teardown_report(&detail, &handle.id, &hint, || async {
        adapter.stop(handle).await.map_err(|e| e.to_string())
    })
    .await;
    Err(CliError::Message(format!(
        "deployment '{}': {message}",
        handle.deployment
    )))
}

/// `Ok` when `handle` is protected as declared (or needs no check); `Err`
/// carries a secret-free description of what is wrong.
async fn verify_handle(
    handle: &RuntimeHandle,
    launch: &LaunchPlan,
    resolver: &dyn SecretResolver,
) -> Result<(), String> {
    let Some(key_ref) = &launch.api_key else {
        return match handle.exposure {
            EndpointExposure::Loopback => Ok(()),
            // Unreachable through `apply_all`'s preflight; fail closed anyway.
            EndpointExposure::Public { .. } => Err(
                "the endpoint is reachable off-machine but the deployment declares no `api_key`"
                    .to_owned(),
            ),
        };
    };
    let key = resolver
        .resolve(key_ref)
        .await
        .map_err(|e| format!("could not resolve the endpoint API key to verify it ({e})"))?;
    let timing = if handle.exposure.is_public() {
        CheckTiming::PUBLIC
    } else {
        CheckTiming::LOCAL
    };
    // Fail closed: without the gateway credentials the runtime key cannot be
    // verified (the preflight normally stops this before anything is deployed).
    let gateway = gateway_headers(&handle.exposure)?;
    eprintln!(
        "verifying endpoint authentication at {} ...",
        handle.endpoint
    );
    amicu_runtime::exposure::verify_endpoint(&EndpointCheck {
        endpoint: &handle.endpoint,
        path: PROBE_PATH,
        api_key: &key,
        gateway_headers: &gateway,
        timing,
    })
    .await
    .map_err(|e| e.detail().to_owned())
}

/// The provider-gateway headers to attach to the authenticated probe. `Err`
/// names the missing configuration (never a value) when the gateway needs
/// credentials this machine does not have.
fn gateway_headers(exposure: &EndpointExposure) -> Result<Vec<(String, SecretValue)>, String> {
    use amicu_modal::proxy_auth::{TOKEN_ID_ENV_VAR, TOKEN_SECRET_ENV_VAR, TokenConfig};
    match exposure {
        EndpointExposure::Loopback
        | EndpointExposure::Public {
            gateway: GatewayAuth::None,
        } => Ok(Vec::new()),
        EndpointExposure::Public {
            gateway: GatewayAuth::ModalProxyToken,
        } => match TokenConfig::from_env() {
            TokenConfig::Complete(token) => token
                .headers()
                .into_iter()
                .map(|(name, value)| {
                    value
                        .to_str()
                        .map(|v| (name.to_owned(), SecretValue::new(v.to_owned())))
                        .map_err(|_| {
                            format!("{TOKEN_ID_ENV_VAR}/{TOKEN_SECRET_ENV_VAR} are invalid")
                        })
                })
                .collect(),
            _ => Err(format!(
                "{TOKEN_ID_ENV_VAR} and {TOKEN_SECRET_ENV_VAR} are not both set to valid values"
            )),
        },
    }
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
    graph: &Graph,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    if output.is_json() {
        write_json(out, handles)?;
    } else {
        let io = |r: std::io::Result<()>| r.map_err(|e| CliError::io("write failed", e));
        io(writeln!(out, "\nApplied {} deployment(s):", handles.len()))?;
        for h in handles {
            io(writeln!(out, "  {} -> {}", h.deployment, h.endpoint))?;
            let deployment = find_deployment(graph, &h.deployment);
            let model = deployment
                .and_then(|d| graph.models.iter().find(|m| m.id == d.model))
                .map(|m| m.source.reference.as_str());
            let key = deployment.and_then(|d| d.api_key.as_ref());
            io(write!(out, "{}", usage_snippets(h, key, model)))?;
        }
    }
    Ok(0)
}

/// `s` as one single-quoted POSIX shell word (`'` becomes `'\''`).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `s` as a double-quoted string literal valid in both JSON and Python.
fn py_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Whether `name` can be written as a shell `$NAME` variable.
fn is_shell_ident(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Ready-to-copy `curl` and OpenAI-SDK snippets for one applied endpoint.
///
/// Credentials are referenced by environment variable name only: the
/// deployment's `api_key` secret name (the variable the local secret resolver
/// reads), and for Modal the proxy-token variables. A value is never printed,
/// and none is available here to print.
fn usage_snippets(
    handle: &RuntimeHandle,
    api_key: Option<&amicu_core::secret::SecretRef>,
    model: Option<&str>,
) -> String {
    use std::fmt::Write as _;
    let base = handle.endpoint.trim_end_matches('/');
    let model = model.unwrap_or("MODEL");
    // Values interpolated into the shell and Python snippets are escaped for
    // their context (single-quoted shell word, JSON/Python string literal).
    let sh_url = sh_quote(&format!("{base}/v1/chat/completions"));
    let sh_body = sh_quote(&format!(
        "{{\"model\": {}, \"messages\": [{{\"role\": \"user\", \"content\": \"Hello\"}}]}}",
        py_str(model)
    ));
    let py_base = py_str(&format!("{base}/v1"));
    let py_model = py_str(model);
    // The variable the key lives in on the caller's machine. A secret name that
    // is not a shell identifier gets a stand-in the caller sets from it.
    let (key_var, key_note) = match api_key {
        Some(r) if is_shell_ident(&r.name) => (r.name.clone(), None),
        Some(r) => (
            "AMICU_API_KEY".to_owned(),
            Some(format!(
                "set AMICU_API_KEY to the value of secret '{}'",
                r.name
            )),
        ),
        None => (String::new(), None),
    };
    let proxy_auth = matches!(
        handle.exposure,
        EndpointExposure::Public {
            gateway: GatewayAuth::ModalProxyToken
        }
    );

    let mut curl_headers = Vec::new();
    if api_key.is_some() {
        curl_headers.push(format!("-H \"Authorization: Bearer ${key_var}\""));
    }
    if proxy_auth {
        curl_headers.push("-H \"Modal-Key: $MODAL_PROXY_TOKEN_ID\"".to_owned());
        curl_headers.push("-H \"Modal-Secret: $MODAL_PROXY_TOKEN_SECRET\"".to_owned());
    }
    curl_headers.push("-H \"Content-Type: application/json\"".to_owned());

    let mut out = String::new();
    if let Some(note) = key_note {
        let _ = writeln!(out, "      note:   {note}");
    }
    let _ = writeln!(out, "      curl:");
    let _ = writeln!(out, "        curl {sh_url} \\");
    for h in &curl_headers {
        let _ = writeln!(out, "          {h} \\");
    }
    let _ = writeln!(out, "          -d {sh_body}");
    let _ = writeln!(out, "      python (openai):");
    let _ = writeln!(out, "        import os");
    let _ = writeln!(out, "        from openai import OpenAI");
    let api_key_expr = if api_key.is_some() {
        format!("os.environ[\"{key_var}\"]")
    } else {
        "\"unused\"".to_owned()
    };
    if proxy_auth {
        let _ = writeln!(out, "        client = OpenAI(");
        let _ = writeln!(out, "            base_url={py_base},");
        let _ = writeln!(out, "            api_key={api_key_expr},");
        let _ = writeln!(out, "            default_headers={{");
        let _ = writeln!(
            out,
            "                \"Modal-Key\": os.environ[\"MODAL_PROXY_TOKEN_ID\"],"
        );
        let _ = writeln!(
            out,
            "                \"Modal-Secret\": os.environ[\"MODAL_PROXY_TOKEN_SECRET\"],"
        );
        let _ = writeln!(out, "            }},");
        let _ = writeln!(out, "        )");
    } else {
        let _ = writeln!(
            out,
            "        client = OpenAI(base_url={py_base}, api_key={api_key_expr})"
        );
    }
    let _ = writeln!(
        out,
        "        client.chat.completions.create(model={py_model}, messages=[{{\"role\": \"user\", \"content\": \"Hello\"}}])"
    );
    out.push('\n');
    out
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
            Some(Dispatch::Modal)
        );
    }

    #[test]
    fn vllm_on_a_local_provider_selects_the_host_supervisor() {
        for provider in [
            Some("local"),
            Some("local_apple"),
            Some("local_cpu"),
            Some("on_prem"),
            None,
        ] {
            assert_eq!(
                dispatch_for(RuntimeKind::Vllm, provider),
                Some(Dispatch::Host),
                "{provider:?}"
            );
        }
    }

    #[test]
    fn vllm_on_an_unsupported_provider_is_not_dispatched() {
        for provider in ["runpod", "hetzner", "aws", "ssh"] {
            assert_eq!(
                dispatch_for(RuntimeKind::Vllm, Some(provider)),
                None,
                "{provider}"
            );
        }
    }

    #[test]
    fn unsupported_provider_error_names_it_and_the_supported_set() {
        let id = amicu_core::id::DeploymentId::from("dep");
        let err = super::require_dispatch(RuntimeKind::Vllm, Some("runpod"), &id)
            .expect_err("runpod is not applyable")
            .to_string();
        assert!(err.contains("provider \"runpod\" is not supported by apply yet"));
        assert!(err.contains("modal"));
    }

    #[test]
    fn llama_cpp_on_modal_selects_the_modal_supervisor() {
        assert_eq!(
            dispatch_for(RuntimeKind::LlamaCpp, Some("modal")),
            Some(Dispatch::Modal)
        );
    }

    #[test]
    fn llama_cpp_on_a_local_provider_selects_the_host_supervisor() {
        for provider in [
            Some("local"),
            Some("local_apple"),
            Some("local_cpu"),
            Some("on_prem"),
            None,
        ] {
            assert_eq!(
                dispatch_for(RuntimeKind::LlamaCpp, provider),
                Some(Dispatch::Host),
                "{provider:?}"
            );
        }
    }

    #[test]
    fn llama_cpp_on_an_unsupported_provider_is_not_dispatched() {
        for provider in ["runpod", "hetzner", "aws", "ssh"] {
            assert_eq!(
                dispatch_for(RuntimeKind::LlamaCpp, Some(provider)),
                None,
                "{provider}"
            );
        }
    }
}

#[cfg(test)]
mod merge_tests {
    use super::{CliError, Progress, merge_endpoints, record_partial};
    use crate::planner::build_plan;
    use crate::project::Project;
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
            exposure: amicu_runtime::EndpointExposure::Loopback,
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
        let merged = merge_endpoints(&prior, &graph, &[], std::slice::from_ref(&new_beta));
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

        let merged = merge_endpoints(&prior, &graph, &[], &[]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].deployment.as_str(), "alpha");
    }

    #[test]
    fn failed_second_deployment_leaves_the_first_in_state() {
        let dir = std::env::temp_dir().join(format!("amicu-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);

        // Fresh project: alpha started, then beta failed.
        let graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let progress = Progress {
            started: vec![handle(&graph, "alpha", "http://alpha")],
            stopped: Vec::new(),
            in_flight: Some(handle(&graph, "beta", "x").deployment),
        };
        let err = record_partial(
            &project,
            &graph,
            None,
            &progress,
            CliError::Message("beta failed to start".to_owned()),
        )
        .to_string();
        assert!(err.contains("beta failed to start"), "{err}");
        assert!(err.contains("started: alpha"), "{err}");
        assert!(err.contains("recorded in state"), "{err}");
        assert!(err.contains("'beta' failed mid-start"), "{err}");

        let state = project.load_state().unwrap().expect("state was saved");
        let ids: Vec<_> = state
            .graph
            .deployments
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(ids, vec!["alpha"], "beta never ran, so it is not recorded");
        assert_eq!(state.endpoints.len(), 1);
        assert_eq!(state.endpoints[0].deployment.as_str(), "alpha");

        // The next plan still wants beta, and leaves alpha alone.
        let next = build_plan(&graph, Some(&state.graph)).unwrap();
        let pending: Vec<_> = next.items.iter().map(|i| i.deployment.as_str()).collect();
        assert_eq!(pending, vec!["beta"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_model_keeps_its_prior_definition_for_unreached_deployments() {
        let dir = std::env::temp_dir().join(format!("amicu-partial-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);

        // Prior state: alpha and beta share model m. The config then changes m.
        let prior_graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let prior = crate::project::LocalState {
            version: crate::project::LocalState::VERSION.to_owned(),
            graph: prior_graph.clone(),
            endpoints: vec![
                handle(&prior_graph, "alpha", "http://old-alpha"),
                handle(&prior_graph, "beta", "http://old-beta"),
            ],
        };
        let graph = amicu_hcl::parse_graph(&CONFIG.replace("model.gguf", "model-q5.gguf")).unwrap();
        // alpha was restarted; beta was never reached.
        let progress = Progress {
            started: vec![handle(&graph, "alpha", "http://new-alpha")],
            stopped: Vec::new(),
            in_flight: None,
        };
        record_partial(
            &project,
            &graph,
            Some(&prior),
            &progress,
            CliError::Message("boom".to_owned()),
        );
        let state = project.load_state().unwrap().unwrap();
        let next = build_plan(&graph, Some(&state.graph)).unwrap();
        let mut pending: Vec<_> = next.items.iter().map(|i| i.deployment.as_str()).collect();
        pending.sort_unstable();
        assert_eq!(
            pending,
            vec!["alpha", "beta"],
            "beta must not look unchanged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_started_leaves_state_untouched() {
        let dir = std::env::temp_dir().join(format!("amicu-partial-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = Project::new(&dir);
        let graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let err = record_partial(
            &project,
            &graph,
            None,
            &Progress::default(),
            CliError::Message("boom".to_owned()),
        )
        .to_string();
        assert_eq!(err, "boom");
        assert!(project.load_state().unwrap().is_none());
    }
}

#[cfg(test)]
mod exposure_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{Dispatch, secure_endpoint, usage_snippets, verify_handle};
    use amicu_core::adapter::{
        Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus,
    };
    use amicu_core::diagnostic::Diagnostic;
    use amicu_core::resource::Deployment;
    use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
    use amicu_runtime::{
        EndpointExposure, GatewayAuth, LaunchPlan, LogOptions, LogStream, Parallelism, Platform,
        RenderedLaunch, ResourceEstimate, RuntimeAdapter, RuntimeCapabilities, RuntimeHandle,
        RuntimeKind, RuntimeMetrics, RuntimeStatus,
    };
    use async_trait::async_trait;

    const KEY: &str = "sk-apply-test-key-9876543210";

    struct FixedResolver;

    #[async_trait]
    impl SecretResolver for FixedResolver {
        async fn resolve(&self, _: &SecretRef) -> Result<SecretValue, AdapterError> {
            Ok(SecretValue::new(KEY.to_owned()))
        }
    }

    /// An adapter that only records whether `stop` ran (and can fail it).
    struct StopRecorder {
        stopped: Arc<AtomicBool>,
        fail: bool,
    }

    #[async_trait]
    impl Adapter for StopRecorder {
        fn id(&self) -> AdapterId {
            AdapterId::new("fake")
        }
        async fn health(&self) -> Result<HealthStatus, AdapterError> {
            Ok(HealthStatus::Healthy)
        }
    }

    impl DescribeCapabilities for StopRecorder {
        fn describe(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
    }

    #[async_trait]
    impl RuntimeAdapter for StopRecorder {
        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities {
                runtime: RuntimeKind::Vllm,
                artifact_formats: Vec::new(),
                platforms: vec![Platform::Cpu],
                parallelism: Parallelism {
                    tensor: false,
                    pipeline: false,
                    max_gpus: 1,
                },
                quantizations: Vec::new(),
                lora: false,
                tool_calling: false,
                multimodal: Vec::new(),
            }
        }
        fn validate(&self, _: &Deployment) -> Vec<Diagnostic> {
            Vec::new()
        }
        fn plan_launch(&self, _: &Deployment) -> Result<LaunchPlan, AdapterError> {
            unreachable!("not used")
        }
        async fn apply(
            &self,
            _: &LaunchPlan,
            _: &dyn SecretResolver,
        ) -> Result<RuntimeHandle, AdapterError> {
            unreachable!("not used")
        }
        async fn status(&self, _: &RuntimeHandle) -> Result<RuntimeStatus, AdapterError> {
            unreachable!("not used")
        }
        async fn metrics(&self, _: &RuntimeHandle) -> Result<RuntimeMetrics, AdapterError> {
            unreachable!("not used")
        }
        async fn logs(&self, _: &RuntimeHandle, _: LogOptions) -> Result<LogStream, AdapterError> {
            unreachable!("not used")
        }
        async fn stop(&self, _: &RuntimeHandle) -> Result<(), AdapterError> {
            self.stopped.store(true, Ordering::SeqCst);
            if self.fail {
                Err(AdapterError::permanent(
                    amicu_core::diagnostic::catalog::PROVIDER_BACKEND,
                    "stop rpc down",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn launch(api_key: Option<&str>) -> LaunchPlan {
        LaunchPlan {
            deployment: "dep".into(),
            runtime: RuntimeKind::Vllm,
            rendered: RenderedLaunch::Manifest {
                text: String::new(),
            },
            settings: amicu_core::resource::Runtime::Vllm(
                amicu_core::resource::VllmSettings::default(),
            ),
            secret_refs: Vec::new(),
            api_key: api_key.map(SecretRef::new),
            estimate: ResourceEstimate {
                estimated_memory: amicu_core::ByteSize::from_bytes(0),
                gpu_count: 0,
                kv_cache: None,
            },
        }
    }

    fn handle(endpoint: &str, exposure: EndpointExposure) -> RuntimeHandle {
        RuntimeHandle {
            id: "inst-1".to_owned(),
            deployment: "dep".into(),
            endpoint: endpoint.to_owned(),
            exposure,
        }
    }

    /// Loopback server: `open` answers 200 to everyone; otherwise it demands
    /// the bearer key.
    async fn serve(open: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                    let ok = open || req.contains(&format!("bearer {}", KEY.to_lowercase()));
                    let status = if ok { "200 OK" } else { "401 Unauthorized" };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                });
            }
        });
        url
    }

    #[tokio::test]
    async fn loopback_without_a_key_skips_the_check() {
        // No server at all: a skipped check must not touch the network.
        let h = handle("http://127.0.0.1:1", EndpointExposure::Loopback);
        verify_handle(&h, &launch(None), &FixedResolver)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_public_endpoint_without_a_key_fails_closed() {
        let h = handle(
            "http://127.0.0.1:1",
            EndpointExposure::Public {
                gateway: GatewayAuth::None,
            },
        );
        let err = verify_handle(&h, &launch(None), &FixedResolver)
            .await
            .unwrap_err();
        assert!(err.contains("no `api_key`"), "{err}");
    }

    #[tokio::test]
    async fn a_protected_loopback_endpoint_with_a_key_passes() {
        let url = serve(false).await;
        let h = handle(&url, EndpointExposure::Loopback);
        verify_handle(&h, &launch(Some("K")), &FixedResolver)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_open_endpoint_is_stopped_and_apply_fails() {
        let url = serve(true).await;
        let stopped = Arc::new(AtomicBool::new(false));
        let adapter = StopRecorder {
            stopped: Arc::clone(&stopped),
            fail: false,
        };
        let h = handle(&url, EndpointExposure::Loopback);
        let err = secure_endpoint(
            &adapter,
            &h,
            &launch(Some("K")),
            Dispatch::Host,
            &FixedResolver,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(stopped.load(Ordering::SeqCst), "adapter.stop must run");
        assert!(err.contains("unauthenticated request"), "{err}");
        assert!(err.contains("was stopped"), "{err}");
        assert!(!err.contains(KEY), "{err}");
    }

    #[tokio::test]
    async fn a_failed_teardown_is_reported_with_the_manual_command() {
        let url = serve(true).await;
        let adapter = StopRecorder {
            stopped: Arc::new(AtomicBool::new(false)),
            fail: true,
        };
        let h = handle(
            &url,
            EndpointExposure::Public {
                gateway: GatewayAuth::None,
            },
        );
        let err = secure_endpoint(
            &adapter,
            &h,
            &launch(Some("K")),
            Dispatch::Modal,
            &FixedResolver,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("teardown FAILED"), "{err}");
        assert!(err.contains("modal app stop inst-1"), "{err}");
        assert!(!err.contains(KEY), "{err}");
    }

    #[test]
    fn snippets_for_modal_reference_every_credential_by_env_name_only() {
        let h = handle(
            "https://x.modal.direct/",
            EndpointExposure::Public {
                gateway: GatewayAuth::ModalProxyToken,
            },
        );
        let text = usage_snippets(&h, Some(&SecretRef::new("CHAT_API_KEY")), Some("org/model"));
        assert!(
            text.contains("https://x.modal.direct/v1/chat/completions"),
            "{text}"
        );
        assert!(
            text.contains("Authorization: Bearer $CHAT_API_KEY"),
            "{text}"
        );
        assert!(text.contains("Modal-Key: $MODAL_PROXY_TOKEN_ID"), "{text}");
        assert!(
            text.contains("Modal-Secret: $MODAL_PROXY_TOKEN_SECRET"),
            "{text}"
        );
        assert!(text.contains("os.environ[\"CHAT_API_KEY\"]"), "{text}");
        assert!(
            text.contains("\"Modal-Key\": os.environ[\"MODAL_PROXY_TOKEN_ID\"]"),
            "{text}"
        );
        assert!(text.contains("org/model"), "{text}");
    }

    #[test]
    fn modal_without_proxy_tokens_fails_before_deploying() {
        use amicu_modal::proxy_auth::TokenConfig;
        let err = super::require_modal_proxy_token(&TokenConfig::None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("MODAL_PROXY_TOKEN_ID"), "{err}");
        assert!(err.contains("MODAL_PROXY_TOKEN_SECRET"), "{err}");
        assert!(err.contains("nothing was started"), "{err}");
        let ok = TokenConfig::from_values(Some("wk-1".into()), Some("ws-1".into()));
        super::require_modal_proxy_token(&ok).unwrap();
    }

    #[test]
    fn snippets_escape_hostile_model_and_url_values() {
        let h = handle("http://h/'; touch x; '", EndpointExposure::Loopback);
        let text = usage_snippets(&h, None, Some("m'\"; $(id)"));
        assert!(text.contains("'\\''"), "{text}");
        assert!(!text.contains("curl http://h/'; touch"), "{text}");
        assert!(text.contains("\\\""), "{text}");
    }

    #[test]
    fn snippets_for_a_keyless_loopback_endpoint_send_no_credentials() {
        let h = handle("http://127.0.0.1:8080", EndpointExposure::Loopback);
        let text = usage_snippets(&h, None, None);
        assert!(!text.contains("Authorization"), "{text}");
        assert!(!text.contains("Modal-"), "{text}");
        assert!(text.contains("api_key=\"unused\""), "{text}");
    }

    #[test]
    fn a_secret_name_that_is_not_a_shell_identifier_gets_a_stand_in() {
        let h = handle("http://127.0.0.1:8080", EndpointExposure::Loopback);
        let text = usage_snippets(&h, Some(&SecretRef::new("chat-api-key")), None);
        assert!(text.contains("Bearer $AMICU_API_KEY"), "{text}");
        assert!(text.contains("value of secret 'chat-api-key'"), "{text}");
    }
}
