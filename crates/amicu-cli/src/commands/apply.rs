//! `amicu apply [--auto-approve]` - plan, approve, then start deployments.

use std::io::Write;
use std::sync::Arc;

use amicu_core::adapter::AdapterError;
use amicu_core::diagnostic::{DiagnosticReport, Severity};
use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
#[cfg(test)]
use amicu_core::resource::Runtime;
use amicu_core::resource::{Deployment, is_local_provider};
use amicu_core::secret::{SecretResolver, SecretValue};
use amicu_llamacpp::{
    Catalog, LlamaCppAdapter, ProcessSupervisor, SidecarPlanner, host_supervisor_with_sidecar,
};
use amicu_modal::ModalSupervisor;
use amicu_modal::supervisor::project_app_name;
use amicu_runpod::{RunPodSupervisor, StopOutcome};
use amicu_runtime::exposure::{CheckTiming, EndpointCheck, PROBE_PATH};
use amicu_runtime::{
    EndpointExposure, EndpointRegistry, GatewayAuth, LaunchPlan, RuntimeAdapter, RuntimeHandle,
};
use amicu_vllm::VllmAdapter;

use crate::OutputFormat;
use crate::catalog::build_catalog;
use crate::checks::report_for;
use crate::commands::telemetry_agent::TelemetryHooks;
use crate::error::CliError;
use crate::lock::StateLock;
use crate::modal_ops::{
    ModalBackend, ModalOps, Workspace, check_workspace, unlocatable_record, unproven_workspace,
};
use crate::planner::{PlanError, PlanResult, build_project_plan};
use crate::project::{LocalState, Project};
use crate::runpod_ops::RunPodOps;
use crate::secret::EnvSecretResolver;
use crate::stops::{Backend, StopRecord};
use crate::write_json;
use amicu_engine::fingerprint::Fingerprint;

mod pod;

/// Recompute the plan, gate on approval, apply, and persist state.
pub(crate) fn run(
    project: &Project,
    auto_approve: bool,
    lock_timeout: Option<std::time::Duration>,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    run_with(
        project,
        auto_approve,
        lock_timeout,
        output,
        out,
        &mut std::io::stdin().lock(),
        &Env::real(false, false),
    )
}

/// [`run`] with the approval answer read from `input` and every external
/// system (host service manager, Modal) injected through `env`.
pub(crate) fn run_with(
    project: &Project,
    auto_approve: bool,
    lock_timeout: Option<std::time::Duration>,
    output: OutputFormat,
    out: &mut dyn Write,
    input: &mut dyn std::io::BufRead,
    env: &Env,
) -> Result<u8, CliError> {
    let quiet = Env {
        progress: |_| {},
        ..*env
    };
    let env = if output.is_json() { &quiet } else { env };
    run_for(project, auto_approve, lock_timeout, output, out, input, env)
}

fn run_for(
    project: &Project,
    auto_approve: bool,
    lock_timeout: Option<std::time::Duration>,
    output: OutputFormat,
    out: &mut dyn Write,
    input: &mut dyn std::io::BufRead,
    env: &Env,
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
    // save section, so a concurrent apply or `modal stop` cannot interleave.
    // Released when `_lock` drops on return.
    let _lock = StateLock::acquire(&project.state_dir(), "apply", lock_timeout)?;
    // Every apply, a no-op one included, enforces an opt-out on this host.
    stop_agents_if_opted_out(env)?;

    let (prior_state, pruned) =
        crate::refresh::load_refreshed(project, env.modal, env.runpod, output, out)?;
    // A pending deployment (start begun, never confirmed) is planned as absent
    // so apply (re)starts it; destroy still sees it in state.
    let prior_graph = prior_state.as_ref().map(|s| {
        let mut g = s.graph.clone();
        g.deployments.retain(|d| !s.pending.contains(&d.id));
        g
    });
    let recorded = prior_state
        .as_ref()
        .map(|s| s.fingerprints.clone())
        .unwrap_or_default();
    let result = match build_project_plan(project, &graph, prior_graph.as_ref(), &recorded) {
        Ok(result) => result,
        // See `commands::plan::run`'s matching arm: `build_plan` hands back
        // every collected diagnostic instead of a bare adapter error.
        Err(PlanError(report)) => {
            emit_report(&report, output, out)?;
            return Ok(1);
        }
    };

    let mut result = result;
    if let Some(state) = &prior_state {
        // A pending deployment that left the config still needs its recorded
        // instance stopped: plan it as a destroy.
        for item in pending_destroys(state, &graph)? {
            result.plan.actions.push(item.action.clone());
            result.items.push(item);
        }
    }

    // Resolve-check every secret before any side effect: no approval prompt, no
    // write-ahead state, no start.
    let unset = crate::secret::missing_secrets_diagnostics(&result, env.secret_is_set);
    result.diagnostics.extend(unset);

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
        // Unchanged deployments with no recorded fingerprint get a baseline
        // now, so a later secret/image/binary change is detected.
        if let Some(state) = &prior_state {
            let mut next = state.clone();
            adopt_baselines(&mut next.fingerprints, &graph, Some(state), &result);
            if pruned || next.fingerprints != state.fingerprints {
                project.save_state(&next)?;
            }
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
        let switches = prior_state
            .as_ref()
            .map(|s| {
                crate::commands::plan::provider_switches(
                    &result,
                    &graph,
                    &effective_stops(Some(s)),
                    s.project_id.as_deref(),
                )
            })
            .unwrap_or_default();
        crate::commands::plan::print_human_with(&result, &graph, &switches, preview)?;
        if !auto_approve {
            require_interactive(env, "apply")?;
        }
        if !auto_approve && !prompt_approval_from(preview, "Apply these changes?", input)? {
            writeln!(preview, "Apply cancelled.").map_err(|e| CliError::io("write failed", e))?;
            return Ok(0);
        }
    }

    let (handles, state) = execute(
        project,
        graph,
        &result,
        prior_state.as_ref(),
        env,
        Op::Apply,
    )?;
    let code = report(&handles, &state.graph, output, out)?;
    // A host deployment comes back after a reboot; tell the user when this
    // machine would only do so at their next login (systemd lingering off).
    if !output.is_json()
        && handles.iter().any(|h| h.id.starts_with("io.amicu."))
        && let Some(hint) = (env.host)(None).boot_hint()
    {
        writeln!(out, "\n{hint}").map_err(|e| CliError::io("write failed", e))?;
    }
    Ok(code)
}

/// When consent resolves to off from any source (the file, `DO_NOT_TRACK`,
/// `AMICU_TELEMETRY=0`), stop every telemetry agent on this host and delete
/// every queue. The agents run under the service manager and never see this
/// shell's environment, so the CLI enforces the opt-out for them. A stop
/// that fails is an error: an agent that may still send must not be ignored.
pub(crate) fn stop_agents_if_opted_out(env: &Env<'_>) -> Result<(), CliError> {
    if env.telemetry.consent_on() {
        return Ok(());
    }
    let host = (env.host)(None);
    let stopped = crate::commands::build_runtime()?
        .block_on(host.stop_all_sidecars())
        .map_err(|e| {
            CliError::Adapter(amicu_core::adapter::AdapterErrorKind::into_adapter_error(e))
        })?;
    env.telemetry.clear_queues();
    if !stopped.is_empty() {
        let _ = writeln!(
            std::io::stderr(),
            "Telemetry is off: stopped {} telemetry agent(s) and deleted their buffered data.",
            stopped.len()
        );
    }
    Ok(())
}

/// The recorded instances of `deployment` that are not the one about to be
/// started: they must be stopped (and confirmed) first.
fn stale_records(
    stops: &[StopRecord],
    deployment: &DeploymentId,
    new_stop: &StopRecord,
) -> Vec<StopRecord> {
    stops
        .iter()
        .filter(|r| &r.deployment == deployment && !r.same_target(new_stop))
        .cloned()
        .collect()
}

/// Builds the host process supervisor (launchd/systemd), optionally with the
/// telemetry sidecar planner. A parameter so tests never touch the real
/// service manager.
pub(crate) type HostSupervisorFactory =
    dyn Fn(Option<Arc<dyn SidecarPlanner>>) -> Box<dyn ProcessSupervisor>;

/// The external systems a run acts on, injected so tests use fakes.
pub(crate) struct Env<'a> {
    pub host: &'a HostSupervisorFactory,
    pub modal: &'a dyn ModalBackend,
    pub runpod: &'a dyn RunPodOps,
    /// Resolves the launch secrets (the process environment for real runs).
    pub resolver: &'a dyn SecretResolver,
    /// Destroy only: treat a Modal app that cannot be found by name (and has
    /// no recorded id) as gone, instead of refusing to forget it.
    pub forget_unconfirmed: bool,
    /// Destroy only: also delete the recorded host log files (Modal volumes
    /// are never deleted).
    pub purge: bool,
    /// The Modal proxy token config `apply` requires for Modal deployments;
    /// a parameter so tests need not touch the process environment.
    pub proxy_tokens: fn() -> amicu_modal::proxy_auth::TokenConfig,
    /// Whether stdin is a terminal a person can answer a prompt on.
    pub interactive: bool,
    /// Whether a secret name is set; a parameter so tests need not touch the
    /// process environment.
    pub secret_is_set: fn(&str) -> bool,
    /// Consent, the sidecar planner and the telemetry data dir; a parameter
    /// so tests never read the real consent file.
    pub telemetry: &'a dyn TelemetryHooks,
    /// Receives progress lines from long waits (stderr; silent for JSON output).
    pub progress: fn(&str),
}

fn progress_to_stderr(line: &str) {
    let _ = writeln!(std::io::stderr(), "  {line}");
}

impl Env<'static> {
    /// The real host service manager and Modal.
    pub(crate) fn real(forget_unconfirmed: bool, purge: bool) -> Self {
        Self {
            host: &host_supervisor_with_sidecar,
            modal: &crate::refresh::ModalLookup,
            runpod: &crate::runpod_ops::RealRunPod,
            resolver: &EnvSecretResolver,
            forget_unconfirmed,
            purge,
            proxy_tokens: amicu_modal::proxy_auth::TokenConfig::from_env,
            interactive: std::io::IsTerminal::is_terminal(&std::io::stdin()),
            secret_is_set: crate::secret::env_has_secret,
            telemetry: &crate::commands::telemetry_agent::HostTelemetry,
            progress: progress_to_stderr,
        }
    }
}

/// Which command is executing a plan, for recovery text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Apply,
    Destroy,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Destroy => "destroy",
        }
    }
}

/// Destroy items for pending deployments that are no longer in the desired
/// graph (so the normal diff, which plans pending as absent, cannot see them).
/// Planner failures are errors, never swallowed.
fn pending_destroys(
    state: &LocalState,
    desired: &Graph,
) -> Result<Vec<crate::planner::PlanItem>, CliError> {
    let mut orphans = state.graph.clone();
    orphans
        .deployments
        .retain(|d| state.pending.contains(&d.id) && find_deployment(desired, &d.id).is_none());
    if orphans.deployments.is_empty() {
        return Ok(Vec::new());
    }
    match amicu_engine::planner::build_plan_in(&Graph::new(), Some(&orphans), None) {
        Ok(result) => Ok(result.items),
        Err(PlanError(report)) => Err(CliError::Message(format!(
            "cannot plan the destroy of pending deployments: {}",
            report
                .diagnostics
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        ))),
    }
}

/// The stop record derived from the graph alone, for state written before
/// stop records existed (v1).
fn derive_stop(state: &LocalState, d: &Deployment) -> StopRecord {
    let hardware = state
        .graph
        .hardware_profiles
        .iter()
        .find(|h| h.id == d.hardware_profile);
    let provider = hardware.map(|h| h.provider.as_str());
    match dispatch_for(amicu_runtime::RuntimeKind::of(&d.runtime), provider)
        .unwrap_or(Dispatch::Host)
    {
        Dispatch::RunPod if hardware.is_some_and(pod::is_pod) => StopRecord::runpod_pod_in(
            state.project_id.as_deref().unwrap_or_default(),
            &d.id,
            find_handle(state, &d.id).map(|h| h.id.clone()),
            None,
            Vec::new(),
        ),
        Dispatch::Host => StopRecord::host(&d.id, None),
        Dispatch::Modal => {
            StopRecord::modal(&d.id, find_handle(state, &d.id).map(|h| h.id.clone()), None)
        }
        Dispatch::RunPod => StopRecord::runpod_in(
            state.project_id.as_deref().unwrap_or_default(),
            &d.id,
            find_handle(state, &d.id).map(|h| h.id.clone()),
            None,
            Vec::new(),
        ),
    }
}

/// Every stop record in force: the recorded ones, plus a derived one for each
/// deployment in the graph that has none (v1 state).
pub(crate) fn effective_stops(prior: Option<&LocalState>) -> Vec<StopRecord> {
    let Some(state) = prior else {
        return Vec::new();
    };
    let mut out = state.stops.clone();
    for d in &state.graph.deployments {
        if !out.iter().any(|r| r.deployment == d.id) {
            out.push(derive_stop(state, d));
        }
    }
    out
}

/// Run an approved plan and persist the resulting state. Shared by `apply` and
/// `destroy`: `graph` is the desired graph that becomes the new state graph.
/// Returns the handles started and the saved state.
pub(crate) fn execute(
    project: &Project,
    graph: Graph,
    result: &PlanResult,
    prior_state: Option<&LocalState>,
    env: &Env,
    op: Op,
) -> Result<(Vec<RuntimeHandle>, LocalState), CliError> {
    let rt = crate::commands::build_runtime()?;
    let mut progress = Progress::new(prior_state);
    if progress.project_id.is_empty() {
        progress.project_id = project.mint_id();
    }
    if let Err(err) = rt.block_on(apply_all(
        project,
        &graph,
        result,
        prior_state,
        &mut progress,
        env,
    )) {
        return Err(record_partial(
            project,
            &graph,
            prior_state,
            &progress,
            err,
            op,
        ));
    }
    let handles = progress.started.clone();

    let prior_endpoints = prior_state.map_or(&[][..], |s| &s.endpoints[..]);
    let destroyed: Vec<DeploymentId> = result
        .items
        .iter()
        .filter(|i| i.launch.is_none())
        .map(|i| i.deployment.clone())
        .collect();
    let endpoints = merge_endpoints(prior_endpoints, &graph, &destroyed, &handles);
    // Everything started is confirmed; pending entries survive only if still
    // in the graph and untouched by this run.
    let pending = pending_after(prior_state, &graph, &handles);
    let state = LocalState {
        version: LocalState::VERSION.to_owned(),
        project_id: Some(progress.project_id.clone()),
        pending,
        stops: progress.stops.clone(),
        purge_pending: purge_pending_after(prior_state, &progress.stops),
        fingerprints: final_fingerprints(prior_state, &graph, &progress, result),
        graph,
        endpoints,
        // Placeholder: `save_state` overwrites it with the serial it read plus one.
        serial: prior_state.map_or(0, |s| s.serial),
        extra: prior_state.map(|s| s.extra.clone()).unwrap_or_default(),
    };
    if let Err(save_err) = project.save_state(&state) {
        return Err(CliError::Message(save_failed_message(
            op, &progress, &save_err,
        )));
    }

    Ok((handles, state))
}

/// The purge-pending records after a run: the prior ones, plus every log file
/// of a prior stop record that the run no longer records (stopped, destroyed or
/// superseded by an instance with different paths), minus any file a live stop
/// record uses again. No recorded file ever leaves state without either
/// staying in a stop record or entering `purge_pending`.
fn purge_pending_after(
    prior: Option<&LocalState>,
    stops: &[StopRecord],
) -> Vec<crate::stops::PurgePending> {
    let live = |l: &String| stops.iter().any(|s| s.logs.contains(l));
    let mut out = prior.map(|s| s.purge_pending.clone()).unwrap_or_default();
    for rec in prior.map_or(&[][..], |s| &s.stops[..]) {
        let Some(mut entry) = rec.purge_entry() else {
            continue;
        };
        entry.logs.retain(|l| !live(l));
        if entry.logs.is_empty() {
            continue;
        }
        match out
            .iter_mut()
            .find(|p| p.deployment == entry.deployment && p.identity == entry.identity)
        {
            Some(existing) => {
                for l in entry.logs {
                    if !existing.logs.contains(&l) {
                        existing.logs.push(l);
                    }
                }
            }
            None => out.push(entry),
        }
    }
    for p in &mut out {
        p.logs.retain(|l| !live(l));
    }
    out.retain(|p| !p.logs.is_empty());
    out
}

/// Fingerprints for a fully successful run: as [`merged_fingerprints`], plus
/// a baseline (the current fingerprint) for every unchanged deployment that has
/// none recorded yet, so later changes to it are detected. Never overwrites a
/// recorded fingerprint of an unchanged deployment: that would hide a change.
fn final_fingerprints(
    prior: Option<&LocalState>,
    graph: &Graph,
    progress_or_none: &Progress,
    result: &PlanResult,
) -> std::collections::BTreeMap<DeploymentId, Fingerprint> {
    let mut out = merged_fingerprints(prior, graph, progress_or_none);
    adopt_baselines(&mut out, graph, prior, result);
    out
}

/// Add the current fingerprint of each deployment in `graph` that is unchanged
/// (not in the plan, still in the prior graph, not pending) and has no record.
fn adopt_baselines(
    out: &mut std::collections::BTreeMap<DeploymentId, Fingerprint>,
    graph: &Graph,
    prior: Option<&LocalState>,
    result: &PlanResult,
) {
    let Some(prior) = prior else { return };
    for (id, current) in &result.fingerprints {
        let planned = result.items.iter().any(|i| &i.deployment == id);
        if planned
            || prior.pending.contains(id)
            || find_deployment(graph, id).is_none()
            || find_deployment(&prior.graph, id).is_none()
        {
            continue;
        }
        out.entry(id.clone())
            .and_modify(|f| f.fill_missing(current))
            .or_insert_with(|| {
                // As `fill_missing`: a running process may not match the current
                // launch command, so its digest is never adopted.
                let mut baseline = current.clone();
                baseline.launch = None;
                baseline
            });
    }
}

/// Prior pending deployments that remain unresolved: still in `graph` and not
/// confirmed started by `started`.
fn pending_after(
    prior: Option<&LocalState>,
    graph: &Graph,
    started: &[RuntimeHandle],
) -> Vec<DeploymentId> {
    prior
        .map(|s| s.pending.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|id| {
            find_deployment(graph, id).is_some() && !started.iter().any(|h| &h.deployment == *id)
        })
        .cloned()
        .collect()
}

/// The state to persist BEFORE `next` is started: everything done so far, plus
/// `next` recorded in the graph and marked pending, so a crash between the
/// provider's start and the final save still leaves a stoppable record.
fn write_ahead_state(
    graph: &Graph,
    prior: Option<&LocalState>,
    progress: &Progress,
    next: &DeploymentId,
) -> LocalState {
    let mut ahead = progress.clone();
    ahead.in_flight = Some(next.clone());
    recorded_state(prior, graph, &ahead)
}

/// The state that records exactly what `progress` did to the real world: the
/// single place a mid-run state is assembled (write-ahead and failure paths).
/// Pure, so every position of failure is table tested without a provider.
fn recorded_state(prior: Option<&LocalState>, graph: &Graph, progress: &Progress) -> LocalState {
    let started: Vec<DeploymentId> = progress
        .started
        .iter()
        .map(|h| h.deployment.clone())
        .collect();
    let partial = partial_graph(
        prior.map(|s| &s.graph),
        graph,
        &started,
        progress.in_flight.as_ref(),
        &progress.stopped,
    );
    let prior_endpoints = prior.map_or(&[][..], |s| &s.endpoints[..]);
    let mut pending = pending_after(prior, &partial, &progress.started);
    if let Some(id) = &progress.in_flight
        && !pending.contains(id)
    {
        pending.push(id.clone());
    }
    LocalState {
        version: LocalState::VERSION.to_owned(),
        project_id: Some(progress.project_id.clone()),
        endpoints: merge_endpoints(
            prior_endpoints,
            &partial,
            &progress.stopped,
            &progress.started,
        ),
        pending,
        stops: progress.stops.clone(),
        purge_pending: purge_pending_after(prior, &progress.stops),
        fingerprints: merged_fingerprints(prior, &partial, progress),
        graph: partial,
        // Placeholder: `save_state` overwrites it with the serial it read plus one.
        serial: prior.map_or(0, |s| s.serial),
        extra: prior.map(|s| s.extra.clone()).unwrap_or_default(),
    }
}

/// Fingerprints to keep: the prior ones for deployments still in `graph`,
/// replaced by those of deployments confirmed started in this run. A deployment
/// that was stopped or removed loses its fingerprint; an in-flight one keeps
/// its OLD fingerprint (its new start is unconfirmed, so it still counts as
/// changed on the next plan).
fn merged_fingerprints(
    prior: Option<&LocalState>,
    graph: &Graph,
    progress: &Progress,
) -> std::collections::BTreeMap<DeploymentId, Fingerprint> {
    let mut out: std::collections::BTreeMap<_, _> = prior
        .map(|s| s.fingerprints.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|(id, _)| find_deployment(graph, id).is_some() && !progress.stopped.contains(id))
        .collect();
    for (id, fp) in &progress.fingerprints {
        if find_deployment(graph, id).is_some() {
            out.insert(id.clone(), fp.clone());
        }
    }
    out
}

/// The endpoints to record after an apply: the prior (refreshed) endpoints of
/// deployments that are still in `graph` and were not destroyed, plus this
/// run's `handles` (which replace any prior entry for the same deployment).
/// Pure so it is unit tested without a network.
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
#[derive(Default, Clone)]
struct Progress {
    /// Deployments started and verified, in plan order.
    started: Vec<RuntimeHandle>,
    /// Deployments stopped because they were removed from the config.
    stopped: Vec<DeploymentId>,
    /// The deployment being started when `apply_all` failed, if any. Its
    /// state entry is left as it was and may no longer match reality.
    in_flight: Option<DeploymentId>,
    /// How to stop everything that may be running, kept current as the run
    /// goes: a record is added before its start and removed only when its
    /// stop is confirmed.
    stops: Vec<StopRecord>,
    /// The project's stable id (minted on the first run), used to scope host
    /// service labels.
    project_id: String,
    /// The fingerprint each started deployment was started with.
    fingerprints: std::collections::BTreeMap<DeploymentId, Fingerprint>,
}

impl Progress {
    fn new(prior: Option<&LocalState>) -> Self {
        Self {
            stops: effective_stops(prior),
            // Empty until `execute` mints one for a project that has none.
            project_id: prior.and_then(|s| s.project_id.clone()).unwrap_or_default(),
            ..Self::default()
        }
    }
}

/// The graph to persist after a partial apply: the prior graph, plus each
/// `started` deployment and the `in_flight` one (with the model and hardware
/// profile it was started from), minus each `stopped` one. Deployments that
/// were not reached keep their prior definition, so the next `plan` still sees
/// them as pending. Pure: no provider, no state, no I/O.
fn partial_graph(
    prior: Option<&Graph>,
    graph: &Graph,
    started: &[DeploymentId],
    in_flight: Option<&DeploymentId>,
    stopped: &[DeploymentId],
) -> Graph {
    let mut out = prior.cloned().unwrap_or_else(Graph::new);
    // The in-flight deployment is recorded too (as pending): it may be running.
    let touched: Vec<DeploymentId> = started.iter().chain(in_flight).cloned().collect();
    for id in &touched {
        let Some(d) = find_deployment(graph, id) else {
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
                p.deployments
                    .iter()
                    .any(|x| uses(x) && !stopped.contains(&x.id) && !touched.contains(&x.id))
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
    out.deployments.retain(|d| !stopped.contains(&d.id));
    out
}

/// Persist what a failed apply did get done, and return `err` annotated with
/// which deployments were started and recorded. Without
/// this they would keep running (and billing) with no state entry, invisible
/// to `destroy` and re-created by the next `apply`.
fn record_partial(
    project: &Project,
    graph: &Graph,
    prior: Option<&LocalState>,
    progress: &Progress,
    err: CliError,
    op: Op,
) -> CliError {
    let verb = op.name();
    if progress.started.is_empty()
        && progress.stopped.is_empty()
        && progress.in_flight.is_none()
        && progress.stops == effective_stops(prior)
    {
        if op == Op::Destroy {
            return CliError::Message(format!(
                "{err}\n\ndestroy stopped before anything was removed; every deployment is \
                 still in state. Fix the error and re-run `amicu destroy` (add `--target <name>` \
                 to limit it)"
            ));
        }
        return err;
    }
    let state = recorded_state(prior, graph, progress);
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
    let finished = done.join("; ");
    if let Some(id) = &progress.in_flight {
        done.push(format!(
            "'{id}' failed mid-start and is recorded as pending: it may be running, and \
             `amicu destroy --target {id}` will stop it (or re-run `amicu apply` to retry)"
        ));
    }
    let done = if done.is_empty() {
        String::new()
    } else {
        format!(" ({})", done.join("; "))
    };
    let recorded = if finished.is_empty() {
        "recorded in state"
    } else {
        "already done and recorded in state"
    };
    let retry = match op {
        Op::Apply => "`amicu apply`",
        Op::Destroy => "`amicu destroy` (add `--target <name>` to limit it)",
    };
    match project.save_state(&state) {
        Ok(()) => CliError::Message(format!(
            "{err}\n\n{verb} stopped part-way; {recorded}{done}. \
             Fix the error and re-run {retry} to finish the rest"
        )),
        Err(save_err) => CliError::Message(format!(
            "{err}\n\n{verb} stopped part-way{done} and recording that in state FAILED: {}",
            save_failed_message(op, progress, &save_err)
        )),
    }
}

/// What to tell the user when the state save at the end of (or part-way through) a
/// run fails, e.g. on a compare-and-swap conflict. Only instances this run
/// started or changed can be untracked: deployments it left alone are not
/// listed, since stopping them would cause an outage. Destroy starts nothing,
/// so for it the state on disk still lists everything and a re-run confirms.
fn save_failed_message(op: Op, progress: &Progress, save_err: &CliError) -> String {
    if op == Op::Destroy {
        return format!(
            "{save_err}. Entries this run destroyed may still be in state, and \
             a re-run of `amicu destroy` confirms they are gone. Nothing needs stopping by hand"
        );
    }
    let touched = |d: &DeploymentId| {
        progress.in_flight.as_ref() == Some(d)
            || progress.started.iter().any(|h| &h.deployment == d)
    };
    let running: Vec<String> = progress
        .stops
        .iter()
        .filter(|r| touched(&r.deployment))
        .map(StopRecord::describe)
        .collect();
    if running.is_empty() {
        return format!(
            "{save_err}. Nothing was started by this run; re-run `amicu apply` from the current state"
        );
    }
    format!(
        "{save_err}. Started by this run and recorded in state as pending before each start: {}. \
         Re-running `amicu apply` (or `amicu destroy --target <name>`) handles them; only if \
         those entries are no longer in state, stop each by hand",
        running.join("; ")
    )
}

/// Hardware profile `provider` value that routes a deployment through
/// [`ModalSupervisor`] instead of the local host supervisor
const MODAL_PROVIDER: &str = "modal";

const RUNPOD_PROVIDER: &str = "runpod";

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
    RunPod,
}

/// Providers `apply` can deploy to today, for the error message.
const SUPPORTED_PROVIDERS: &str = "local, local_apple, local_cpu, on_prem, modal, runpod";

/// Computes [`Dispatch`] for one deployment's runtime family and hardware
/// profile provider. `None` when `apply` has no path for that provider.
fn dispatch_for(runtime: amicu_runtime::RuntimeKind, provider: Option<&str>) -> Option<Dispatch> {
    use amicu_runtime::RuntimeKind::{LlamaCpp, Vllm};
    match (runtime, provider) {
        (LlamaCpp | Vllm, Some(MODAL_PROVIDER)) => Some(Dispatch::Modal),
        (LlamaCpp | Vllm, Some(RUNPOD_PROVIDER)) => Some(Dispatch::RunPod),
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
    app_name: String,
    runtime: amicu_runtime::RuntimeKind,
    previous_endpoint: Option<String>,
) -> Result<ModalSupervisor, CliError> {
    let (deployment, hardware) = deployment_hardware(graph, catalog, id)?;
    let supervisor = ModalSupervisor::for_hardware(app_name, &hardware)
        .map_err(|e| CliError::Adapter(AdapterError::from_kind(e)))?;
    let supervisor = supervisor
        .with_scaling(deployment.scaling.as_ref())
        .with_previous_endpoint(previous_endpoint);
    Ok(match (runtime, catalog.resolve_model(&deployment.model)) {
        (amicu_runtime::RuntimeKind::LlamaCpp, Some(resolved)) => {
            supervisor.with_gguf_model(&resolved.model)
        }
        _ => supervisor,
    })
}

fn deployment_hardware<'g>(
    graph: &'g Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
) -> Result<(&'g Deployment, amicu_core::resource::HardwareProfile), CliError> {
    let deployment = find_deployment(graph, id).ok_or_else(|| {
        CliError::Message(format!(
            "deployment '{id}' missing from its own graph while dispatching it"
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
    Ok((deployment, hardware))
}

fn supervisor_err(e: amicu_llamacpp::LlamaCppError) -> CliError {
    CliError::Adapter(amicu_core::adapter::AdapterErrorKind::into_adapter_error(e))
}

/// Every jurisdiction the project's `data_class`es allow (graph-wide, as the
/// plan-time residency check); empty when none is declared.
fn residency_allowlist(graph: &Graph) -> Vec<amicu_core::resource::Residency> {
    let mut out: Vec<_> = Vec::new();
    for r in graph.data_classes.iter().flat_map(|d| d.residency.iter()) {
        if !out.contains(r) {
            out.push(r.clone());
        }
    }
    out
}

/// The network volume a RunPod deployment mounts, read before anything is
/// recorded or written (amicu never creates one).
pub(super) async fn runpod_volume(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
    env: &Env<'_>,
) -> Result<Option<amicu_runpod::NetworkVolume>, CliError> {
    let (_, hardware) = deployment_hardware(graph, catalog, id)?;
    let Some(volume) = hardware.runpod.and_then(|r| r.network_volume) else {
        return Ok(None);
    };
    env.runpod
        .client()?
        .get_network_volume(&volume)
        .await
        .map(Some)
        .map_err(|e| {
            crate::runpod_ops::runpod_error(
                &format!(
                    "deployment '{id}': could not read RunPod network volume `{volume}`; nothing \
                     was started"
                ),
                &e,
            )
        })
}

/// The [`RunPodSupervisor`] for one deployment, adopting the endpoint and
/// secrets recorded in `prior`. The caller has already checked that `prior`
/// lives in `account`, so a `404` on stop counts as confirmed.
#[allow(clippy::too_many_arguments)]
fn runpod_supervisor(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
    project_id: &str,
    account: &str,
    env: &Env<'_>,
    prior: Option<&StopRecord>,
    volume: Option<&amicu_runpod::NetworkVolume>,
) -> Result<RunPodSupervisor, CliError> {
    let (deployment, hardware) = deployment_hardware(graph, catalog, id)?;
    let flashboot = hardware
        .runpod
        .as_ref()
        .and_then(|r| r.flashboot)
        .unwrap_or(false);
    let mut supervisor =
        RunPodSupervisor::for_hardware(env.runpod.client()?, project_id, id.as_str(), &hardware)
            .and_then(|s| s.with_allowed_residencies(&residency_allowlist(graph)))
            .and_then(|s| match volume {
                Some(v) => s.with_network_volume(v),
                None => Ok(s),
            })
            .map_err(supervisor_err)?
            .with_flashboot(flashboot)
            .with_probe_transport(env.runpod.probe())
            .with_progress(amicu_runpod::supervisor::PROGRESS_EVERY, env.progress)
            .with_workspace(account)
            .with_scaling(deployment.scaling.as_ref())
            .with_confirmed_account(true);
    if let Some(prior) = prior {
        if let Some(endpoint) = &prior.app_id {
            supervisor = supervisor.with_recorded_endpoint(endpoint);
        }
        supervisor = supervisor.with_recorded_secrets(prior.secrets.clone());
    }
    if amicu_runtime::RuntimeKind::of(&deployment.runtime) == amicu_runtime::RuntimeKind::LlamaCpp
        && let Some(resolved) = catalog.resolve_model(&deployment.model)
    {
        supervisor = supervisor.with_gguf_model(&resolved.model);
    }
    Ok(supervisor)
}

/// Reject unsupported providers, and off-machine endpoints with no API key,
/// before anything is started or stopped. `amicu validate`/`plan` already
/// report the latter (`AMICU-POLICY-007`); this is the backstop for callers
/// that reach `apply` without them.
fn preflight(result: &PlanResult, graph: &Graph, env: &Env<'_>) -> Result<(), CliError> {
    let missing = crate::secret::missing_secrets(result, env.secret_is_set);
    if !missing.is_empty() {
        return Err(CliError::Message(format!(
            "[AMICU-POLICY-002] {}; nothing was started",
            crate::secret::missing_secrets_message(&missing)
        )));
    }
    for item in &result.items {
        if let Some(launch) = &item.launch {
            require_dispatch(
                launch.runtime,
                item.action.provider.as_deref(),
                &item.deployment,
            )?;
            if item.action.provider.as_deref() == Some(RUNPOD_PROVIDER) && launch.api_key.is_none()
            {
                let why = if pod::deployment_is_pod(graph, &item.deployment) {
                    "a random key the runtime checks (for example `openssl rand -hex 32`, not a \
                     RunPod API key): the pod proxy adds no authentication"
                } else {
                    "a RunPod API key with api.runpod.ai access (ideally restricted to this \
                     endpoint): RunPod's gateway checks it on every request and the runtime has \
                     no key of its own"
                };
                return Err(CliError::Message(format!(
                    "deployment '{}': RunPod deployments need an `api_key`, and it must be {why} \
                     (AMICU-POLICY-007); nothing was started",
                    item.deployment
                )));
            }
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
        require_modal_proxy_token(&(env.proxy_tokens)())?;
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
#[allow(clippy::too_many_lines)] // one linear start/stop dispatch loop
async fn apply_all(
    project: &Project,
    graph: &Graph,
    result: &PlanResult,
    prior: Option<&LocalState>,
    progress: &mut Progress,
    env: &Env<'_>,
) -> Result<(), CliError> {
    // Dispatch to the adapter matching each deployment's own runtime family -
    // previously `apply` only ever started/stopped
    // llama.cpp instances. The Modal work adds a second axis: a
    // vLLM deployment whose hardware profile has `provider = "modal"` is
    // routed through `ModalSupervisor` instead of the local launchd/systemd
    // `host_supervisor()` below - built fresh per such deployment (it needs
    // that deployment's own GPU/region), never reused across iterations.
    // A llama.cpp deployment on Modal takes the same route through a
    // `LlamaCppAdapter`.
    let catalog = build_catalog(graph);
    // The opt-in telemetry agent rides along with host instances only when
    // consent resolves to on right now; otherwise a previous agent is stopped.
    let sidecar = env.telemetry.planner(graph, &catalog, result);
    let llama_adapter =
        LlamaCppAdapter::new(Box::new(catalog.clone()), (env.host)(sidecar.clone()));
    let vllm_adapter = VllmAdapter::new(Box::new(catalog.clone()), (env.host)(sidecar));
    let host_probe = (env.host)(None);
    let host_ctx = host_probe.host_context();
    let resolver = env.resolver;
    let registry = EndpointRegistry::new();
    preflight(result, graph, env)?;
    let runpod_account = runpod_preflight(result, graph, env).await?;

    for item in &result.items {
        if let Some(launch) = &item.launch {
            let dispatch = require_dispatch(
                launch.runtime,
                item.action.provider.as_deref(),
                &item.deployment,
            )?;
            // Modal and RunPod build a per-deployment adapter; the host adapters
            // are shared.
            let remote_adapter: Box<dyn RuntimeAdapter>;
            // Set for Modal: the id of the app `start` deployed, readable even when
            // `start` fails after the deploy (see below).
            let mut deployed_app = None;
            let mut runpod = None;
            let adapter: &dyn RuntimeAdapter = match (launch.runtime, dispatch) {
                (runtime, Dispatch::Modal) => {
                    let previous_endpoint = prior
                        .and_then(|s| find_handle(s, &item.deployment))
                        .map(|h| h.endpoint.clone());
                    let supervisor = modal_supervisor(
                        graph,
                        &catalog,
                        &item.deployment,
                        project_app_name(&progress.project_id, item.deployment.as_str()),
                        runtime,
                        previous_endpoint,
                    )?;
                    deployed_app = Some(supervisor.deployed_app());
                    remote_adapter =
                        boxed_adapter(Box::new(catalog.clone()), runtime, Box::new(supervisor));
                    remote_adapter.as_ref()
                }
                (runtime, Dispatch::RunPod) => {
                    let account = runpod_account.as_deref().ok_or_else(|| {
                        CliError::Message("the RunPod preflight did not run".to_owned())
                    })?;
                    for r in progress
                        .stops
                        .iter()
                        .filter(|r| r.deployment == item.deployment && r.backend.is_runpod())
                    {
                        crate::runpod_ops::check_account(
                            item.deployment.as_str(),
                            r.workspace.as_deref(),
                            Some(account),
                        )?;
                    }
                    if pod::deployment_is_pod(graph, &item.deployment) {
                        let (supervisor, rp) = pod::prepare(
                            graph,
                            &catalog,
                            launch,
                            &item.deployment,
                            account,
                            env,
                            &*host_probe,
                            progress,
                        )
                        .await?;
                        runpod = Some(rp);
                        remote_adapter =
                            boxed_adapter(Box::new(catalog.clone()), runtime, Box::new(supervisor));
                        remote_adapter.as_ref()
                    } else {
                        let name = amicu_runpod::project_endpoint_name(
                            &progress.project_id,
                            item.deployment.as_str(),
                        );
                        let recorded = progress
                            .stops
                            .iter()
                            .find(|r| {
                                r.deployment == item.deployment
                                    && r.backend == Backend::Runpod
                                    && r.identity == name
                            })
                            .cloned();
                        let volume = runpod_volume(graph, &catalog, &item.deployment, env).await?;
                        let supervisor = runpod_supervisor(
                            graph,
                            &catalog,
                            &item.deployment,
                            &progress.project_id,
                            account,
                            env,
                            recorded.as_ref(),
                            volume.as_ref(),
                        )?;
                        let launch_secrets =
                            amicu_llamacpp::resolve_launch_secrets(launch, resolver).await?;
                        let planned = supervisor
                            .planned_secret_names(launch, &launch_secrets)
                            .map_err(supervisor_err)?;
                        runpod = Some(RunPodStart {
                            pod: false,
                            account: account.to_owned(),
                            recorded_id: recorded.as_ref().and_then(|r| r.app_id.clone()),
                            secrets: merged_names(
                                recorded.as_ref().map_or(&[][..], |r| &r.secrets[..]),
                                &planned,
                            ),
                            deployed: supervisor.deployed_endpoint(),
                            created_endpoint: supervisor.created_endpoint(),
                            created_secrets: supervisor.created_secrets(),
                        });
                        remote_adapter =
                            boxed_adapter(Box::new(catalog.clone()), runtime, Box::new(supervisor));
                        remote_adapter.as_ref()
                    }
                }
                (amicu_runtime::RuntimeKind::LlamaCpp, Dispatch::Host) => &llama_adapter,
                (amicu_runtime::RuntimeKind::Vllm, Dispatch::Host) => &vllm_adapter,
            };
            // The workspace a Modal app will be created in; a deployment whose
            // recorded instances live in another workspace is refused.
            let active_ws = if dispatch == Dispatch::Modal {
                Some(env.modal.workspace().await?)
            } else {
                None
            };
            let new_stop = match (dispatch, &runpod) {
                (Dispatch::RunPod, Some(rp)) => (if rp.pod {
                    StopRecord::runpod_pod_in
                } else {
                    StopRecord::runpod_in
                })(
                    &progress.project_id,
                    &item.deployment,
                    rp.recorded_id.clone(),
                    Some(&rp.account),
                    rp.secrets.clone(),
                ),
                (Dispatch::Modal, _) => StopRecord::modal_in(
                    &progress.project_id,
                    &item.deployment,
                    None,
                    active_ws.as_ref(),
                )
                .with_volumes(&modal_volume_names(launch.runtime)),
                _ => {
                    let label = amicu_llamacpp::project_host_label(
                        &progress.project_id,
                        item.deployment.as_str(),
                    );
                    StopRecord::host_in(
                        &progress.project_id,
                        &item.deployment,
                        host_ctx.as_ref(),
                        host_probe.service_paths(&label),
                    )
                }
            };
            // The instance is started under the label recorded in state, and
            // a relative model path is resolved against the project root now,
            // not baked into the plan.
            let mut launch_owned = launch.clone();
            if dispatch == Dispatch::Host {
                launch_owned.service_label = Some(new_stop.identity.clone());
                resolve_model_paths(&mut launch_owned, project.root());
            }
            let launch = &launch_owned;
            if let Some(ws) = &active_ws {
                for r in progress
                    .stops
                    .iter()
                    .filter(|r| r.deployment == item.deployment && r.backend.is_modal())
                {
                    check_workspace(
                        item.deployment.as_str(),
                        r.workspace.as_deref(),
                        r.environment.as_deref(),
                        ws,
                    )?;
                }
            } else if dispatch == Dispatch::Host {
                // Never start something whose owner cannot be recorded.
                if host_ctx.is_none() {
                    return Err(CliError::Message(format!(
                        "cannot determine which user and service manager amicu acts as on this \
                         machine, so '{}' was not started (it could not be recorded for a safe \
                         stop later)",
                        item.deployment
                    )));
                }
                // An in-place restart of a host unit started as someone else
                // would leave the old one behind: refuse.
                for r in progress
                    .stops
                    .iter()
                    .filter(|r| r.deployment == item.deployment && r.same_target(&new_stop))
                {
                    check_host_context(r, host_ctx.as_ref(), &*host_probe)?;
                }
            }
            // A provider (or identity) change: stop what is running under the
            // OLD identity first, and confirm it, before starting the new one.
            // The old record stays in state until that stop is confirmed.
            let stale = stale_records(&progress.stops, &item.deployment, &new_stop);
            for old in stale {
                stop_record(
                    &old,
                    env,
                    &*host_probe,
                    &progress.project_id,
                    &mut progress.stops,
                )
                .await?;
                progress.stops.retain(|r| !r.same_instance(&old));
            }
            // Write-ahead: record the deployment as pending, with its stop
            // identity, BEFORE the provider starts anything, so a crash can
            // never leave an instance that state cannot stop. A previous
            // instance's record (a different Modal app id) is kept alongside.
            // A RunPod record is updated in place to also list the secrets
            // about to be written.
            if dispatch == Dispatch::RunPod {
                if let Some(r) = progress.stops.iter_mut().find(|r| r.same_target(&new_stop)) {
                    r.workspace.clone_from(&new_stop.workspace);
                    r.secrets = merged_names(&r.secrets, &new_stop.secrets);
                } else {
                    progress.stops.push(new_stop.clone());
                }
            } else if !progress
                .stops
                .iter()
                .any(|r| r.same_target(&new_stop) && (!dispatch_is_modal(r) || r.app_id.is_none()))
            {
                progress.stops.push(new_stop.clone());
            }
            project.save_state(&write_ahead_state(graph, prior, progress, &item.deployment))?;
            progress.in_flight = Some(item.deployment.clone());
            // An update of a Modal app that is serving: remember its version
            // and endpoint, so a new version that fails the post-deploy check
            // is rolled back instead of taking the good one down with it.
            let previous = if dispatch == Dispatch::Modal {
                previous_modal(&progress.stops, prior, &item.deployment, env.modal).await
            } else {
                None
            };
            let handle = match adapter.apply(launch, resolver).await {
                Ok(handle) => handle,
                Err(e) => {
                    // The supervisor may have rolled back or stopped an app it had
                    // already deployed: its id is known even though no handle came
                    // back. Record it, so a later destroy stops by id without
                    // `--forget-unconfirmed`.
                    if let Some(app_id) = deployed_app_id(deployed_app.as_ref()) {
                        record_modal_start(
                            &mut progress.stops,
                            &new_stop.identity,
                            &item.deployment,
                            &app_id,
                        );
                    }
                    if let Some(rp) = &runpod {
                        let note =
                            runpod_failed_start(rp, adapter, &new_stop, &mut progress.stops, None)
                                .await;
                        return Err(CliError::Message(format!("{e}{note}")));
                    }
                    return Err(e.into());
                }
            };
            // Shared post-deploy check, for every adapter: an endpoint that is
            // not protected as declared is rolled back (Modal update) or torn
            // down, and apply fails. RunPod tears down only an endpoint this
            // run created, as after a failed start.
            let secured = if let Some(rp) = &runpod {
                // A pod's supervisor already proved anonymous requests are refused.
                let checked = if rp.pod {
                    Ok(())
                } else {
                    verify_runpod_handle(&handle, launch, resolver, env.runpod).await
                };
                match checked {
                    Ok(()) => Ok(()),
                    Err(detail) => {
                        let note = runpod_failed_start(
                            rp,
                            adapter,
                            &new_stop,
                            &mut progress.stops,
                            Some(env.runpod),
                        )
                        .await;
                        Err(CliError::Message(format!(
                            "deployment '{}': endpoint {} failed its exposure check: \
                             {detail}{note}",
                            item.deployment, handle.id
                        )))
                    }
                }
            } else {
                secure_endpoint(
                    adapter,
                    &handle,
                    launch,
                    dispatch,
                    previous.as_ref(),
                    resolver,
                    env.runpod,
                )
                .await
            };
            if let Err(e) = secured {
                if dispatch == Dispatch::Modal {
                    // The app id is known now: record it, so a later destroy
                    // stops by id without `--forget-unconfirmed`. Any older
                    // record with another id is kept as it is.
                    record_modal_start(
                        &mut progress.stops,
                        &new_stop.identity,
                        &item.deployment,
                        &handle.id,
                    );
                }
                return Err(e);
            }
            registry.register(&handle);
            let mut superseded = Vec::new();
            if dispatch == Dispatch::Modal {
                // Modal's handle id is the app id: stop by id from now on. An
                // older record with a different app id is never overwritten;
                // it stays until its stop is confirmed below.
                superseded = record_modal_start(
                    &mut progress.stops,
                    &new_stop.identity,
                    &item.deployment,
                    &handle.id,
                );
            }
            if let Some(rp) = &runpod {
                sync_runpod_record(&mut progress.stops, &new_stop, Some(&handle.id), rp, true);
            }
            progress.started.push(handle);
            if let Some(fp) = &item.fingerprint {
                progress
                    .fingerprints
                    .insert(item.deployment.clone(), fp.clone());
            }
            progress.in_flight = None;
            for old in superseded {
                stop_record(
                    &old,
                    env,
                    &*host_probe,
                    &progress.project_id,
                    &mut progress.stops,
                )
                .await?;
                progress.stops.retain(|r| !r.same_instance(&old));
            }
        } else {
            // Destroy: stop every recorded instance of this deployment. A
            // deployment leaves state only once each stop is confirmed (or the
            // provider confirms nothing exists): any error aborts, the
            // deployment and its unconfirmed records stay in state, and the
            // command exits non-zero.
            let prior_deployment = prior.and_then(|s| find_deployment(&s.graph, &item.deployment));
            let mut records: Vec<StopRecord> = progress
                .stops
                .iter()
                .filter(|r| r.deployment == item.deployment)
                .cloned()
                .collect();
            if records.is_empty() {
                // No record at all (should not happen): derive one.
                records.push(match (prior, prior_deployment) {
                    (Some(s), Some(d)) => derive_stop(s, d),
                    _ => StopRecord::host(&item.deployment, host_ctx.as_ref()),
                });
            }
            for record in records {
                stop_record(
                    &record,
                    env,
                    &*host_probe,
                    &progress.project_id,
                    &mut progress.stops,
                )
                .await?;
                progress.stops.retain(|r| !r.same_instance(&record));
            }
            registry.deregister(&item.deployment);
            progress.stopped.push(item.deployment.clone());
        }
    }

    for policy in &graph.routing_policies {
        registry.register_policy(policy.clone());
    }

    Ok(())
}

/// The Modal version that is serving before an update, with the endpoint
/// state recorded for it.
struct PreviousModal<'a> {
    modal: &'a dyn ModalOps,
    app_id: String,
    version: u32,
    endpoint: String,
}

/// The serving version of `deployment`'s recorded Modal app, when state holds
/// both its app id and its endpoint and Modal can tell its version. Best
/// effort: `None` makes a failed update stop the app instead of rolling back.
async fn previous_modal<'a>(
    stops: &[StopRecord],
    prior: Option<&LocalState>,
    deployment: &DeploymentId,
    modal: &'a dyn ModalOps,
) -> Option<PreviousModal<'a>> {
    let app_id = stops
        .iter()
        .rev()
        .find(|r| &r.deployment == deployment && r.backend.is_modal() && r.app_id.is_some())?
        .app_id
        .clone()?;
    let endpoint = prior?
        .endpoints
        .iter()
        .find(|h| &h.deployment == deployment && h.id == app_id)?
        .endpoint
        .clone();
    let version = modal.latest_app_version(&app_id).await.ok()??;
    Some(PreviousModal {
        modal,
        app_id,
        version,
        endpoint,
    })
}

/// The shared post-deploy exposure check, run on the
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
    previous: Option<&PreviousModal<'_>>,
    resolver: &dyn SecretResolver,
    runpod: &dyn RunPodOps,
) -> Result<(), CliError> {
    let verdict = verify_handle(handle, launch, resolver, runpod).await;
    let Err(detail) = verdict else {
        return Ok(());
    };
    let hint = match dispatch {
        Dispatch::Modal => format!("`modal app stop {}`", handle.id),
        Dispatch::Host => host_stop_hint(std::env::consts::OS, &handle.id),
        Dispatch::RunPod => format!("delete endpoint {} in the RunPod console", handle.id),
    };
    // A Modal update redeploys under the same app: roll the new version back
    // and re-verify the previous endpoint recorded in state. Anything short of
    // a verified rollback stops the whole app (fail closed).
    let previous = previous.filter(|p| dispatch == Dispatch::Modal && p.app_id == handle.id);
    let label = previous.map(|p| format!("version {}", p.version));
    let (message, _kept) = amicu_runtime::exposure::rollback_or_teardown(
        &detail,
        &handle.id,
        &hint,
        label.as_deref(),
        || async {
            let p = previous.expect("only called with a previous version");
            p.modal
                .rollback_app(&p.app_id, p.version)
                .await
                .map_err(|e| e.to_string())
        },
        || async {
            let p = previous.expect("only called with a previous version");
            let mut old = handle.clone();
            old.endpoint.clone_from(&p.endpoint);
            verify_handle(&old, launch, resolver, runpod).await
        },
        || async { adapter.stop(handle).await.map_err(|e| e.to_string()) },
    )
    .await;
    Err(CliError::Message(format!(
        "deployment '{}': {message}",
        handle.deployment
    )))
}

/// The post-deploy check of a RunPod endpoint or pod: an anonymous request
/// is refused and the deployment's key is accepted.
async fn verify_runpod_handle(
    handle: &RuntimeHandle,
    launch: &LaunchPlan,
    resolver: &dyn SecretResolver,
    runpod: &dyn RunPodOps,
) -> Result<(), String> {
    let Some(key_ref) = &launch.api_key else {
        return Err(
            "the endpoint is reachable off-machine but the deployment declares no `api_key`"
                .to_owned(),
        );
    };
    let key = resolver
        .resolve(key_ref)
        .await
        .map_err(|e| format!("could not resolve the endpoint API key to verify it ({e})"))?;
    let _ = writeln!(
        std::io::stderr(),
        "verifying endpoint authentication at {} ...",
        handle.endpoint
    );
    runpod.verify_exposure(&handle.endpoint, &key).await
}

/// `Ok` when `handle` is protected as declared (or needs no check); `Err`
/// carries a secret-free description of what is wrong.
async fn verify_handle(
    handle: &RuntimeHandle,
    launch: &LaunchPlan,
    resolver: &dyn SecretResolver,
    runpod: &dyn RunPodOps,
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
    if handle.exposure
        == (EndpointExposure::Public {
            gateway: GatewayAuth::RunPodBearer,
        })
    {
        let _ = writeln!(
            std::io::stderr(),
            "verifying endpoint authentication at {} ...",
            handle.endpoint
        );
        return report_verify(runpod.verify_exposure(&handle.endpoint, &key).await);
    }
    let timing = if handle.exposure.is_public() {
        CheckTiming::PUBLIC
    } else {
        CheckTiming::LOCAL
    };
    // Fail closed: without the gateway credentials the runtime key cannot be
    // verified (the preflight normally stops this before anything is deployed).
    let gateway = gateway_headers(&handle.exposure)?;
    let _ = writeln!(
        std::io::stderr(),
        "verifying endpoint authentication at {} ...",
        handle.endpoint
    );
    report_verify(
        amicu_runtime::exposure::verify_endpoint(&EndpointCheck {
            endpoint: &handle.endpoint,
            path: PROBE_PATH,
            api_key: &key,
            gateway_headers: &gateway,
            timing,
        })
        .await
        .map_err(|e| e.detail().to_owned()),
    )
}

/// Prints the outcome of the "verifying endpoint authentication" check.
fn report_verify(result: Result<(), String>) -> Result<(), String> {
    let line = match &result {
        Ok(()) => "endpoint authentication: ok".to_owned(),
        Err(e) => format!("endpoint authentication: failed ({e})"),
    };
    let _ = writeln!(std::io::stderr(), "{line}");
    result
}

/// The provider-gateway headers to attach to the authenticated probe. `Err`
/// names the missing configuration (never a value) when the gateway needs
/// credentials this machine does not have.
fn gateway_headers(exposure: &EndpointExposure) -> Result<Vec<(String, SecretValue)>, String> {
    use amicu_modal::proxy_auth::{TOKEN_ID_ENV_VAR, TOKEN_SECRET_ENV_VAR, TokenConfig};
    match exposure {
        // RunPod's gateway checks the `Authorization: Bearer` key (a RunPod
        // API key) itself: the shared check needs nothing beyond the key.
        EndpointExposure::Loopback
        | EndpointExposure::Public {
            gateway: GatewayAuth::None | GatewayAuth::RunPodBearer,
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

/// The stop handle for a host instance: launchd label / systemd unit name.
fn host_handle(id: &DeploymentId, label: &str) -> RuntimeHandle {
    RuntimeHandle {
        id: label.to_owned(),
        deployment: id.clone(),
        endpoint: String::new(),
        exposure: EndpointExposure::Loopback,
    }
}

fn dispatch_is_modal(r: &StopRecord) -> bool {
    r.backend.is_modal()
}

/// The app id a [`ModalSupervisor`] reported deploying, if any.
fn deployed_app_id(
    slot: Option<&std::sync::Arc<std::sync::Mutex<Option<String>>>>,
) -> Option<String> {
    slot?.lock().ok()?.clone()
}

/// Apply the result of a successful Modal start to the stop records: the
/// unconfirmed placeholder (no app id) takes the real id. Records that carry a
/// different app id are kept untouched and returned, so the caller can stop
/// (and confirm) them before forgetting them.
fn record_modal_start(
    stops: &mut Vec<StopRecord>,
    identity: &str,
    deployment: &DeploymentId,
    app_id: &str,
) -> Vec<StopRecord> {
    let same = |r: &StopRecord| {
        &r.deployment == deployment && r.backend.is_modal() && r.identity == identity
    };
    let known = stops
        .iter()
        .any(|r| same(r) && r.app_id.as_deref() == Some(app_id));
    if let Some(i) = stops.iter().position(|r| same(r) && r.app_id.is_none()) {
        if known {
            stops.remove(i);
        } else {
            stops[i].app_id = Some(app_id.to_owned());
        }
    }
    stops
        .iter()
        .filter(|r| same(r) && r.app_id.as_deref().is_some_and(|a| a != app_id))
        .cloned()
        .collect()
}

/// The adapter for `runtime` over a per-deployment remote supervisor.
fn boxed_adapter(
    catalog: Box<dyn Catalog>,
    runtime: amicu_runtime::RuntimeKind,
    supervisor: Box<dyn ProcessSupervisor>,
) -> Box<dyn RuntimeAdapter> {
    match runtime {
        amicu_runtime::RuntimeKind::Vllm => Box::new(VllmAdapter::new(catalog, supervisor)),
        amicu_runtime::RuntimeKind::LlamaCpp => Box::new(LlamaCppAdapter::new(catalog, supervisor)),
    }
}

/// What a RunPod start began from, and the supervisor's view of what exists.
struct RunPodStart {
    /// An always-on pod rather than a serverless endpoint.
    pod: bool,
    account: String,
    recorded_id: Option<String>,
    secrets: Vec<String>,
    deployed: Arc<std::sync::Mutex<Option<String>>>,
    created_endpoint: Arc<std::sync::atomic::AtomicBool>,
    created_secrets: Arc<std::sync::Mutex<Vec<String>>>,
}

fn merged_names(a: &[String], b: &[String]) -> Vec<String> {
    let mut out: Vec<String> = a.iter().chain(b).cloned().collect();
    out.sort();
    out.dedup();
    out
}

/// Checks the RunPod management key and reads the account, once per run,
/// when any launch goes to RunPod. Nothing is started before this passes.
async fn runpod_preflight(
    result: &PlanResult,
    graph: &Graph,
    env: &Env<'_>,
) -> Result<Option<String>, CliError> {
    let launches: Vec<(&DeploymentId, &LaunchPlan)> = result
        .items
        .iter()
        .filter_map(|i| {
            let l = i.launch.as_ref()?;
            (dispatch_for(l.runtime, i.action.provider.as_deref()) == Some(Dispatch::RunPod))
                .then_some((&i.deployment, l))
        })
        .collect();
    if launches.is_empty() {
        return Ok(None);
    }
    for (d, launch) in &launches {
        let Some(key_ref) = &launch.api_key else {
            continue;
        };
        let key = env.resolver.resolve(key_ref).await?;
        if env.runpod.is_management_key(&key) {
            let fix = if pod::deployment_is_pod(graph, d) {
                "use a separate random key for the runtime (for example `openssl rand -hex 32`)"
            } else {
                "create a separate RunPod API key restricted to invoking this endpoint \
                 (api.runpod.ai access) and use it as the api_key"
            };
            return Err(CliError::Message(format!(
                "deployment '{d}': its api_key is the same key as {var}, the management key \
                 amicu deploys with. Clients would hold full control of the RunPod account; \
                 {fix}. Nothing was started",
                var = crate::runpod_ops::API_KEY_ENV_VAR
            )));
        }
    }
    let account = crate::runpod_ops::preflight(env.runpod).await?;
    for (d, launch) in launches {
        if pod::deployment_is_pod(graph, d) {
            let _ = writeln!(
                std::io::stderr(),
                "note: '{d}' runs as a RunPod pod: its api_key is the runtime's own key, checked \
                 by the runtime; the pod proxy adds no authentication"
            );
            if amicu_runtime::RuntimeKind::of(&launch.settings) == amicu_runtime::RuntimeKind::Vllm
            {
                let _ = writeln!(
                    std::io::stderr(),
                    "note: vLLM's --api-key leaves /invocations, /tokenize, /detokenize, \
                     /metrics and /docs open on the public pod URL (a vLLM issue, \
                     https://github.com/vllm-project/vllm/issues/58144); serverless mode \
                     puts RunPod's gateway key in front of every route"
                );
            }
            continue;
        }
        let _ = writeln!(
            std::io::stderr(),
            "note: '{d}' runs on RunPod: its api_key must be a RunPod API key with \
             api.runpod.ai access; RunPod's gateway checks it and the runtime has no key of its own"
        );
    }
    Ok(Some(account))
}

/// Updates the RunPod record matching `record` from the supervisor: the
/// endpoint id once known, and the secrets. `exact` (a finished start or a
/// confirmed teardown) records the supervisor's set as is; otherwise names
/// are only ever added, so nothing that may exist is forgotten.
fn sync_runpod_record(
    stops: &mut [StopRecord],
    record: &StopRecord,
    endpoint_id: Option<&str>,
    rp: &RunPodStart,
    exact: bool,
) {
    let Some(r) = stops.iter_mut().find(|r| r.same_target(record)) else {
        return;
    };
    if let Some(id) = endpoint_id {
        r.app_id = Some(id.to_owned());
    }
    if let Ok(created) = rp.created_secrets.lock() {
        r.secrets = if exact {
            merged_names(&created, &[])
        } else {
            merged_names(&r.secrets, &created)
        };
    }
}

fn runpod_handle(deployment: &DeploymentId, id: &str) -> RuntimeHandle {
    RuntimeHandle {
        id: id.to_owned(),
        deployment: deployment.clone(),
        endpoint: amicu_runpod::endpoint_url(id),
        exposure: EndpointExposure::Public {
            gateway: GatewayAuth::RunPodBearer,
        },
    }
}

/// After a failed RunPod start or exposure check: record what exists, and
/// tear down an endpoint this run created (nothing was serving from it). An
/// endpoint adopted by name or recorded id is left in place, scaled to zero
/// workers when `scale_down` is given (a failed exposure check). Either way
/// the record stays until a stop is confirmed. Returns a note for the error.
async fn runpod_failed_start(
    rp: &RunPodStart,
    adapter: &dyn RuntimeAdapter,
    record: &StopRecord,
    stops: &mut [StopRecord],
    scale_down: Option<&dyn RunPodOps>,
) -> String {
    if rp.pod {
        return pod::failed_start(rp, adapter, record, stops).await;
    }
    let deployed = rp.deployed.lock().ok().and_then(|d| d.clone());
    sync_runpod_record(stops, record, deployed.as_deref(), rp, false);
    let Some(id) = deployed else {
        return String::new();
    };
    if !rp
        .created_endpoint
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        let Some(ops) = scale_down else {
            return format!(
                "; RunPod endpoint {id} existed before this run, so it was left in place and \
                 stays recorded (`amicu destroy --target {}` removes it)",
                record.deployment
            );
        };
        let drain = amicu_runpod::UpdateEndpoint {
            workers: Some(amicu_runpod::WorkersConfig {
                min: 0,
                max: 0,
                idle_timeout: None,
            }),
            ..amicu_runpod::UpdateEndpoint::default()
        };
        let scaled = match ops.client() {
            Ok(client) => client
                .update_endpoint(&id, &drain)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        return match scaled {
            Ok(()) => format!(
                "; RunPod endpoint {id} existed before this run, so it was scaled to zero \
                 workers (not deleted) and stays recorded; fix the api_key and apply again, or \
                 `amicu destroy --target {}` removes it",
                record.deployment
            ),
            Err(e) => format!(
                "; RunPod endpoint {id} existed before this run and scaling it to zero workers \
                 FAILED ({e}); it may still be serving. Scale it down in the RunPod console or \
                 run `amicu destroy --target {}`",
                record.deployment
            ),
        };
    }
    match adapter.stop(&runpod_handle(&record.deployment, &id)).await {
        Ok(()) => {
            sync_runpod_record(stops, record, None, rp, true);
            format!(
                "; RunPod endpoint {id}, created by this run, was torn down (it stays recorded \
                 until a later apply or destroy confirms it is gone)"
            )
        }
        Err(e) => format!(
            "; tearing down RunPod endpoint {id}, created by this run, FAILED ({e}); it stays \
             recorded and `amicu destroy --target {}` retries",
            record.deployment
        ),
    }
}

/// Stops a RunPod endpoint and deletes its secrets. Only a confirmed stop in
/// the recorded account is `Ok`; anything else keeps the record. A record
/// without an endpoint id is resolved by exact name and owner marker.
async fn stop_runpod(
    record: &StopRecord,
    env: &Env<'_>,
    project_id: &str,
    stops: &mut [StopRecord],
) -> Result<(), CliError> {
    let client = env.runpod.client()?;
    let Some(active) = crate::runpod_ops::current_account(&client).await? else {
        return unknown_runpod_account(record, env);
    };
    crate::runpod_ops::check_account(
        record.deployment.as_str(),
        record.workspace.as_deref(),
        Some(&active),
    )?;
    let id = match &record.app_id {
        Some(id) => id.clone(),
        None => match owned_endpoint_by_name(&client, record, &active, project_id, env).await? {
            Some(id) => id,
            None => return delete_runpod_secrets(&client, record, env, stops).await,
        },
    };
    let supervisor = RunPodSupervisor::new(client, "", record.deployment.as_str())
        .with_recorded_secrets(record.secrets.clone())
        .with_confirmed_account(true);
    let result = supervisor
        .stop_endpoint(&runpod_handle(&record.deployment, &id))
        .await;
    let kept = match result {
        Ok(StopOutcome::Deleted) => return Ok(()),
        Ok(StopOutcome::AlreadyGone { kept_secrets }) if kept_secrets.is_empty() => {
            return Ok(());
        }
        Ok(StopOutcome::AlreadyGone { kept_secrets }) => format!(
            "{} is gone, but its secrets were kept ({})",
            record.describe(),
            kept_secrets.join(", ")
        ),
        Err(e) => format!("{}: {e}", record.describe()),
    };
    let remaining = supervisor.created_secrets().lock().map_or_else(
        |_| record.secrets.clone(),
        |c| merged_names(&record.secrets, &c),
    );
    if let Some(r) = stops.iter_mut().find(|r| r.same_instance(record)) {
        r.app_id = Some(id);
        r.secrets = remaining;
    }
    Err(CliError::Message(format!("{kept}; it was kept in state")))
}

/// The id of the one endpoint named `record.identity` that carries this
/// deployment's owner marker in `account`, or `None` when no endpoint has the
/// name. A foreign or duplicate name is refused, unless
/// `--forget-unconfirmed` (the foreign endpoint is then left untouched).
async fn owned_endpoint_by_name(
    client: &amicu_runpod::RunPodClient,
    record: &StopRecord,
    account: &str,
    project_id: &str,
    env: &Env<'_>,
) -> Result<Option<String>, CliError> {
    let lookup = |e: &amicu_runpod::RunPodError| {
        crate::runpod_ops::runpod_error(
            &format!(
                "{} has no endpoint id recorded and looking it up by name failed; it was kept \
                 in state",
                record.describe()
            ),
            e,
        )
    };
    let found = client
        .find_endpoints_by_name(&record.identity)
        .await
        .map_err(|e| lookup(&e))?;
    let one = match found.as_slice() {
        [] => return Ok(None),
        [one] => one,
        many => {
            return Err(CliError::Message(format!(
                "{} has no endpoint id recorded and several RunPod endpoints are named '{}' \
                 ({}); delete the extra ones in the RunPod console. It was kept in state",
                record.describe(),
                record.identity,
                many.iter()
                    .map(|e| e.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };
    let ep = client.get_endpoint(&one.id).await.map_err(|e| lookup(&e))?;
    let owner = amicu_runpod::owner_marker(Some(account), project_id, record.deployment.as_str());
    if ep.owner.as_deref() == Some(owner.as_str()) {
        return Ok(Some(ep.id));
    }
    if env.forget_unconfirmed {
        let _ = writeln!(
            std::io::stderr(),
            "warning: RunPod endpoint {} is named '{}' but does not carry this deployment's {} \
             marker; it was NOT deleted, and '{}' is forgotten as you asked \
             (--forget-unconfirmed)",
            ep.id,
            ep.name,
            amicu_runpod::OWNER_ENV_VAR,
            record.deployment
        );
        return Ok(None);
    }
    Err(CliError::Message(format!(
        "{} has no endpoint id recorded, and RunPod endpoint {} named '{}' does not carry this \
         deployment's {} marker, so amicu will not delete it. Check it in the RunPod console; \
         if this deployment never created an endpoint, run `amicu destroy --target {} \
         --forget-unconfirmed`. It was kept in state",
        record.describe(),
        ep.id,
        ep.name,
        amicu_runpod::OWNER_ENV_VAR,
        record.deployment
    )))
}

/// No endpoint exists for `record`: deletes its recorded secrets. `Ok` only
/// when every delete succeeded (or `--forget-unconfirmed`); the record then
/// keeps listing the secrets that could not be deleted.
async fn delete_runpod_secrets(
    client: &amicu_runpod::RunPodClient,
    record: &StopRecord,
    env: &Env<'_>,
    stops: &mut [StopRecord],
) -> Result<(), CliError> {
    let mut remaining = Vec::new();
    let mut failed = Vec::new();
    for name in &record.secrets {
        let deleted = match client.find_secret(name).await {
            Ok(Some(s)) => client.delete_secret(&s.id).await.map(|_| ()),
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        };
        if let Err(e) = deleted {
            failed.push(format!("`{name}` ({e})"));
            remaining.push(name.clone());
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    if env.forget_unconfirmed {
        let _ = writeln!(
            std::io::stderr(),
            "warning: forgetting '{}' as you asked (--forget-unconfirmed), but these RunPod \
             secrets could not be deleted and may remain: {}",
            record.deployment,
            failed.join(", ")
        );
        return Ok(());
    }
    if let Some(r) = stops.iter_mut().find(|r| r.same_instance(record)) {
        r.secrets = remaining;
    }
    Err(CliError::Message(format!(
        "{}: no {} exists, but these secrets could not be deleted: {}; it was kept in state",
        record.describe(),
        record.runpod_kind(),
        failed.join(", ")
    )))
}

/// `RUNPOD_API_KEY` acts in an account amicu cannot identify (the personal
/// account marker secret is missing): nothing can be confirmed.
fn unknown_runpod_account(record: &StopRecord, env: &Env<'_>) -> Result<(), CliError> {
    let leftovers = format!(
        "{} '{}'{}{}",
        record.runpod_kind(),
        record.identity,
        record
            .app_id
            .as_deref()
            .map(|id| format!(" (id {id})"))
            .unwrap_or_default(),
        if record.secrets.is_empty() {
            String::new()
        } else {
            format!(" and secrets {}", record.secrets.join(", "))
        }
    );
    if env.forget_unconfirmed {
        let _ = writeln!(
            std::io::stderr(),
            "warning: forgetting '{}' WITHOUT confirming anything (--forget-unconfirmed): amicu \
             cannot identify the RunPod account, so these may still exist in {}: {leftovers}",
            record.deployment,
            record.workspace.as_deref().unwrap_or("an unknown account")
        );
        return Ok(());
    }
    Err(CliError::Message(format!(
        "cannot confirm which RunPod account {key} acts in: the `{marker}` secret amicu uses to \
         identify a personal account is missing (deleted, or {key} belongs to another account), \
         so '{dep}' (recorded in {recorded}) cannot be stopped safely. If {key} is the right \
         account, delete {leftovers} in the RunPod console, then run `amicu destroy --target \
         {dep} --forget-unconfirmed`. It was kept in state",
        key = crate::runpod_ops::API_KEY_ENV_VAR,
        marker = amicu_runpod::ACCOUNT_MARKER_SECRET,
        dep = record.deployment,
        recorded = record.workspace.as_deref().unwrap_or("an unknown account"),
    )))
}

/// Refuse a host stop that would act as someone else: launchd and systemd are
/// never interchangeable, and the uid (the SID on Windows) / launchd domain
/// must match the ones the instance was started with. An unknown current identity is a refusal. A
/// record without a uid (older state) is accepted only for a non-root user who
/// owns the instance's service definition. When the service definition (the
/// launchd plist) is missing there is nothing to check, so that owner check is
/// skipped.
fn check_host_context(
    record: &StopRecord,
    current: Option<&amicu_llamacpp::HostContext>,
    probe: &dyn ProcessSupervisor,
) -> Result<(), CliError> {
    let Some(cur) = current else {
        return Err(CliError::Message(format!(
            "cannot determine which user and service manager amicu acts as on this machine, so \
             it will not act on {}. Nothing was changed",
            record.describe()
        )));
    };
    let expected = Backend::from_host(cur.backend);
    if record.backend != expected {
        return Err(CliError::Message(format!(
            "{} was started under {} but this machine uses {}; stop it on the machine and as \
             the user that started it. Nothing was changed",
            record.describe(),
            record.backend.name(),
            expected.name()
        )));
    }
    // Windows has no uid: the user is told by SID.
    if let (Some(rec), Some(now)) = (record.sid.as_deref(), cur.sid.as_deref())
        && rec != now
    {
        return Err(CliError::Message(format!(
            "{} was started as the Windows user {rec}, but you are {now}; re-run as that user. \
             Nothing was changed",
            record.describe()
        )));
    }
    match (record.uid, cur.uid) {
        (Some(rec), Some(now)) if rec != now => {
            return Err(CliError::Message(format!(
                "{} was started as uid {rec}, but you are uid {now}; re-run as that user. Nothing \
                 was changed",
                record.describe(),
            )));
        }
        (None, Some(now)) => {
            if now == 0 {
                return Err(CliError::Message(format!(
                    "{} has no recorded owner and you are root, so amicu cannot tell whose job \
                     it is; re-run as the user that started it. Nothing was changed",
                    record.describe()
                )));
            }
            if let Some(owner) = probe.instance_owner(&record.identity)
                && owner != now
            {
                return Err(CliError::Message(format!(
                    "{} belongs to uid {owner}, but you are uid {now}; re-run as that user. \
                     Nothing was changed",
                    record.describe(),
                )));
            }
        }
        _ => {}
    }
    if let (Some(rec), Some(now)) = (record.domain.as_deref(), cur.domain.as_deref())
        && rec != now
    {
        return Err(CliError::Message(format!(
            "{} was started in launchd domain {rec}, but the current domain is {now}. Re-run \
             from the same login session. If that session has ended (you logged out), its job \
             ended with it: confirm with `launchctl print {rec}/{}` (exit status 113 means it is \
             gone), log back in as that user, and run `amicu destroy --target {}` again. \
             Nothing was changed",
            record.describe(),
            record.identity,
            record.deployment
        )));
    }
    Ok(())
}

/// How to stop a host service by hand, for the host OS (`std::env::consts::OS`).
fn host_stop_hint(os: &str, id: &str) -> String {
    match os {
        "macos" => format!("`launchctl bootout gui/$(id -u)/{id}`"),
        "windows" => format!(
            "`schtasks /End /TN {task}` then `schtasks /Delete /TN {task} /F` (in PowerShell, or use Task Scheduler)",
            task = amicu_llamacpp::service_windows::ps_quote(
                &amicu_llamacpp::service_windows::task_name(id)
            )
        ),
        _ => format!("`systemctl --user disable --now {id}`"),
    }
}

/// Stop the instance `record` describes and confirm it. `Ok` means it is
/// stopped or the provider confirms it does not exist; anything else is an
/// error and the caller must keep the record in state.
async fn stop_record(
    record: &StopRecord,
    env: &Env<'_>,
    probe: &dyn ProcessSupervisor,
    project_id: &str,
    stops: &mut [StopRecord],
) -> Result<(), CliError> {
    match record.backend {
        Backend::HostLaunchd | Backend::HostSystemd | Backend::HostTaskScheduler => {
            check_host_context(record, probe.host_context().as_ref(), probe)?;
            let handle = host_handle(&record.deployment, &record.identity);
            // Validated BEFORE anything is stopped: a recorded path that is
            // not exactly this record's own service file is refused, so state
            // can never point the delete at an unrelated file.
            let definition = record.definition_path().map_err(|why| {
                CliError::Message(format!(
                    "{} has a recorded service file that amicu will not delete: {why}. Check \
                     the state file; the record was kept and nothing was changed",
                    record.describe()
                ))
            })?;
            let env_file = record.env_path().map_err(|why| {
                CliError::Message(format!(
                    "{} has a recorded secrets file that amicu will not delete: {why}. Check \
                     the state file; the record was kept and nothing was changed",
                    record.describe()
                ))
            })?;
            // Stop, then delete the service definition (a launchd plist holds
            // the endpoint API key; a systemd unit's key is in its env file,
            // deleted first) at the recorded paths; a failure to delete is an
            // error and the record stays in state.
            probe
                .stop_recorded_with_env(&handle, definition.as_deref(), env_file.as_deref())
                .await
                .map_err(|e| {
                    CliError::Adapter(amicu_core::adapter::AdapterErrorKind::into_adapter_error(e))
                })?;
            if let Some(path) = definition
                .iter()
                .chain(env_file.iter())
                .find(|p| p.exists())
            {
                return Err(CliError::Message(format!(
                    "{} is stopped but its service file {} (a plist, or the systemd secrets \
                     file, holds the endpoint API key) could not be removed. Delete it by hand; \
                     the record was kept so a re-run retries",
                    record.describe(),
                    path.display()
                )));
            }
            // Its agent was stopped with it: drop the agent's id and queue.
            env.telemetry.forget(&record.identity);
            Ok(())
        }
        Backend::Modal => stop_modal(record, env, stops).await,
        Backend::Runpod => stop_runpod(record, env, project_id, stops).await,
        Backend::RunpodPod => pod::stop(record, env, project_id, stops).await,
    }
}

/// The Volumes a Modal deployment of `runtime` mounts.
fn modal_volume_names(runtime: amicu_runtime::RuntimeKind) -> Vec<&'static str> {
    match runtime {
        amicu_runtime::RuntimeKind::Vllm => amicu_modal::supervisor::VOLUMES
            .iter()
            .map(|v| v.0)
            .collect(),
        amicu_runtime::RuntimeKind::LlamaCpp => amicu_modal::supervisor::LLAMACPP_VOLUMES
            .iter()
            .map(|v| v.0)
            .collect(),
    }
}

/// Flags whose value is a file path relative to the project root.
const PATH_FLAGS: [&str; 5] = [
    "-m",
    "--model",
    "--model-draft",
    "--mmproj",
    "--chat-template-file",
];

/// Make the file paths in a host launch absolute under `root`: the model
/// (`-m`), draft model, `--mmproj`/`-mm` and `--chat-template-file` values
/// (also in `--flag=value` form) that are relative. Done when the instance
/// starts, so plans and state never hold an absolute project path.
fn resolve_model_paths(launch: &mut LaunchPlan, root: &std::path::Path) {
    let amicu_runtime::RenderedLaunch::Command { argv, .. } = &mut launch.rendered else {
        return;
    };
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let absolute = |v: &str| root.join(v).to_string_lossy().into_owned();
    let is_path_flag = |f: &str| PATH_FLAGS.contains(&f) || f == "-mm";
    let mut i = 0;
    while i < argv.len() {
        if let Some((flag, value)) = argv[i].split_once('=')
            && is_path_flag(flag)
            && !std::path::Path::new(value).is_absolute()
        {
            argv[i] = format!("{flag}={}", absolute(value));
        } else if is_path_flag(&argv[i])
            && i + 1 < argv.len()
            && !std::path::Path::new(&argv[i + 1]).is_absolute()
        {
            argv[i + 1] = absolute(&argv[i + 1]);
        }
        i += 1;
    }
}

/// Record that `record`'s app was proven to live in `active`.
fn stamp_workspace(stops: &mut [StopRecord], record: &StopRecord, active: &Workspace) {
    for r in stops.iter_mut().filter(|r| r.same_instance(record)) {
        r.workspace = Some(active.name.clone());
        r.environment = Some(active.environment.clone());
    }
}

/// Stop a Modal app. A "not found" counts as confirmed only when the record's
/// workspace is KNOWN and matches the active one. A record without a
/// workspace must first be proven to belong to it (its app id appears in the
/// active workspace's app list), and is then stamped with it. With only a
/// name, a lookup that finds nothing cannot confirm (the app may exist
/// unpublished): an error, unless the user passed `--forget-unconfirmed` for
/// this explicit target AND the record's workspace is known and matches.
async fn stop_modal(
    record: &StopRecord,
    env: &Env<'_>,
    stops: &mut [StopRecord],
) -> Result<(), CliError> {
    let active = env.modal.workspace().await?;
    if record.workspace.is_some() {
        check_workspace(
            record.deployment.as_str(),
            record.workspace.as_deref(),
            record.environment.as_deref(),
            &active,
        )?;
    } else if let Some(id) = &record.app_id {
        if !env.modal.list_app_ids().await?.contains(id) {
            return Err(unproven_workspace(record.deployment.as_str(), id, &active));
        }
        stamp_workspace(stops, record, &active);
    }
    let app_id = match &record.app_id {
        Some(id) => id.clone(),
        None => match env.modal.find_app(&record.identity).await? {
            Some(id) => {
                // Found in the active workspace by name: it lives here.
                stamp_workspace(stops, record, &active);
                id
            }
            // No workspace and nothing found: nothing proves where (or whether)
            // the app lives, so neither stopping nor forgetting can act on it.
            None if record.workspace.is_none() => {
                return Err(unlocatable_record(
                    record.deployment.as_str(),
                    &record.identity,
                ));
            }
            None if env.forget_unconfirmed => {
                let _ = writeln!(
                    std::io::stderr(),
                    "warning: no Modal app '{}' found in {active}; forgetting '{}' as you asked \
                     (--forget-unconfirmed)",
                    record.identity,
                    record.deployment
                );
                return Ok(());
            }
            None => {
                return Err(CliError::Message(format!(
                    "cannot confirm Modal app '{name}' is stopped: no app id was recorded and \
                     {active} has no published app by that name (it may exist unpublished). \
                     Check `modal app list` or the Modal dashboard; once you are sure it is \
                     gone, run `amicu destroy --target {dep} --forget-unconfirmed`. It was kept \
                     in state.",
                    name = record.identity,
                    dep = record.deployment
                )));
            }
        },
    };
    // `false`: Modal reports no such app. The workspace is known and matches
    // by now, so that is a confirmed stop.
    env.modal.stop_app(&app_id).await?;
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
                .map(|m| {
                    // Must agree with the `--alias` llama-server was started with.
                    let llama = deployment.is_some_and(|d| {
                        amicu_runtime::RuntimeKind::of(&d.runtime)
                            == amicu_runtime::RuntimeKind::LlamaCpp
                    });
                    if llama {
                        m.served_name()
                    } else {
                        m.source.reference.as_str()
                    }
                });
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

/// Refuse to prompt when stdin is not a terminal: a script or CI job would
/// hang or silently cancel. Fails with an error naming `--auto-approve`.
pub(crate) fn require_interactive(env: &Env<'_>, command: &str) -> Result<(), CliError> {
    if env.interactive {
        return Ok(());
    }
    Err(CliError::Message(format!(
        "amicu {command} needs confirmation but stdin is not a terminal; \
         re-run with --auto-approve to proceed without a prompt"
    )))
}

/// Ask `question` and read the answer from `input` (EOF means no).
pub(crate) fn prompt_approval_from(
    out: &mut dyn Write,
    question: &str,
    input: &mut dyn std::io::BufRead,
) -> Result<bool, CliError> {
    write!(out, "\n{question} [y/N] ").map_err(|e| CliError::io("write failed", e))?;
    out.flush().map_err(|e| CliError::io("write failed", e))?;
    let mut line = String::new();
    input
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

    // `dispatch_for` is the whole of `apply_all`'s
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
        for provider in ["hetzner", "aws", "ssh"] {
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
        let err = super::require_dispatch(RuntimeKind::Vllm, Some("hetzner"), &id)
            .expect_err("hetzner is not applyable")
            .to_string();
        assert!(err.contains("provider \"hetzner\" is not supported by apply yet"));
        assert!(err.contains("modal, runpod"));
    }

    #[test]
    fn both_runtimes_on_runpod_select_the_runpod_supervisor() {
        for runtime in [RuntimeKind::Vllm, RuntimeKind::LlamaCpp] {
            assert_eq!(
                dispatch_for(runtime, Some("runpod")),
                Some(Dispatch::RunPod)
            );
        }
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
        for provider in ["hetzner", "aws", "ssh"] {
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

    pub(super) const CONFIG: &str = r#"resource "model" "m" {
  source {
    registry  = "hugging_face"
    reference = "unsloth/Qwen3.5-9B-GGUF"
  }
  architecture   = "qwen35"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "5680522464B"
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
            project_id: String::new(),
            started: vec![handle(&graph, "alpha", "http://alpha")],
            stopped: Vec::new(),
            in_flight: Some(handle(&graph, "beta", "x").deployment),
            stops: Vec::new(),
            fingerprints: std::collections::BTreeMap::default(),
        };
        let err = record_partial(
            &project,
            &graph,
            None,
            &progress,
            CliError::Message("beta failed to start".to_owned()),
            super::Op::Apply,
        )
        .to_string();
        assert!(err.contains("beta failed to start"), "{err}");
        assert!(err.contains("started: alpha"), "{err}");
        assert!(err.contains("recorded in state"), "{err}");
        assert!(err.contains("'beta' failed mid-start"), "{err}");
        assert!(!err.contains("(;"), "{err}");

        let state = project.load_state().unwrap().expect("state was saved");
        let ids: Vec<_> = state
            .graph
            .deployments
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["alpha", "beta"],
            "beta may be running, so it is recorded (as pending)"
        );
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].as_str(), "beta");
        assert_eq!(state.endpoints.len(), 1);
        assert_eq!(state.endpoints[0].deployment.as_str(), "alpha");

        // The next apply plan re-applies beta (pending is planned as absent)
        // and leaves alpha alone.
        let mut planning = state.graph.clone();
        planning
            .deployments
            .retain(|d| !state.pending.contains(&d.id));
        let next = build_plan(&graph, Some(&planning)).unwrap();
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
            project_id: None,
            pending: Vec::new(),
            stops: Vec::new(),
            fingerprints: std::collections::BTreeMap::default(),
            serial: 0,
            purge_pending: Vec::new(),
            extra: std::collections::BTreeMap::default(),
            graph: prior_graph.clone(),
            endpoints: vec![
                handle(&prior_graph, "alpha", "http://old-alpha"),
                handle(&prior_graph, "beta", "http://old-beta"),
            ],
        };
        let graph = amicu_hcl::parse_graph(&CONFIG.replace("model.gguf", "model-q5.gguf")).unwrap();
        // alpha was restarted; beta was never reached.
        let progress = Progress {
            project_id: String::new(),
            started: vec![handle(&graph, "alpha", "http://new-alpha")],
            stopped: Vec::new(),
            in_flight: None,
            stops: Vec::new(),
            fingerprints: std::collections::BTreeMap::default(),
        };
        record_partial(
            &project,
            &graph,
            Some(&prior),
            &progress,
            CliError::Message("boom".to_owned()),
            super::Op::Apply,
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
    fn only_an_in_flight_start_has_no_empty_done_list() {
        let dir = std::env::temp_dir().join(format!("amicu-partial-fl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let project = Project::new(&dir);
        let graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let progress = Progress {
            in_flight: Some(handle(&graph, "beta", "x").deployment),
            ..Progress::default()
        };
        let err = record_partial(
            &project,
            &graph,
            None,
            &progress,
            CliError::Message("boom".to_owned()),
            super::Op::Apply,
        )
        .to_string();
        assert!(!err.contains("(;"), "{err}");
        assert!(!err.contains("already done"), "{err}");
        assert!(
            err.contains("recorded in state ('beta' failed mid-start"),
            "{err}"
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
            super::Op::Apply,
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

    use super::{Dispatch, PreviousModal, secure_endpoint, usage_snippets, verify_handle};
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
            service_label: None,
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
        verify_handle(
            &h,
            &launch(None),
            &FixedResolver,
            &crate::runpod_ops::RealRunPod,
        )
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
        let err = verify_handle(
            &h,
            &launch(None),
            &FixedResolver,
            &crate::runpod_ops::RealRunPod,
        )
        .await
        .unwrap_err();
        assert!(err.contains("no `api_key`"), "{err}");
    }

    #[tokio::test]
    async fn a_protected_loopback_endpoint_with_a_key_passes() {
        let url = serve(false).await;
        let h = handle(&url, EndpointExposure::Loopback);
        verify_handle(
            &h,
            &launch(Some("K")),
            &FixedResolver,
            &crate::runpod_ops::RealRunPod,
        )
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
            None,
            &FixedResolver,
            &crate::runpod_ops::RealRunPod,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(stopped.load(Ordering::SeqCst), "adapter.stop must run");
        assert!(err.contains("unauthenticated request"), "{err}");
        assert!(err.contains("was stopped"), "{err}");
        assert!(!err.contains(KEY), "{err}");
    }

    /// A Modal that only records rollbacks (and can fail them).
    struct RollbackModal {
        rolled_back: std::sync::Mutex<Vec<(String, u32)>>,
        fail: bool,
    }

    #[async_trait]
    impl crate::modal_ops::ModalOps for RollbackModal {
        async fn workspace(&self) -> Result<crate::modal_ops::Workspace, crate::error::CliError> {
            unreachable!("not used")
        }
        async fn list_app_ids(&self) -> Result<Vec<String>, crate::error::CliError> {
            unreachable!("not used")
        }
        async fn find_app(&self, _: &str) -> Result<Option<String>, crate::error::CliError> {
            unreachable!("not used")
        }
        async fn latest_app_version(&self, _: &str) -> Result<Option<u32>, crate::error::CliError> {
            unreachable!("not used")
        }
        async fn rollback_app(
            &self,
            app_id: &str,
            version: u32,
        ) -> Result<(), crate::error::CliError> {
            self.rolled_back
                .lock()
                .unwrap()
                .push((app_id.to_owned(), version));
            if self.fail {
                Err(crate::error::CliError::Message("rpc down".to_owned()))
            } else {
                Ok(())
            }
        }
        async fn stop_app(&self, _: &str) -> Result<bool, crate::error::CliError> {
            unreachable!("apply stops through the adapter")
        }
    }

    const PUBLIC: EndpointExposure = EndpointExposure::Public {
        gateway: GatewayAuth::None,
    };

    /// A failed update on Modal: the new version's endpoint is `new_url`; the
    /// previous version (app `inst-1`, version 3) was serving at `old_url`.
    async fn modal_update(
        new_url: &str,
        old_url: &str,
        rollback_fails: bool,
        previous_app: &str,
    ) -> (String, bool, Vec<(String, u32)>) {
        let stopped = Arc::new(AtomicBool::new(false));
        let adapter = StopRecorder {
            stopped: Arc::clone(&stopped),
            fail: false,
        };
        let modal = RollbackModal {
            rolled_back: std::sync::Mutex::new(Vec::new()),
            fail: rollback_fails,
        };
        let previous = PreviousModal {
            modal: &modal,
            app_id: previous_app.to_owned(),
            version: 3,
            endpoint: old_url.to_owned(),
        };
        let err = secure_endpoint(
            &adapter,
            &handle(new_url, PUBLIC),
            &launch(Some("K")),
            Dispatch::Modal,
            Some(&previous),
            &FixedResolver,
            &crate::runpod_ops::RealRunPod,
        )
        .await
        .unwrap_err()
        .to_string();
        let calls = modal.rolled_back.lock().unwrap().clone();
        (err, stopped.load(Ordering::SeqCst), calls)
    }

    #[tokio::test]
    async fn a_modal_update_failing_the_key_check_is_rolled_back_not_stopped() {
        let bad = serve(true).await;
        let good = serve(false).await;
        let (err, stopped, calls) = modal_update(&bad, &good, false, "inst-1").await;
        assert!(!stopped, "the previous version must keep serving: {err}");
        assert_eq!(calls, [("inst-1".to_owned(), 3)]);
        assert!(err.contains("rolled back to version 3"), "{err}");
        assert!(err.contains("still serving"), "{err}");
        assert!(!err.contains(KEY), "{err}");
    }

    #[tokio::test]
    async fn a_modal_rollback_that_fails_or_leaves_the_previous_endpoint_open_stops_the_app() {
        let bad = serve(true).await;
        let good = serve(false).await;
        // The rollback RPC fails.
        let (err, stopped, _) = modal_update(&bad, &good, true, "inst-1").await;
        assert!(stopped, "{err}");
        assert!(err.contains("rolling back to version 3 failed"), "{err}");
        // The rollback "works" but the previous endpoint answers anyone.
        let (err, stopped, calls) = modal_update(&bad, &bad, false, "inst-1").await;
        assert!(stopped, "an open endpoint is never left serving: {err}");
        assert_eq!(calls.len(), 1);
        assert!(err.contains("still failed its check"), "{err}");
    }

    #[tokio::test]
    async fn a_previous_version_of_another_app_is_never_rolled_back_to() {
        let bad = serve(true).await;
        let good = serve(false).await;
        let (err, stopped, calls) = modal_update(&bad, &good, false, "inst-OTHER").await;
        assert!(stopped, "{err}");
        assert_eq!(calls.len(), 0);
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
            None,
            &FixedResolver,
            &crate::runpod_ops::RealRunPod,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("teardown FAILED"), "{err}");
        assert!(err.contains("modal app stop inst-1"), "{err}");
        assert!(!err.contains(KEY), "{err}");
    }

    #[test]
    fn a_runpod_endpoint_is_checked_with_the_bearer_key_alone() {
        let headers = super::gateway_headers(&EndpointExposure::Public {
            gateway: GatewayAuth::RunPodBearer,
        })
        .unwrap();
        assert!(headers.is_empty());
    }

    #[test]
    fn a_failed_update_records_the_app_id_on_the_placeholder_stop_record() {
        let dep: amicu_core::id::DeploymentId = "dep".into();
        let mut stops = vec![crate::stops::StopRecord::modal(&dep, None, None)];
        let identity = stops[0].identity.clone();
        super::record_modal_start(&mut stops, &identity, &dep, "inst-1");
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].app_id.as_deref(), Some("inst-1"));
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

#[cfg(test)]
mod stop_invariant_tests {
    //! Write-ahead state and stop-identity invariants: nothing may be running
    //! that state cannot stop, and an identity is never overwritten.

    use super::merge_tests::CONFIG;
    use super::{
        Progress, effective_stops, pending_after, pending_destroys, stale_records,
        write_ahead_state,
    };
    use crate::project::LocalState;
    use crate::stops::{Backend, StopRecord};
    use amicu_core::id::DeploymentId;

    fn id(name: &str) -> DeploymentId {
        DeploymentId::new(name)
    }

    fn state(graph: amicu_core::graph::Graph) -> LocalState {
        LocalState {
            version: LocalState::VERSION.to_owned(),
            project_id: None,
            graph,
            endpoints: Vec::new(),
            pending: Vec::new(),
            stops: Vec::new(),
            fingerprints: std::collections::BTreeMap::default(),
            serial: 0,
            purge_pending: Vec::new(),
            extra: std::collections::BTreeMap::default(),
        }
    }

    /// A name-scheme change must never lose a live app: the record under the old
    /// name is stale for the new target, so apply stops it (and confirms) first.
    #[test]
    fn a_legacy_modal_record_is_stale_for_the_project_scoped_target() {
        let dep = id("alpha");
        let legacy = StopRecord::modal(&dep, Some("ap-OLD".to_owned()), None);
        let target = StopRecord::modal_in("aaaa1111", &dep, None, None);
        assert_ne!(legacy.identity, target.identity);
        let stale = stale_records(std::slice::from_ref(&legacy), &dep, &target);
        assert_eq!(stale, vec![legacy.clone()]);
        // A record under the new name is the target itself, not stale.
        let none: Vec<StopRecord> = Vec::new();
        assert_eq!(
            stale_records(std::slice::from_ref(&target), &dep, &target),
            none
        );
        // Another project's app for the same deployment id is a different target.
        let other = StopRecord::modal_in("bbbb2222", &dep, Some("ap-2".to_owned()), None);
        assert!(!other.same_target(&target));
    }

    /// A supervisor that rolled the app back or stopped it still leaves its id
    /// behind, and it lands on the placeholder record.
    #[test]
    fn the_app_id_of_a_supervisor_side_rollback_fills_the_placeholder() {
        let dep = id("alpha");
        let supervisor = amicu_modal::ModalSupervisor::new("amicu-x");
        let slot = supervisor.deployed_app();
        assert_eq!(super::deployed_app_id(Some(&slot)), None);
        *slot.lock().unwrap() = Some("ap-9".to_owned());
        let app_id = super::deployed_app_id(Some(&slot)).unwrap();
        let mut stops = vec![StopRecord::modal_in("aaaa1111", &dep, None, None)];
        let identity = stops[0].identity.clone();
        let superseded = super::record_modal_start(&mut stops, &identity, &dep, &app_id);
        assert_eq!(superseded, Vec::<StopRecord>::new());
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].app_id.as_deref(), Some("ap-9"));
        assert_eq!(super::deployed_app_id(None), None);
    }

    /// Crash between the provider start and the final save: only the
    /// write-ahead record exists. It holds the deployment, marks it pending,
    /// carries its stop record, and destroy plans it.
    #[test]
    fn write_ahead_state_leaves_a_pending_stoppable_record() {
        let graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let mut progress = Progress::default();
        progress.stops.push(StopRecord::host(&id("alpha"), None));
        let ahead = write_ahead_state(&graph, None, &progress, &id("alpha"));
        assert_eq!(ahead.pending, vec![id("alpha")]);
        assert!(ahead.graph.deployments.iter().any(|d| d.id == id("alpha")));
        assert_eq!(ahead.stops, vec![StopRecord::host(&id("alpha"), None)]);
        assert_eq!(ahead.endpoints.len(), 0, "no handle yet: the crash case");

        let plan = crate::commands::destroy::plan_destroy(&ahead.graph, &[], &[]).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert!(plan.items[0].launch.is_none(), "a destroy action");
    }

    /// The write-ahead save must not replace the previous instance's stop
    /// identity: both are present until the old one is confirmed stopped.
    #[test]
    fn write_ahead_keeps_the_old_identity_alongside_the_new_one() {
        let graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let old = StopRecord::modal(&id("alpha"), Some("ap-123".to_owned()), None);
        let mut progress = Progress::default();
        progress.stops.push(old.clone());
        progress.stops.push(StopRecord::host(&id("alpha"), None));
        let ahead = write_ahead_state(&graph, None, &progress, &id("alpha"));
        assert!(ahead.stops.contains(&old), "{:?}", ahead.stops);
        assert!(ahead.stops.contains(&StopRecord::host(&id("alpha"), None)));
    }

    #[test]
    fn a_provider_change_makes_the_old_identity_stale() {
        let old_modal = StopRecord::modal(&id("alpha"), Some("ap-1".to_owned()), None);
        let host = StopRecord::host(&id("alpha"), None);
        let other = StopRecord::host(&id("beta"), None);
        let stops = vec![old_modal.clone(), host.clone(), other];
        // Moving to the host: the Modal app is stale, the host unit is not,
        // and another deployment's record is never touched.
        assert_eq!(stale_records(&stops, &id("alpha"), &host), vec![old_modal]);
        // Moving to Modal: the host unit is stale.
        let new_modal = StopRecord::modal(&id("alpha"), None, None);
        assert_eq!(stale_records(&stops, &id("alpha"), &new_modal), vec![host]);
        // Same target: nothing is stale (a restart replaces in place).
        let only = vec![StopRecord::host(&id("alpha"), None)];
        assert_eq!(
            stale_records(&only, &id("alpha"), &StopRecord::host(&id("alpha"), None)).len(),
            0
        );
    }

    /// State written before stop records existed: identities are derived from
    /// the graph (host label; Modal app name plus the endpoint's app id).
    #[test]
    fn v1_state_gets_derived_stop_records() {
        let host_graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let derived = effective_stops(Some(&state(host_graph)));
        assert_eq!(derived.len(), 2);
        assert!(derived.iter().all(|r| r.backend == Backend::host()));
        assert_eq!(derived[0].identity, "io.amicu.llama.alpha");

        let modal_graph = amicu_hcl::parse_graph(&CONFIG.replace("local_apple", "modal")).unwrap();
        let mut s = state(modal_graph);
        s.endpoints = vec![amicu_runtime::RuntimeHandle {
            id: "ap-77".to_owned(),
            deployment: id("alpha"),
            endpoint: "https://x".to_owned(),
            exposure: amicu_runtime::EndpointExposure::Loopback,
        }];
        let derived = effective_stops(Some(&s));
        let alpha = derived
            .iter()
            .find(|r| r.deployment == id("alpha"))
            .unwrap();
        assert_eq!(alpha.backend, Backend::Modal);
        assert_eq!(alpha.identity, "amicu-alpha");
        assert_eq!(alpha.app_id.as_deref(), Some("ap-77"));
    }

    /// A pending deployment removed from the config still gets a destroy.
    #[test]
    fn a_pending_deployment_removed_from_config_is_destroyed() {
        let prior = amicu_hcl::parse_graph(CONFIG).unwrap();
        let mut s = state(prior.clone());
        s.pending = vec![id("beta")];
        let mut desired = prior;
        desired.deployments.retain(|d| d.id != id("beta"));
        let items = pending_destroys(&s, &desired).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].deployment, id("beta"));
        assert!(items[0].launch.is_none());
        // Still wanted: planned as a normal (re)create instead.
        let kept = amicu_hcl::parse_graph(CONFIG).unwrap();
        assert_eq!(pending_destroys(&s, &kept).unwrap().len(), 0);
    }

    #[test]
    fn pending_is_cleared_only_on_confirmed_start() {
        let graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let mut progress = Progress::default();
        progress.stops.push(StopRecord::host(&id("alpha"), None));
        let ahead = write_ahead_state(&graph, None, &progress, &id("alpha"));
        assert_eq!(pending_after(Some(&ahead), &graph, &[]), vec![id("alpha")]);
        let started = [super::host_handle(&id("alpha"), "io.amicu.llama.alpha")];
        assert_eq!(pending_after(Some(&ahead), &graph, &started).len(), 0);
    }

    /// A Modal restart that yields a different app id never overwrites the old
    /// record: it stays, and is handed back to be stopped and confirmed.
    #[test]
    fn a_new_modal_app_id_never_overwrites_a_different_old_one() {
        let dep = id("alpha");
        let old = StopRecord::modal(&dep, Some("ap-OLD".to_owned()), None);
        let placeholder = StopRecord::modal(&dep, None, None);
        let mut stops = vec![old.clone(), placeholder];
        let superseded = super::record_modal_start(&mut stops, "amicu-alpha", &dep, "ap-NEW");
        assert_eq!(superseded, vec![old.clone()]);
        assert!(stops.contains(&old), "old id kept: {stops:?}");
        assert!(
            stops.iter().any(|r| r.app_id.as_deref() == Some("ap-NEW")),
            "{stops:?}"
        );
        assert_eq!(stops.len(), 2);

        // Same id again: the placeholder just merges away.
        let mut stops = vec![
            StopRecord::modal(&dep, Some("ap-1".to_owned()), None),
            StopRecord::modal(&dep, None, None),
        ];
        let superseded = super::record_modal_start(&mut stops, "amicu-alpha", &dep, "ap-1");
        assert_eq!(superseded.len(), 0);
        assert_eq!(stops.len(), 1);
    }

    #[test]
    fn recovery_text_names_the_command_that_ran() {
        use super::{Op, record_partial};
        let dir = std::env::temp_dir().join(format!("amicu-recovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let project = crate::project::Project::new(&dir);
        let graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let mut progress = Progress::default();
        progress.stopped.push(id("alpha"));
        let boom = || crate::error::CliError::Message("boom".to_owned());
        let destroy =
            record_partial(&project, &graph, None, &progress, boom(), Op::Destroy).to_string();
        assert!(destroy.contains("destroy stopped part-way"), "{destroy}");
        assert!(destroy.contains("`amicu destroy`"), "{destroy}");
        assert!(!destroy.contains("`amicu apply`"), "{destroy}");
        let apply =
            record_partial(&project, &graph, None, &progress, boom(), Op::Apply).to_string();
        assert!(apply.contains("`amicu apply`"), "{apply}");

        // Nothing done yet: the destroy error still carries a recovery hint.
        let first = record_partial(
            &project,
            &graph,
            None,
            &Progress::default(),
            boom(),
            Op::Destroy,
        )
        .to_string();
        assert!(first.contains("amicu destroy"), "{first}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// If recording the partial state fails, the message must list exactly
    /// what may still be running (labels and app ids) so it can be stopped
    /// by hand.
    #[test]
    fn a_failed_save_lists_the_exact_labels_and_app_ids() {
        use super::{Op, record_partial};
        let file = std::env::temp_dir().join(format!("amicu-not-a-dir-{}", std::process::id()));
        std::fs::write(&file, "x").unwrap();
        // The state dir cannot be created under a regular file.
        let project = crate::project::Project::new(&file);
        let graph = amicu_hcl::parse_graph(CONFIG).unwrap();
        let mut progress = Progress {
            in_flight: Some(id("alpha")),
            started: vec![amicu_runtime::RuntimeHandle {
                id: "h".to_owned(),
                deployment: id("beta"),
                endpoint: "http://127.0.0.1:1".to_owned(),
                exposure: amicu_runtime::EndpointExposure::Loopback,
            }],
            ..Progress::default()
        };
        progress.stops = vec![
            StopRecord::host(&id("alpha"), None),
            StopRecord::modal(&id("beta"), Some("ap-777".to_owned()), None),
            StopRecord::host(&id("gamma"), None),
        ];
        let msg = record_partial(
            &project,
            &graph,
            None,
            &progress,
            crate::error::CliError::Message("boom".to_owned()),
            Op::Apply,
        )
        .to_string();
        assert!(msg.contains("FAILED"), "{msg}");
        assert!(msg.contains("io.amicu.llama.alpha"), "{msg}");
        assert!(
            msg.contains("ap-777") && msg.contains("amicu-beta"),
            "{msg}"
        );
        assert!(!msg.contains("gamma"), "untouched deployment listed: {msg}");
        let _ = std::fs::remove_file(&file);
    }
}

#[cfg(test)]
mod scoped_tests {
    use super::*;

    #[test]
    fn a_legacy_unscoped_record_is_stale_against_the_project_scoped_one() {
        let legacy = StopRecord::host(&"chat".into(), None);
        let scoped = StopRecord::host_in("aaaa1111", &"chat".into(), None, None);
        let stale = stale_records(std::slice::from_ref(&legacy), &"chat".into(), &scoped);
        assert_eq!(
            stale,
            vec![legacy],
            "stopped and confirmed before the new start"
        );
    }

    #[cfg(unix)]
    #[test]
    fn relative_model_paths_are_resolved_at_start_and_only_for_model_flags() {
        let argv = [
            "llama-server",
            "-m",
            "models/m/a.gguf",
            "--model-draft",
            "models/d/b.gguf",
            "--port",
            "8080",
            "--mmproj",
            "vision/mm.gguf",
            "--chat-template-file=tpl/chat.jinja",
            "-mm",
            "/abs/already.gguf",
        ];
        let mut launch = LaunchPlan {
            deployment: "chat".into(),
            runtime: amicu_runtime::RuntimeKind::LlamaCpp,
            rendered: amicu_runtime::RenderedLaunch::Command {
                argv: argv.iter().map(|s| (*s).to_owned()).collect(),
                text: String::new(),
            },
            settings: amicu_core::resource::Runtime::LlamaCpp(
                amicu_core::resource::LlamaCppSettings::default(),
            ),
            secret_refs: Vec::new(),
            api_key: None,
            service_label: None,
            estimate: amicu_runtime::ResourceEstimate {
                estimated_memory: amicu_core::ByteSize::from_bytes(0),
                gpu_count: 0,
                kv_cache: None,
            },
        };
        resolve_model_paths(&mut launch, std::path::Path::new("/work/proj"));
        let argv = launch.rendered.argv().unwrap();
        assert_eq!(argv[2], "/work/proj/models/m/a.gguf");
        assert_eq!(argv[4], "/work/proj/models/d/b.gguf");
        assert_eq!(argv[6], "8080");
        assert_eq!(argv[8], "/work/proj/vision/mm.gguf");
        assert_eq!(argv[9], "--chat-template-file=/work/proj/tpl/chat.jinja");
        assert_eq!(
            argv[11], "/abs/already.gguf",
            "absolute paths are untouched"
        );
    }
}

/// Table tests for the pure partial-state functions:
/// failure at every position, update vs create vs destroy, in-flight entries.
#[cfg(test)]
mod partial_state_tests {
    use super::*;
    use crate::commands::apply::merge_tests::CONFIG;
    use amicu_core::resource::LlamaCppSettings;

    fn id(s: &str) -> DeploymentId {
        DeploymentId::new(s)
    }

    fn ids(v: &[&str]) -> Vec<DeploymentId> {
        v.iter().map(|s| id(s)).collect()
    }

    /// alpha, beta, gamma, all on model `m` / hardware `hw`; `port_base` makes
    /// the deployment definitions differ between "prior" and "desired".
    fn graph(names: &[&str], port_base: u16) -> Graph {
        let mut g = amicu_hcl::parse_graph(CONFIG).unwrap();
        let template = g.deployments[0].clone();
        g.deployments.clear();
        for (i, n) in names.iter().enumerate() {
            let mut d = template.clone();
            d.id = id(n);
            if let Runtime::LlamaCpp(LlamaCppSettings { port, .. }) = &mut d.runtime {
                *port = port_base + u16::try_from(i).unwrap();
            }
            g.deployments.push(d);
        }
        g
    }

    fn port(g: &Graph, n: &str) -> u16 {
        match &find_deployment(g, &id(n)).unwrap().runtime {
            Runtime::LlamaCpp(s) => s.port,
            Runtime::Vllm(_) => unreachable!(),
        }
    }

    fn handle(n: &str) -> RuntimeHandle {
        RuntimeHandle {
            id: format!("unit-{n}"),
            deployment: id(n),
            endpoint: format!("http://{n}"),
            exposure: EndpointExposure::Loopback,
        }
    }

    fn fp(tag: &str) -> Fingerprint {
        Fingerprint {
            secrets: Some(tag.to_owned()),
            ..Fingerprint::default()
        }
    }

    fn prior_state(g: Graph, pending: &[&str]) -> LocalState {
        LocalState {
            version: LocalState::VERSION.to_owned(),
            project_id: None,
            endpoints: g
                .deployments
                .iter()
                .map(|d| handle(d.id.as_str()))
                .collect(),
            fingerprints: g
                .deployments
                .iter()
                .map(|d| (d.id.clone(), fp("old")))
                .collect(),
            pending: ids(pending),
            stops: Vec::new(),
            serial: 0,
            purge_pending: Vec::new(),
            extra: std::collections::BTreeMap::default(),
            graph: g,
        }
    }

    const ALL: [&str; 3] = ["alpha", "beta", "gamma"];

    /// Failure at each position of an update-everything run: everything before
    /// the failure is recorded as new, the failed one is new AND pending, and
    /// everything after keeps its prior definition.
    #[test]
    fn failure_at_each_position_records_exactly_what_happened() {
        let prior = graph(&ALL, 8000);
        let desired = graph(&ALL, 9000);
        for fail_at in 0..=ALL.len() {
            let started = &ALL[..fail_at];
            let in_flight = ALL.get(fail_at).map(|s| id(s));
            let got = partial_graph(
                Some(&prior),
                &desired,
                &ids(started),
                in_flight.as_ref(),
                &[],
            );
            assert_eq!(got.deployments.len(), 3, "position {fail_at}");
            for (i, n) in ALL.iter().enumerate() {
                let want = if i <= fail_at && (i < fail_at || in_flight.is_some()) {
                    port(&desired, n)
                } else {
                    port(&prior, n)
                };
                assert_eq!(port(&got, n), want, "position {fail_at}, {n}");
            }
        }
    }

    #[test]
    fn recorded_state_marks_only_the_in_flight_one_pending_and_keeps_its_old_fingerprint() {
        let prior = prior_state(graph(&ALL, 8000), &[]);
        let desired = graph(&ALL, 9000);
        let mut progress = Progress::new(Some(&prior));
        progress.started.push(handle("alpha"));
        progress.fingerprints.insert(id("alpha"), fp("new"));
        progress.in_flight = Some(id("beta"));
        let s = recorded_state(Some(&prior), &desired, &progress);
        assert_eq!(s.pending, ids(&["beta"]));
        assert_eq!(s.fingerprints[&id("alpha")], fp("new"));
        // Unconfirmed: still differs from what is running, so still an Update.
        assert_eq!(s.fingerprints[&id("beta")], fp("old"));
        assert_eq!(s.fingerprints[&id("gamma")], fp("old"));
        assert_eq!(port(&s.graph, "gamma"), 8002);
    }

    /// Create + update + destroy in one run, with the destroy confirmed and a
    /// later create failing mid-start.
    #[test]
    fn create_update_and_destroy_mix() {
        let prior = prior_state(graph(&["alpha", "beta"], 8000), &[]);
        let mut desired = graph(&["alpha", "delta"], 9000);
        desired.deployments.retain(|d| d.id != id("beta"));
        let mut progress = Progress::new(Some(&prior));
        progress.stopped.push(id("beta"));
        progress.started.push(handle("alpha"));
        progress.fingerprints.insert(id("alpha"), fp("new"));
        progress.in_flight = Some(id("delta"));
        let s = recorded_state(Some(&prior), &desired, &progress);
        let names: Vec<&str> = s.graph.deployments.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"alpha") && names.contains(&"delta"));
        assert!(!names.contains(&"beta"), "a confirmed stop leaves state");
        assert_eq!(s.pending, ids(&["delta"]));
        assert!(!s.fingerprints.contains_key(&id("beta")));
        assert!(s.endpoints.iter().all(|h| h.deployment != id("beta")));
        assert_eq!(
            s.endpoints
                .iter()
                .find(|h| h.deployment == id("alpha"))
                .unwrap()
                .endpoint,
            "http://alpha"
        );
    }

    /// An unconfirmed stop never drops anything: the deployment (and so its
    /// pending/stop bookkeeping) stays when it is not in `stopped`.
    #[test]
    fn an_unstopped_removed_deployment_stays_in_state() {
        let prior = prior_state(graph(&["alpha", "beta"], 8000), &[]);
        let desired = graph(&["alpha"], 8000);
        let progress = Progress::new(Some(&prior));
        let s = recorded_state(Some(&prior), &desired, &progress);
        assert_eq!(s.graph.deployments.len(), 2);
        assert!(s.fingerprints.contains_key(&id("beta")));
    }

    #[test]
    fn existing_pending_survives_unless_confirmed_started() {
        let prior = prior_state(graph(&ALL, 8000), &["alpha", "beta"]);
        let desired = graph(&ALL, 8000);
        let mut progress = Progress::new(Some(&prior));
        progress.started.push(handle("alpha"));
        let s = recorded_state(Some(&prior), &desired, &progress);
        assert_eq!(s.pending, ids(&["beta"]));
    }

    #[test]
    fn a_model_shared_with_an_unreached_deployment_keeps_its_prior_definition() {
        let prior = graph(&["alpha", "beta"], 8000);
        let mut desired = graph(&["alpha", "beta"], 8000);
        desired.models[0].context_length = Some(1);
        let got = partial_graph(Some(&prior), &desired, &ids(&["alpha"]), None, &[]);
        assert_eq!(got.models[0], prior.models[0]);
        // Once nothing unreached uses it, it moves to the new definition.
        let got = partial_graph(Some(&prior), &desired, &ids(&["alpha", "beta"]), None, &[]);
        assert_eq!(got.models[0], desired.models[0]);
    }

    #[test]
    fn no_prior_and_a_failed_first_start_records_it_pending() {
        let desired = graph(&["alpha"], 8000);
        let progress = Progress {
            in_flight: Some(id("alpha")),
            ..Progress::default()
        };
        let s = recorded_state(None, &desired, &progress);
        assert_eq!(s.pending, ids(&["alpha"]));
        assert_eq!(s.graph.deployments.len(), 1);
    }

    #[test]
    fn unchanged_deployments_get_a_baseline_and_recorded_ones_are_never_overwritten() {
        let g = graph(&["alpha", "beta"], 8000);
        let mut prior = prior_state(g.clone(), &[]);
        prior.fingerprints.remove(&id("beta"));
        let result = PlanResult {
            plan: amicu_plan::Plan {
                version: amicu_plan::Plan::VERSION.to_owned(),
                actions: Vec::new(),
                policy: None,
            },
            items: Vec::new(),
            diagnostics: Vec::new(),
            fingerprints: [(id("alpha"), fp("now")), (id("beta"), fp("now"))].into(),
        };
        let mut out = prior.fingerprints.clone();
        adopt_baselines(&mut out, &g, Some(&prior), &result);
        assert_eq!(
            out[&id("alpha")],
            fp("old"),
            "never overwrite a recorded one"
        );
        assert_eq!(out[&id("beta")], fp("now"));
    }

    #[test]
    fn an_adopted_baseline_never_records_the_launch_digest() {
        let g = graph(&["beta"], 8000);
        let mut prior = prior_state(g.clone(), &[]);
        prior.fingerprints.remove(&id("beta"));
        let now = fp("now").with_launch("llama-server --ctx-size 8192");
        let result = PlanResult {
            plan: amicu_plan::Plan {
                version: amicu_plan::Plan::VERSION.to_owned(),
                actions: Vec::new(),
                policy: None,
            },
            items: Vec::new(),
            diagnostics: Vec::new(),
            fingerprints: [(id("beta"), now)].into(),
        };
        let mut out = prior.fingerprints.clone();
        adopt_baselines(&mut out, &g, Some(&prior), &result);
        assert_eq!(out[&id("beta")].secrets, Some("now".to_owned()));
        assert_eq!(out[&id("beta")].launch, None);
    }
}

#[cfg(test)]
mod purge_pending_tests {
    use super::*;

    fn rec(identity: &str, logs: &[&str]) -> StopRecord {
        let mut r = StopRecord::host(&"d".into(), None);
        r.identity = identity.to_owned();
        r.logs = logs.iter().map(|l| (*l).to_owned()).collect();
        r
    }

    fn prior(stops: Vec<StopRecord>) -> LocalState {
        LocalState {
            version: LocalState::VERSION.to_owned(),
            project_id: None,
            serial: 0,
            graph: Graph::new(),
            endpoints: Vec::new(),
            pending: Vec::new(),
            stops,
            purge_pending: Vec::new(),
            fingerprints: std::collections::BTreeMap::default(),
            extra: std::collections::BTreeMap::default(),
        }
    }

    #[test]
    fn a_superseded_record_with_different_logs_keeps_its_files_tracked() {
        let p = prior(vec![rec("old", &["/l/old.out.log"])]);
        let out = purge_pending_after(Some(&p), &[rec("new", &["/l/new.out.log"])]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].logs, ["/l/old.out.log"]);
    }

    #[test]
    fn logs_still_in_a_live_record_are_not_pending_and_stale_pending_clears() {
        let mut p = prior(vec![rec("a", &["/l/a.out.log"])]);
        p.purge_pending = vec![rec("a", &["/l/a.out.log"]).purge_entry().unwrap()];
        let out = purge_pending_after(Some(&p), &[rec("a", &["/l/a.out.log"])]);
        assert_eq!(out.len(), 0);
    }
}

#[cfg(test)]
mod host_stop_hint_tests {
    use super::host_stop_hint;

    #[test]
    fn names_the_stop_command_of_each_host_os() {
        assert!(host_stop_hint("macos", "io.amicu.x").contains("launchctl bootout"));
        assert!(host_stop_hint("linux", "io.amicu.x").contains("systemctl --user"));
        assert!(host_stop_hint("windows", "io.amicu.x").contains("schtasks"));
    }
}
