//! `amicu plan [--output json]`.

use std::collections::BTreeMap;
use std::io::Write;

use amicu_core::diagnostic::{DiagnosticReport, Severity};
use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{HardwareProfile, RunpodCloud, RunpodMode};
use amicu_modal::ModalSupervisor;
use amicu_modal::supervisor::project_app_name;
use amicu_plan::Plan;
use amicu_plan::contract::{ActionKind, ChangeOp, PlannedAction, PlannedChange, Setting};

use crate::OutputFormat;
use crate::checks::report_for;
use crate::error::CliError;
use crate::planner::{PlanError, PlanItem, PlanResult, build_project_plan};
use crate::project::Project;
use crate::stops::{Backend, StopRecord};
use crate::write_json;

/// Build and render the plan; exits non-zero when validation errors block it.
pub(crate) fn run(
    project: &Project,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    run_with_host(project, output, out, crate::secret::env_has_secret, &|| {
        crate::host::HostInfo::detect().ok().map(|h| h.facts())
    })
}

/// [`run`] with the secret lookup injected and no host check.
#[cfg(test)]
pub(crate) fn run_with(
    project: &Project,
    output: OutputFormat,
    out: &mut dyn Write,
    secret_is_set: fn(&str) -> bool,
) -> Result<u8, CliError> {
    run_with_host(project, output, out, secret_is_set, &|| None)
}

/// [`run_with`] plus the host facts, injected. `host` is called at most once,
/// and only when a deployment targets a local profile, so a project that
/// plans for Modal never probes the machine. `None` (detection failed)
/// skips the host check.
pub(crate) fn run_with_host(
    project: &Project,
    output: OutputFormat,
    out: &mut dyn Write,
    secret_is_set: fn(&str) -> bool,
    host: &dyn Fn() -> Option<amicu_engine::host::HostFacts>,
) -> Result<u8, CliError> {
    let graph = project.load_graph()?;

    // Surface structural errors (dangling refs, duplicate ids) through the same
    // diagnostic path as `validate`, before `build_plan` can hard-error on them.
    let report = report_for(&graph);
    if !report.valid {
        emit_report(&report, output, out)?;
        return Ok(1);
    }

    let state = crate::refresh::load_refreshed_state(
        project,
        &crate::refresh::ModalLookup,
        &crate::runpod_ops::RealRunPod,
        output,
        out,
    )?;
    let recorded = state
        .as_ref()
        .map(|s| s.fingerprints.clone())
        .unwrap_or_default();
    // The same records apply acts on: the recorded ones plus the legacy-named
    // ones derived for state written before stop records existed.
    let stops = crate::commands::apply::effective_stops(state.as_ref());
    let project_id = state.as_ref().and_then(|s| s.project_id.clone());
    let prior = state.map(|s| {
        // A pending deployment (start unconfirmed) is planned as absent.
        let mut g = s.graph;
        g.deployments.retain(|d| !s.pending.contains(&d.id));
        g
    });
    let mut result = match build_project_plan(project, &graph, prior.as_ref(), &recorded) {
        Ok(result) => result,
        // A deployment failed to plan (e.g. a CAP-002/CAP-003 extra_args/env
        // conflict); `build_plan` hands back everything it collected instead
        // of a bare adapter error, so render it the same way as any other
        // blocking report.
        Err(PlanError(report)) => {
            emit_report(&report, output, out)?;
            return Ok(1);
        }
    };

    // apply would fail on these after starting work, so fail here instead.
    let unset = crate::secret::missing_secrets_diagnostics(&result, secret_is_set);
    result.diagnostics.extend(unset);

    // A local profile that declares more memory than this machine has: a
    // warning (AMICU-MEM-003), since plan may run elsewhere than the target.
    let has_local = graph.hardware_profiles.iter().any(|h| {
        amicu_core::resource::is_local_provider(&h.provider)
            && graph.deployments.iter().any(|d| d.hardware_profile == h.id)
    });
    if has_local && let Some(facts) = host() {
        result
            .diagnostics
            .extend(amicu_engine::host::host_diagnostics(&graph, &facts));
    }

    if result.has_errors() {
        // Cannot produce a reviewable plan for an invalid config.
        emit_errors(&result, output, out)?;
        return Ok(1);
    }

    if output.is_json() {
        let switches = provider_switches(&result, &graph, &stops, project_id.as_deref());
        let mut plan = result.plan.clone();
        add_switch_actions(&mut plan, &switches);
        write_json(out, &plan)?;
    } else {
        let switches = provider_switches(&result, &graph, &stops, project_id.as_deref());
        print_human_with(&result, &graph, &switches, project_id.as_deref(), out)?;
    }
    Ok(0)
}

fn emit_errors(
    result: &PlanResult,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    emit_report(
        &DiagnosticReport::new(result.diagnostics.clone()),
        output,
        out,
    )
}

fn emit_report(
    report: &DiagnosticReport,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    if output.is_json() {
        write_json(out, report)?;
    } else {
        writeln!(out, "Cannot plan: configuration has errors.")
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

/// Per deployment, the recorded instances (`<backend> <identity>`) that a
/// provider change leaves behind: they are stopped and confirmed before the
/// new instance is started, so the plan shows that as its own step.
pub(crate) type ProviderSwitches = BTreeMap<DeploymentId, Vec<OldInstance>>;

/// One recorded instance a provider switch replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OldInstance {
    pub backend: &'static str,
    pub identity: String,
    /// A Modal app replaced by another Modal app (the name scheme changed).
    pub modal_rename: bool,
}

impl OldInstance {
    fn describe(&self) -> String {
        format!("{} {}", self.backend, self.identity)
    }
}

/// The provider-switch destroy steps for `result` given the recorded `stops`.
/// A recorded instance of a deployment that is being launched is replaced
/// when it is not the target the new start will use: the same test `apply`
/// uses (backend and identity differ, see `StopRecord::same_target`), so a
/// legacy unscoped host label replaced by a project-scoped one shows too.
/// Destroys are not switches.
pub(crate) fn provider_switches(
    result: &PlanResult,
    graph: &Graph,
    stops: &[StopRecord],
    project_id: Option<&str>,
) -> ProviderSwitches {
    let mut switches = ProviderSwitches::new();
    for item in result.items.iter().filter(|i| i.launch.is_some()) {
        let id = item.deployment.as_str();
        let (backend, identity) = match item.action.provider.as_deref() {
            Some("modal") => (Backend::Modal, project_id.map(|p| project_app_name(p, id))),
            Some("runpod") => (
                if runpod_hardware(graph, &item.deployment)
                    .is_some_and(|h| h.runpod_mode() == RunpodMode::Pod)
                {
                    Backend::RunpodPod
                } else {
                    Backend::Runpod
                },
                project_id.map(|p| amicu_runpod::project_endpoint_name(p, id)),
            ),
            _ => {
                // Without a project id apply mints one, so no recorded host label
                // can match the new one.
                (
                    Backend::host(),
                    project_id.map(|p| amicu_llamacpp::project_host_label(p, id)),
                )
            }
        };
        let old: Vec<OldInstance> = stops
            .iter()
            .filter(|r| {
                r.deployment == item.deployment
                    && !(r.backend == backend && Some(&r.identity) == identity.as_ref())
            })
            .map(|r| OldInstance {
                backend: r.backend.name(),
                identity: r.identity.clone(),
                modal_rename: r.backend == Backend::Modal && backend == Backend::Modal,
            })
            .collect();
        if !old.is_empty() {
            switches.insert(item.deployment.clone(), old);
        }
    }
    switches
}

/// Insert each `destroy old` step into the JSON plan as a Destroy action
/// directly before the deployment's create/update action (`provider` names
/// the old backend), so the JSON has the same action count as the human plan.
pub(crate) fn add_switch_actions(plan: &mut Plan, switches: &ProviderSwitches) {
    let mut actions = Vec::with_capacity(plan.actions.len());
    for action in std::mem::take(&mut plan.actions) {
        let id = action
            .address
            .strip_prefix("deployment.")
            .unwrap_or_default();
        for (dep, olds) in switches {
            if dep.as_str() == id {
                for old in olds {
                    actions.push(PlannedAction {
                        kind: ActionKind::Destroy,
                        address: action.address.clone(),
                        artifact: None,
                        runtime: vec![Setting {
                            key: "old_identity".to_owned(),
                            value: old.identity.clone(),
                        }],
                        hardware: None,
                        provider: Some(old.backend.to_owned()),
                        region: None,
                        cost_performance: None,
                        required_secrets: Vec::new(),
                        required_action: None,
                        reason: None,
                        changes: Vec::new(),
                    });
                }
            }
        }
        actions.push(action);
    }
    plan.actions = actions;
}

/// The `destroy old` step of a provider switch, with what a Modal rename costs.
fn write_destroy_old(out: &mut dyn Write, old: &OldInstance, address: &str) -> std::io::Result<()> {
    writeln!(out, "  - destroy old ({}) for {address}", old.describe())?;
    if old.modal_rename {
        writeln!(
            out,
            "      one-time rename of the Modal app: the endpoint URL changes (clients must \
             update), the app is down until the new one is up, and there is no rollback \
             to the previous version if the new deploy fails"
        )?;
    }
    writeln!(
        out,
        "      stopped and confirmed before the new instance starts\n"
    )
}

pub(crate) fn print_human(
    result: &PlanResult,
    graph: &Graph,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    print_human_with(result, graph, &ProviderSwitches::new(), None, out)
}

/// [`print_human`], plus a `destroy old` step before each provider-switch
/// create. `project_id` names RunPod endpoints and pods once state has one.
pub(crate) fn print_human_with(
    result: &PlanResult,
    graph: &Graph,
    switches: &ProviderSwitches,
    project_id: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let io = |r: std::io::Result<()>| r.map_err(|e| CliError::io("write failed", e));

    if result.items.is_empty() {
        io(writeln!(
            out,
            "No changes. Infrastructure matches the configuration."
        ))?;
    } else {
        let steps: usize = switches.values().map(Vec::len).sum();
        io(writeln!(
            out,
            "Plan: {} action(s).\n",
            result.items.len() + steps
        ))?;
        for item in &result.items {
            for old in switches.get(&item.deployment).into_iter().flatten() {
                io(write_destroy_old(out, old, &item.action.address))?;
            }
            let kind = format!("{:?}", item.action.kind).to_lowercase();
            io(writeln!(out, "  {} {}", symbol(&kind), item.action.address))?;
            io(writeln!(out, "      action:   {kind}"))?;
            if let Some(reason) = &item.reason {
                io(writeln!(out, "      reason:   {reason}"))?;
            }
            io(write_changes(out, &item.changes, home_dir().as_deref()))?;
            if let Some(hw) = &item.action.hardware {
                io(writeln!(out, "      hardware: {hw}"))?;
            }
            // `provider`/`region` were already computed by `action_for`
            // (`amicu_engine::planner`) for every hardware profile, not just
            // Modal's - this just surfaces them.
            // The exact Modal `gpu=` string (`"H100:2"`) is not shown here:
            // deriving it needs `amicu-modal`'s mapping, and this print path
            // is shared with the HTTP API's `/plan` display contract, so
            // pulling that crate in here was avoided (see
            // `crates/amicu-modal/src/supervisor.rs`'s packaging note).
            if let Some(provider) = &item.action.provider {
                match &item.action.region {
                    Some(region) => {
                        io(writeln!(
                            out,
                            "      target:   {provider} (region {region})"
                        ))?;
                    }
                    None => io(writeln!(out, "      target:   {provider}"))?,
                }
            }
            let remote_launch = write_runpod(out, graph, item, project_id)
                .map_err(|e| CliError::io("write failed", e))?;
            if let Some(artifact) = &item.action.artifact {
                io(writeln!(out, "      artifact: {}", artifact.name))?;
            }
            // The endpoint API key: always redacted, and only the secret's name
            // is shown - the value is never read at plan time.
            if let Some(shown) = item.action.runtime.iter().find(|s| s.key == "api_key") {
                let secret = graph
                    .deployments
                    .iter()
                    .find(|d| d.id == item.deployment)
                    .and_then(|d| d.api_key.as_ref());
                match secret {
                    Some(r) => io(writeln!(
                        out,
                        "      api_key:  {} (secret \"{}\")",
                        shown.value, r.name
                    ))?,
                    None => io(writeln!(out, "      api_key:  {}", shown.value))?,
                }
            }
            if item
                .action
                .runtime
                .iter()
                .any(|s| s.key == "memory_check" && s.value == "warn")
            {
                io(writeln!(out, "      memory_check: warn (bypassed)"))?;
            }
            if let Some(cmd) = &item.rendered {
                // On Modal the supervisor rewrites the argv before launch
                // (`-hf`/`-hff`, `--no-mmproj`, `--host 0.0.0.0`); show what
                // is really sent, via the supervisor's own transformation.
                let cmd = if let Some(remote) = remote_launch {
                    remote
                } else if item.action.provider.as_deref() == Some("modal") {
                    modal_launch_text(graph, item).unwrap_or_else(|| cmd.clone())
                } else {
                    cmd.clone()
                };
                io(writeln!(out, "      launch:   {cmd}"))?;
            }
            if let Some(cp) = &item.action.cost_performance {
                for a in &cp.assumptions {
                    io(writeln!(out, "      note:     {a}"))?;
                }
            }
            io(writeln!(out))?;
        }
    }

    for d in &result.diagnostics {
        let marker = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warn",
            Severity::Info => "info",
        };
        match &d.resource {
            Some(resource) => io(writeln!(out, "  {marker}: {resource}: {}", d.message))?,
            None => io(writeln!(out, "  {marker}: {}", d.message))?,
        }
    }
    Ok(())
}

/// The hardware profile of `deployment` when it runs on RunPod.
fn runpod_hardware<'g>(graph: &'g Graph, deployment: &DeploymentId) -> Option<&'g HardwareProfile> {
    let d = graph.deployments.iter().find(|d| &d.id == deployment)?;
    graph
        .hardware_profiles
        .iter()
        .find(|h| h.id == d.hardware_profile && h.provider == "runpod")
}

/// How a RunPod deployment is billed, with a static hourly estimate when one
/// is known for its GPU.
fn runpod_billing(graph: &Graph, deployment: &DeploymentId) -> Option<String> {
    let hw = runpod_hardware(graph, deployment)?;
    let acc = hw.accelerator.as_ref();
    Some(match hw.runpod_mode() {
        RunpodMode::Pod => {
            let community = hw.runpod.and_then(|r| r.cloud) == Some(RunpodCloud::Community);
            match acc.and_then(|a| amicu_runpod::pod_hourly_usd(a, community)) {
                Some(usd) => format!("always on \u{b7} ~${usd:.2}/hr"),
                None => "always on".to_owned(),
            }
        }
        RunpodMode::Serverless => match acc.and_then(amicu_runpod::serverless_hourly_usd) {
            Some(usd) => format!("scale to zero \u{b7} ~${usd:.2}/hr while active"),
            None => "scale to zero".to_owned(),
        },
    })
}

/// How many RunPod workers a deployment may run: serverless min/max and
/// idle window as `apply` sends them, or the one always-on pod.
fn runpod_workers(graph: &Graph, deployment: &DeploymentId) -> Option<String> {
    let hw = runpod_hardware(graph, deployment)?;
    if hw.runpod_mode() == RunpodMode::Pod {
        return Some("1 pod (always on, never scales)".to_owned());
    }
    let d = graph.deployments.iter().find(|d| &d.id == deployment)?;
    let w = amicu_runpod::workers_config(d.scaling.as_ref());
    let idle = w.idle_timeout.map_or_else(String::new, |s| {
        format!(" \u{b7} scale down after {} idle", duration_text(s))
    });
    Some(format!("min {} \u{b7} max {}{idle}", w.min, w.max))
}

/// The RunPod lines of `item` (billing, workers, what the create body holds);
/// returns the remote launch line.
fn write_runpod(
    out: &mut dyn Write,
    graph: &Graph,
    item: &PlanItem,
    project_id: Option<&str>,
) -> std::io::Result<Option<String>> {
    if item.launch.is_none() {
        return Ok(None);
    }
    if let Some(billing) = runpod_billing(graph, &item.deployment) {
        writeln!(out, "      billing:  {billing}")?;
    }
    if let Some(workers) = runpod_workers(graph, &item.deployment) {
        writeln!(out, "      workers:  {workers}")?;
    }
    let Some(remote) = runpod_remote(graph, item, project_id) else {
        return Ok(None);
    };
    for (key, value) in &remote.details {
        writeln!(out, "      {:<9} {value}", format!("{key}:"))?;
    }
    Ok(Some(remote.launch))
}

/// What a RunPod deployment is created with, rendered from the create body
/// `apply` sends.
struct RunpodRemote {
    details: Vec<(&'static str, String)>,
    launch: String,
}

/// The parts of a create body the plan shows, shared by endpoints and pods.
struct RemoteBody {
    details: Vec<(&'static str, String)>,
    image: String,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    data_centers: Vec<String>,
    disk_gb: Option<u32>,
}

fn runpod_remote(graph: &Graph, item: &PlanItem, project_id: Option<&str>) -> Option<RunpodRemote> {
    let hw = runpod_hardware(graph, &item.deployment)?;
    let launch = item.launch.as_ref()?;
    let catalog = crate::catalog::build_catalog(graph);
    let client = amicu_runpod::RunPodClient::new(amicu_runpod::ApiKey::new(String::new()));
    let (id, pid) = (&item.deployment, project_id.unwrap_or_default());
    let named = |name: String| match project_id {
        Some(_) => name,
        None => "named on first apply".to_owned(),
    };
    let body = match hw.runpod_mode() {
        RunpodMode::Serverless => {
            crate::commands::apply::configured_endpoint(client, graph, &catalog, id, pid)
                .map_err(|e| e.to_string())
                .and_then(|s| s.planned_endpoint(launch).map_err(|e| e.to_string()))
                .map(|b| {
                    let excluding = if b.gpu.excluded_types.is_empty() {
                        String::new()
                    } else {
                        format!(" (excluding {})", b.gpu.excluded_types.join(", "))
                    };
                    let gpu = format!("{} x {}{excluding}", b.gpu.count, b.gpu.pools.join(", "));
                    RemoteBody {
                        details: vec![("endpoint", named(b.name)), ("gpu", gpu)],
                        image: b.image,
                        argv: b.entrypoint.into_iter().chain(b.cmd).collect(),
                        env: b.env,
                        data_centers: b.data_center_ids,
                        disk_gb: b.disk_gb,
                    }
                })
        }
        RunpodMode::Pod => crate::commands::apply::configured_pod(client, graph, &catalog, id, pid)
            .map_err(|e| e.to_string())
            .and_then(|s| {
                let pod = s.planned_pod(launch).map_err(|e| e.to_string())?;
                let argv = s.launch_argv(launch).map_err(|e| e.to_string())?;
                Ok((pod, argv))
            })
            .map(|(b, argv)| RemoteBody {
                details: vec![
                    ("pod", named(b.name)),
                    ("gpu", format!("{} x {}", b.gpu_count, b.gpu_type_id)),
                    ("cloud", b.cloud.wire().to_ascii_lowercase()),
                ],
                image: b.image,
                argv,
                env: b.env,
                data_centers: b.data_center_ids,
                disk_gb: b.disk_gb,
            }),
    };
    let body = match body {
        Ok(body) => body,
        Err(e) => {
            return Some(RunpodRemote {
                details: Vec::new(),
                launch: format!("cannot be derived: {e}"),
            });
        }
    };
    let mut details = body.details;
    details.insert(1, ("image", body.image));
    details.push((
        "location",
        if body.data_centers.is_empty() {
            "any data center".to_owned()
        } else {
            body.data_centers.join(", ")
        },
    ));
    if let Some(gb) = body.disk_gb {
        details.push(("disk", format!("{gb} GB")));
    }
    let secrets: Vec<&str> = body
        .env
        .iter()
        .filter(|(_, v)| v.contains("RUNPOD_SECRET_"))
        .map(|(k, _)| k.as_str())
        .collect();
    if !secrets.is_empty() {
        details.push((
            "secrets",
            format!("{} (as RunPod secrets)", secrets.join(", ")),
        ));
    }
    Some(RunpodRemote {
        details,
        launch: amicu_engine::planner::redacted_argv_text(&body.argv),
    })
}

fn duration_text(secs: u32) -> String {
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m{s:02}s"),
    }
}

/// The launch line `ModalSupervisor` would really send for `item`, or `None`
/// when it cannot be derived (the caller then shows the local rendering;
/// `apply` surfaces the underlying error).
fn modal_launch_text(graph: &Graph, item: &PlanItem) -> Option<String> {
    let launch = item.launch.as_ref()?;
    // The app name is not part of the launch line, so a placeholder will do.
    let mut supervisor = ModalSupervisor::new("amicu-plan");
    if let Some(model) = graph
        .deployments
        .iter()
        .find(|d| d.id == item.deployment)
        .and_then(|d| graph.models.iter().find(|m| m.id == d.model))
    {
        supervisor = supervisor.with_gguf_model(model);
    }
    supervisor
        .launch_argv(launch)
        .ok()
        .map(|argv| amicu_engine::planner::redacted_argv_text(&argv))
}

/// How many changed fields an update lists before "and N more".
const SHOWN_CHANGES: usize = 6;
/// Longest value printed before it is cut with an ellipsis.
const MAX_VALUE_CHARS: usize = 80;

fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|h| h.len() > 1)
}

/// A value as shown: the user's home prefix becomes `~`, long values are cut,
/// and string values are quoted (numbers, booleans and JSON are not).
fn display_value(v: &str, home: Option<&str>) -> String {
    let mut text = v.to_owned();
    if let Some(home) = home
        && let Some(rest) = v.strip_prefix(home)
        && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
    {
        text = format!("~{rest}");
    }
    let bare = text.parse::<f64>().is_ok()
        || matches!(text.as_str(), "true" | "false")
        || text.starts_with('[')
        || text.starts_with('{');
    if text.chars().count() > MAX_VALUE_CHARS {
        text = text.chars().take(MAX_VALUE_CHARS - 1).collect::<String>() + "\u{2026}";
    }
    if bare { text } else { format!("\"{text}\"") }
}

/// One change line: `~ key: "old" -> "new"`, `+ key: "new"` or `- key: "old"`;
/// a redacted field never shows a value.
fn change_line(c: &PlannedChange, home: Option<&str>) -> String {
    let val = |v: &Option<String>| v.as_deref().map(|v| display_value(v, home));
    match (c.op, c.redacted) {
        (ChangeOp::Added, true) => format!("+ {}: (set)", c.key),
        (ChangeOp::Removed, true) => format!("- {}: (unset)", c.key),
        (ChangeOp::Changed, true) => format!("~ {}: (changed)", c.key),
        (ChangeOp::Added, false) => format!("+ {}: {}", c.key, val(&c.new).unwrap_or_default()),
        (ChangeOp::Removed, false) => format!("- {}: {}", c.key, val(&c.old).unwrap_or_default()),
        (ChangeOp::Changed, false) => format!(
            "~ {}: {} -> {}",
            c.key,
            val(&c.old).unwrap_or_default(),
            val(&c.new).unwrap_or_default()
        ),
    }
}

fn write_changes(
    out: &mut dyn Write,
    changes: &[PlannedChange],
    home: Option<&str>,
) -> std::io::Result<()> {
    for c in changes.iter().take(SHOWN_CHANGES) {
        writeln!(out, "      {}", change_line(c, home))?;
    }
    if changes.len() > SHOWN_CHANGES {
        writeln!(out, "      and {} more", changes.len() - SHOWN_CHANGES)?;
    }
    Ok(())
}

fn symbol(kind: &str) -> &'static str {
    match kind {
        "create" => "+",
        "destroy" => "-",
        "replace" => "±",
        _ => "~",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use amicu_core::ByteSize;
    use amicu_core::resource::{
        Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Deployment, GgufVariant,
        HardwareProfile, LlamaCppSettings, Model, ModelSource, Quantization, Runtime,
        SourceRegistry, VllmSettings,
    };

    use super::*;
    use crate::planner::build_plan;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn graph(provider: &str, runtime: Runtime) -> Graph {
        let mut g = Graph::new();
        g.models.push(Model {
            id: "m".into(),
            source: ModelSource {
                registry: SourceRegistry::HuggingFace,
                reference: "org/tiny-1B".into(),
            },
            architecture: "llama".into(),
            modalities: Vec::new(),
            artifact: Artifact {
                format: ArtifactFormat::Gguf,
                quantization: None,
            },
            context_length: Some(8192),
            variants: vec![GgufVariant {
                quantization: Quantization::Q4KM,
                file: "m.q4_k_m.gguf".into(),
                size: Some(ByteSize::from_bytes(GIB)),
                sha256: None,
            }],
            n_layers: None,
            n_kv_heads: None,
            n_kv_layers: None,
            head_dim: None,
        });
        g.hardware_profiles.push(HardwareProfile {
            id: "hw".into(),
            provider: provider.into(),
            region: None,
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::Gpu,
                model: "l4".into(),
                count: 1,
                memory: ByteSize::from_bytes(24 * GIB),
                memory_bandwidth_gbps: None,
            }),
            cpu: None,
            memory: ByteSize::from_bytes(48 * GIB),
            interconnect: None,
            price: None,
            runpod: None,
        });
        g.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
            id: "dep".into(),
            model: "m".into(),
            hardware_profile: "hw".into(),
            runtime,
        });
        g
    }

    fn llama() -> Runtime {
        Runtime::LlamaCpp(LlamaCppSettings {
            context_length: 8192,
            port: 8000,
            n_gpu_layers: -1,
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            ..Default::default()
        })
    }

    fn vllm() -> Runtime {
        Runtime::Vllm(VllmSettings {
            context_length: 8192,
            port: 8000,
            ..Default::default()
        })
    }

    fn launch_line(g: &Graph) -> String {
        let result = build_plan(g, None).expect("plan");
        let mut out = Vec::new();
        print_human(&result, g, &mut out).expect("print");
        let text = String::from_utf8(out).expect("utf8");
        text.lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("launch:")
                    .map(|c| c.trim().to_owned())
            })
            .unwrap_or_else(|| panic!("no launch line in:\n{text}"))
    }

    #[cfg(unix)]
    #[test]
    fn project_plan_keeps_the_model_path_relative() {
        let g = graph("local", llama());
        let project = Project::new("/proj");
        let result = crate::planner::build_project_plan(
            &project,
            &g,
            None,
            &std::collections::BTreeMap::default(),
        )
        .expect("plan");
        let launch = result.items[0].rendered.clone().expect("rendered");
        assert!(launch.contains("models/m/"), "{launch}");
        assert!(
            !launch.contains("/proj"),
            "no absolute project path: {launch}"
        );
    }

    fn human_plan(g: &Graph) -> String {
        let result = build_plan(g, None).expect("plan");
        let mut out = Vec::new();
        print_human(&result, g, &mut out).expect("print");
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn api_key_shows_redacted_with_the_secret_name_only() {
        let mut g = graph("modal", vllm());
        assert!(!human_plan(&g).contains("api_key"));
        g.deployments[0].api_key = Some(amicu_core::secret::SecretRef::new("CHAT_API_KEY"));
        let text = human_plan(&g);
        let line = text
            .lines()
            .find(|l| l.trim().starts_with("api_key:"))
            .unwrap_or_else(|| panic!("no api_key line in:\n{text}"));
        assert!(line.contains("******"), "{line}");
        assert!(line.contains("CHAT_API_KEY"), "{line}");
        assert_eq!(line.trim(), "api_key:  ****** (secret \"CHAT_API_KEY\")");
        // Planning never resolves: even with the variable set, its value is
        // nowhere in the output.
        assert!(!text.contains("--api-key"), "{text}");
    }

    #[test]
    fn human_plan_names_the_deployment_whose_memory_check_is_bypassed() {
        let mut g = graph("local", llama());
        let mut tight = g.hardware_profiles[0].clone();
        tight.id = "tight".into();
        tight.memory = ByteSize::from_bytes(GIB / 2);
        tight.accelerator.as_mut().unwrap().memory = ByteSize::from_bytes(GIB / 2);
        g.hardware_profiles.push(tight);
        let mut bypassed = g.deployments[0].clone();
        bypassed.id = "bypassed".into();
        bypassed.hardware_profile = "tight".into();
        bypassed.memory_check = amicu_core::resource::MemoryCheck::Warn;
        g.deployments.push(bypassed);

        let text = human_plan(&g);
        assert_eq!(
            text.matches("memory_check: warn (bypassed)").count(),
            1,
            "{text}"
        );
        let warn = text
            .lines()
            .find(|l| l.trim().starts_with("warn:"))
            .unwrap_or_else(|| panic!("no warning in:\n{text}"));
        assert!(warn.contains("deployment.bypassed:"), "{warn}");
        assert!(warn.contains("bypassed by config"), "{warn}");
    }

    #[test]
    fn modal_vllm_shows_all_interfaces_bind() {
        let line = launch_line(&graph("modal", vllm()));
        assert!(line.contains("--host 0.0.0.0"), "{line}");
        assert!(!line.contains("127.0.0.1"), "{line}");
    }

    #[test]
    fn modal_llamacpp_shows_the_argv_modal_really_runs() {
        let line = launch_line(&graph("modal", llama()));
        assert!(
            line.contains("-hf org/tiny-1B -hff m.q4_k_m.gguf"),
            "{line}"
        );
        assert!(!line.contains(" -m "), "{line}");
        assert!(line.contains("--host 0.0.0.0"), "{line}");
        assert!(line.contains("--no-mmproj"), "{line}");
    }

    fn runpod_graph(runtime: Runtime, mode: RunpodMode) -> Graph {
        let mut g = graph("runpod", runtime);
        g.hardware_profiles[0].region = Some(amicu_core::resource::Region::from_code("eu"));
        g.hardware_profiles[0].runpod =
            Some(amicu_core::resource::RunpodSettings { mode, cloud: None });
        g.deployments[0].api_key = Some(amicu_core::secret::SecretRef::new("RUNPOD_KEY"));
        g
    }

    fn line<'t>(text: &'t str, key: &str) -> &'t str {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key).map(str::trim))
            .unwrap_or_else(|| panic!("no {key} line in:\n{text}"))
    }

    fn assert_remote_only(text: &str) {
        assert!(!text.contains("127.0.0.1"), "{text}");
        assert!(!text.contains("models/"), "{text}");
        assert!(!text.contains(" -m "), "{text}");
        assert!(!text.contains("--api-key"), "{text}");
    }

    #[test]
    fn runpod_serverless_llamacpp_shows_what_the_endpoint_runs() {
        let text = human_plan(&runpod_graph(llama(), RunpodMode::Serverless));
        let launch = line(&text, "launch:");
        assert!(launch.starts_with("/app/llama-server "), "{launch}");
        assert!(
            launch.contains("-hf org/tiny-1B -hff m.q4_k_m.gguf"),
            "{launch}"
        );
        assert!(launch.contains("--host 0.0.0.0"), "{launch}");
        assert!(launch.contains("--port 8080"), "{launch}");
        assert!(launch.contains("--no-mmproj"), "{launch}");
        assert_remote_only(&text);
        assert_eq!(line(&text, "endpoint:"), "named on first apply");
        assert_eq!(
            line(&text, "image:"),
            amicu_llamacpp::remote::DEFAULT_LLAMACPP_IMAGE
        );
        assert!(line(&text, "gpu:").starts_with("1 x "), "{text}");
        let placement = line(&text, "location:");
        assert!(placement.contains("EU-"), "{placement}");
        assert!(!placement.contains("US-"), "{placement}");
        assert_eq!(line(&text, "disk:"), "100 GB");
        assert!(line(&text, "workers:").starts_with("min 0"), "{text}");
        assert_eq!(line(&text, "api_key:"), "****** (secret \"RUNPOD_KEY\")");
    }

    #[test]
    fn runpod_endpoint_is_named_once_the_project_has_an_id() {
        let g = runpod_graph(vllm(), RunpodMode::Serverless);
        let result = build_plan(&g, None).expect("plan");
        let mut out = Vec::new();
        print_human_with(&result, &g, &ProviderSwitches::new(), Some("p1"), &mut out)
            .expect("print");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(
            line(&text, "endpoint:"),
            amicu_runpod::project_endpoint_name("p1", "dep")
        );
        let launch = line(&text, "launch:");
        assert!(launch.contains("--host 0.0.0.0"), "{launch}");
        assert_remote_only(&text);
    }

    #[test]
    fn runpod_pod_shows_what_the_pod_runs() {
        let text = human_plan(&runpod_graph(llama(), RunpodMode::Pod));
        let launch = line(&text, "launch:");
        assert!(
            launch.contains("-hf org/tiny-1B -hff m.q4_k_m.gguf"),
            "{launch}"
        );
        assert!(launch.contains("--host 0.0.0.0"), "{launch}");
        assert_remote_only(&text);
        assert_eq!(line(&text, "pod:"), "named on first apply");
        assert_eq!(line(&text, "cloud:"), "secure");
        assert!(line(&text, "gpu:").starts_with("1 x NVIDIA"), "{text}");
        assert!(line(&text, "location:").contains("EU-"), "{text}");
        let secrets = line(&text, "secrets:");
        assert!(secrets.contains("LLAMA_API_KEY"), "{secrets}");
        assert!(secrets.ends_with("(as RunPod secrets)"), "{secrets}");
        assert_eq!(line(&text, "workers:"), "1 pod (always on, never scales)");
    }

    #[test]
    fn runpod_plan_never_shows_secret_values() {
        let mut g = leaky_graph("runpod");
        g.deployments[0].api_key = Some(amicu_core::secret::SecretRef::new("RUNPOD_KEY"));
        let refused = human_plan(&g);
        assert_no_secrets(&refused);
        assert!(
            line(&refused, "launch:").starts_with("cannot be derived: "),
            "{refused}"
        );

        let Runtime::LlamaCpp(s) = &mut g.deployments[0].runtime else {
            unreachable!()
        };
        s.extra_args.retain(|a| !a.starts_with("--hf-token"));
        let text = human_plan(&g);
        assert_no_secrets(&text);
        assert!(!text.contains("amicu-plan-placeholder"), "{text}");
        assert!(line(&text, "launch:").contains("--threads 4"), "{text}");
    }

    fn update_text(prior: &Graph, next: &Graph) -> String {
        let result = build_plan(next, Some(prior)).expect("plan");
        let mut out = Vec::new();
        print_human(&result, next, &mut out).expect("print");
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn a_model_swap_prints_the_old_and_new_reference() {
        let mut prior = graph("local", llama());
        prior.models[0].source.reference = "Qwen/Qwen3.6-27B".into();
        let mut next = prior.clone();
        next.models[0].source.reference = "Qwen/Qwen3.8-27B".into();
        let text = update_text(&prior, &next);
        let head: Vec<&str> = text.lines().take(7).collect();
        assert_eq!(
            head,
            [
                "Plan: 1 action(s).",
                "",
                "  ~ deployment.dep",
                "      action:   update",
                "      reason:   launch settings changed, so the service restarts",
                "      ~ model.source.reference: \"Qwen/Qwen3.6-27B\" -> \"Qwen/Qwen3.8-27B\"",
                "      hardware: gpu.l4-24gb (24 GB)",
            ],
            "{text}"
        );
    }

    #[test]
    fn url_references_never_print_in_human_output() {
        let prior = graph("local", llama());
        for url in [
            "https://h/dl/tok_path_secret/m.gguf",
            "https://h/m.gguf#sig=tok_path_secret",
        ] {
            let mut next = prior.clone();
            next.models[0].source.reference = url.into();
            let full = update_text(&prior, &next);
            // Only the change lines: the `launch:` line is a separate, existing surface.
            let text: String = full
                .lines()
                .filter(|l| !l.contains("launch:"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!text.contains("tok_path_secret"), "{text}");
            assert!(
                text.contains("~ model.source.reference: (changed)"),
                "{text}"
            );
        }
    }

    #[test]
    fn a_non_launch_change_says_configuration_changed() {
        let prior = graph("local", llama());
        let mut next = prior.clone();
        next.hardware_profiles[0].region =
            Some(amicu_core::resource::Region::from_code("eu-west-1"));
        let text = update_text(&prior, &next);
        assert!(text.contains("reason:   configuration changed"), "{text}");
        assert!(!text.contains("restarts"), "{text}");
    }

    #[test]
    fn a_denylisted_field_never_prints_its_value() {
        let prior = graph("local", llama());
        let mut next = prior.clone();
        let Runtime::LlamaCpp(s) = &mut next.deployments[0].runtime else {
            unreachable!()
        };
        s.env
            .insert("HF_TOKEN".into(), "hf_literal_secret_value".into());
        s.extra_args = vec!["--seed".into(), "sk-literal-token".into()];
        s.chat_template = Some("{{ tmpl_secret_marker }}".into());
        next.deployments[0].api_key = Some(amicu_core::secret::SecretRef::new("MY_SECRET_NAME"));
        let text = update_text(&prior, &next);
        let changes: String = text
            .lines()
            .filter(|l| l.trim_start().starts_with(['~', '+', '-']))
            .filter(|l| !l.contains("deployment.dep"))
            .collect::<Vec<_>>()
            .join("\n");
        for leaked in [
            "hf_literal_secret_value",
            "sk-literal-token",
            "--seed",
            "tmpl_secret_marker",
            "MY_SECRET_NAME",
        ] {
            assert!(!changes.contains(leaked), "{leaked} in {changes}");
        }
        assert!(changes.contains("+ env.HF_TOKEN: (set)"), "{changes}");
        assert!(changes.contains("+ extra_args: (set)"), "{changes}");
        assert!(changes.contains("+ chat_template: (set)"), "{changes}");
    }

    fn change(op: ChangeOp, redacted: bool, old: Option<&str>, new: Option<&str>) -> PlannedChange {
        PlannedChange {
            key: "k".into(),
            op,
            old: old.map(str::to_owned),
            new: new.map(str::to_owned),
            restart: true,
            redacted,
        }
    }

    #[test]
    fn change_lines_cover_added_removed_changed_and_redacted() {
        let line = |c: &PlannedChange| change_line(c, None);
        assert_eq!(
            line(&change(ChangeOp::Changed, false, Some("a"), Some("b"))),
            "~ k: \"a\" -> \"b\""
        );
        assert_eq!(
            line(&change(
                ChangeOp::Changed,
                false,
                Some("8192"),
                Some("16384")
            )),
            "~ k: 8192 -> 16384"
        );
        assert_eq!(
            line(&change(ChangeOp::Added, false, None, Some("true"))),
            "+ k: true"
        );
        assert_eq!(
            line(&change(ChangeOp::Removed, false, Some("x"), None)),
            "- k: \"x\""
        );
        assert_eq!(
            line(&change(ChangeOp::Changed, true, None, None)),
            "~ k: (changed)"
        );
        assert_eq!(
            line(&change(ChangeOp::Added, true, None, None)),
            "+ k: (set)"
        );
        assert_eq!(
            line(&change(ChangeOp::Removed, true, None, None)),
            "- k: (unset)"
        );
    }

    #[test]
    fn home_prefix_becomes_a_tilde() {
        let home = Some("/Users/ann");
        assert_eq!(
            display_value("/Users/ann/models/a.gguf", home),
            "\"~/models/a.gguf\""
        );
        assert_eq!(display_value("/Users/anna/x", home), "\"/Users/anna/x\"");
        assert_eq!(display_value("/opt/x", home), "\"/opt/x\"");
    }

    #[test]
    fn long_values_are_truncated_with_an_ellipsis() {
        let long = "a".repeat(200);
        let shown = display_value(&long, None);
        assert_eq!(shown.chars().count(), MAX_VALUE_CHARS + 2, "{shown}");
        assert!(shown.ends_with("\u{2026}\""), "{shown}");
        assert_eq!(display_value(&"b".repeat(80), None).chars().count(), 82);
    }

    #[test]
    fn only_six_changes_are_listed_then_and_n_more() {
        let many: Vec<PlannedChange> = (0..9)
            .map(|i| PlannedChange {
                key: format!("k{i}"),
                ..change(ChangeOp::Changed, false, Some("1"), Some("2"))
            })
            .collect();
        let mut out = Vec::new();
        write_changes(&mut out, &many, None).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 7, "{text}");
        assert!(text.ends_with("      and 3 more\n"), "{text}");
    }

    #[test]
    fn plan_json_carries_reason_and_structured_changes() {
        let mut prior = graph("local", llama());
        prior.models[0].source.reference = "Qwen/Qwen3.6-27B".into();
        let mut next = prior.clone();
        next.models[0].source.reference = "Qwen/Qwen3.8-27B".into();
        let result = build_plan(&next, Some(&prior)).expect("plan");
        let mut out = Vec::new();
        crate::write_json(&mut out, &result.plan).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let a = &v["actions"][0];
        assert_eq!(
            a["reason"],
            "launch settings changed, so the service restarts"
        );
        assert_eq!(
            a["changes"][0],
            serde_json::json!({
                "key": "model.source.reference",
                "op": "changed",
                "old": "Qwen/Qwen3.6-27B",
                "new": "Qwen/Qwen3.8-27B",
                "restart": true,
                "redacted": false,
            })
        );
    }

    #[test]
    fn host_targets_keep_the_local_rendering() {
        for runtime in [llama(), vllm()] {
            let line = launch_line(&graph("local", runtime));
            assert!(!line.contains("0.0.0.0"), "{line}");
            assert!(!line.contains("--no-mmproj"), "{line}");
            assert!(!line.contains("-hf "), "{line}");
        }
    }

    const SECRETS: [&str; 6] = [
        "s3cr3t-extra",
        "s3cr3t-equals",
        "s3cr3t-token-path",
        "s3cr3t-template",
        "s3cr3t-header",
        "s3cr3t-arg-url",
    ];

    fn leaky_graph(provider: &str) -> Graph {
        let mut g = graph(provider, llama());
        g.models[0].source.reference = "https://host.example/dl/s3cr3t-token-path/m.gguf".into();
        let Runtime::LlamaCpp(s) = &mut g.deployments[0].runtime else {
            unreachable!()
        };
        s.extra_args = [
            "--proxy-token",
            "s3cr3t-extra",
            "--hf-token=s3cr3t-equals",
            "--header",
            "Authorization: Bearer s3cr3t-header",
            "--mmproj-url",
            "https://u:s3cr3t-arg-url@cdn.example/p.gguf",
            "--threads",
            "4",
        ]
        .map(String::from)
        .to_vec();
        s.chat_template = Some("{{ s3cr3t-template }}".into());
        g
    }

    fn assert_no_secrets(text: &str) {
        for secret in SECRETS {
            assert!(!text.contains(secret), "{secret} leaked in:\n{text}");
        }
    }

    #[test]
    fn plan_output_never_shows_secrets_in_extra_args_urls_or_templates() {
        for provider in ["local", "modal"] {
            let g = leaky_graph(provider);
            let result = build_plan(&g, None).expect("plan");

            let mut human = Vec::new();
            print_human(&result, &g, &mut human).expect("print");
            let human = String::from_utf8(human).expect("utf8");
            assert_no_secrets(&human);
            assert!(human.contains("(redacted)"), "{human}");
            assert!(human.contains("--threads 4"), "benign args stay: {human}");

            let mut json = Vec::new();
            crate::write_json(&mut json, &result.plan).unwrap();
            let json = String::from_utf8(json).expect("utf8");
            assert_no_secrets(&json);
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            let runtime = v["actions"][0]["runtime"].as_array().unwrap();
            let value = |k: &str| {
                runtime
                    .iter()
                    .find(|s| s["key"] == k)
                    .map(|s| s["value"].as_str().unwrap().to_owned())
            };
            assert_eq!(value("chat_template").as_deref(), Some("(redacted)"));
            let extra = value("extra_args").unwrap();
            assert!(extra.contains("--proxy-token (redacted)"), "{extra}");
            assert!(extra.contains("--hf-token=(redacted)"), "{extra}");
            assert!(extra.contains("--threads 4"), "{extra}");
        }
    }
}
