//! RunPod pod mode (`runpod { mode = "pod" }`) for apply and destroy: the
//! write-ahead start, replacement, failure teardown and terminate.

use std::io::Write;
use std::sync::atomic::Ordering;

use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{HardwareProfile, RunpodCloud, RunpodMode};
use amicu_llamacpp::{Catalog, ProcessSupervisor};
use amicu_runpod::{Cloud, PodStatus, PodSupervisor, RunPodClient, RunPodError, StopOutcome};
use amicu_runtime::{EndpointExposure, GatewayAuth, LaunchPlan, RuntimeAdapter, RuntimeHandle};

use super::{
    Env, Progress, RUNPOD_PROVIDER, RunPodStart, delete_runpod_secrets, deployment_hardware,
    find_deployment, merged_names, stop_record, supervisor_err, sync_runpod_record,
    unknown_runpod_account,
};
use crate::error::CliError;
use crate::runpod_ops::{check_account, current_account, runpod_error};
use crate::stops::{Backend, StopRecord};

pub(super) fn is_pod(hw: &HardwareProfile) -> bool {
    hw.provider == RUNPOD_PROVIDER && hw.runpod_mode() == RunpodMode::Pod
}

pub(super) fn deployment_is_pod(graph: &Graph, id: &DeploymentId) -> bool {
    find_deployment(graph, id)
        .and_then(|d| {
            graph
                .hardware_profiles
                .iter()
                .find(|h| h.id == d.hardware_profile)
        })
        .is_some_and(is_pod)
}

/// What a RunPod record names, for messages.
pub(super) fn kind(record: &StopRecord) -> &'static str {
    if record.backend == Backend::RunpodPod {
        "pod"
    } else {
        "endpoint"
    }
}

fn handle(deployment: &DeploymentId, id: &str) -> RuntimeHandle {
    RuntimeHandle {
        id: id.to_owned(),
        deployment: deployment.clone(),
        endpoint: String::new(),
        exposure: EndpointExposure::Public {
            gateway: GatewayAuth::None,
        },
    }
}

fn build(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
    project_id: &str,
    account: &str,
    env: &Env<'_>,
    prior: Option<&StopRecord>,
) -> Result<PodSupervisor, CliError> {
    let (deployment, hardware) = deployment_hardware(graph, catalog, id)?;
    let cloud = match hardware.runpod.and_then(|r| r.cloud) {
        Some(RunpodCloud::Community) => Cloud::Community,
        _ => Cloud::Secure,
    };
    let mut supervisor =
        PodSupervisor::for_hardware(env.runpod.client()?, project_id, id.as_str(), &hardware)
            .map_err(supervisor_err)?
            .with_probe_transport(env.runpod.probe())
            .with_workspace(account)
            .with_cloud(cloud)
            .with_confirmed_account(true);
    if let Some(prior) = prior {
        if let Some(pod) = &prior.app_id {
            supervisor = supervisor.with_recorded_pod(pod);
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

/// The supervisor for one pod deployment, plus what its start is recorded
/// with. A recorded pod whose GPU, count, cloud or data center no longer
/// matches is terminated first (confirmed, like any stale record), so the
/// start creates its replacement. The caller has checked every RunPod record
/// of the deployment against `account`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare(
    graph: &Graph,
    catalog: &dyn Catalog,
    launch: &LaunchPlan,
    id: &DeploymentId,
    account: &str,
    env: &Env<'_>,
    host_probe: &dyn ProcessSupervisor,
    progress: &mut Progress,
) -> Result<(PodSupervisor, RunPodStart), CliError> {
    let name = amicu_runpod::project_endpoint_name(&progress.project_id, id.as_str());
    let mut recorded = progress
        .stops
        .iter()
        .find(|r| r.deployment == *id && r.backend == Backend::RunpodPod && r.identity == name)
        .cloned();
    if let Some(old) = recorded.clone()
        && let Some(pod_id) = &old.app_id
    {
        let probe = build(graph, catalog, id, &progress.project_id, account, env, None)?;
        let why = match probe.client().get_pod(pod_id).await {
            Ok(pod) if pod.status != PodStatus::Terminated => probe.replacement_reason(&pod),
            Ok(_) | Err(RunPodError::NotFound { .. }) => None,
            Err(e) => {
                return Err(runpod_error(
                    &format!("could not read RunPod pod {pod_id} of '{id}'; nothing was changed"),
                    &e,
                ));
            }
        };
        if let Some(why) = why {
            let _ = writeln!(
                std::io::stderr(),
                "replacing RunPod pod {pod_id} of '{id}' ({why}): it is terminated before the \
                 new pod starts"
            );
            stop_record(
                &old,
                env,
                host_probe,
                &progress.project_id,
                &mut progress.stops,
            )
            .await?;
            progress.stops.retain(|r| !r.same_instance(&old));
            recorded = None;
        }
    }
    let supervisor = build(
        graph,
        catalog,
        id,
        &progress.project_id,
        account,
        env,
        recorded.as_ref(),
    )?;
    let launch_secrets = amicu_llamacpp::resolve_launch_secrets(launch, env.resolver).await?;
    let planned = supervisor
        .planned_secret_names(launch, &launch_secrets)
        .map_err(supervisor_err)?;
    let start = RunPodStart {
        pod: true,
        account: account.to_owned(),
        recorded_id: recorded.as_ref().and_then(|r| r.app_id.clone()),
        secrets: merged_names(
            recorded.as_ref().map_or(&[][..], |r| &r.secrets[..]),
            &planned,
        ),
        deployed: supervisor.deployed_pod(),
        created_endpoint: supervisor.created_pod(),
        created_secrets: supervisor.created_secrets(),
    };
    Ok((supervisor, start))
}

/// After a failed pod start or exposure check: record what exists, and
/// terminate a pod this run created. A pod that existed before is left in
/// place after a failed start, but terminated when it failed the exposure
/// check (it must not keep serving). The record stays until a stop is
/// confirmed. Returns a note for the error.
pub(super) async fn failed_start(
    rp: &RunPodStart,
    adapter: &dyn RuntimeAdapter,
    record: &StopRecord,
    stops: &mut [StopRecord],
    exposure_failed: bool,
) -> String {
    let deployed = rp.deployed.lock().ok().and_then(|d| d.clone());
    sync_runpod_record(stops, record, deployed.as_deref(), rp, false);
    let Some(id) = deployed else {
        return String::new();
    };
    let created = rp.created_endpoint.load(Ordering::SeqCst);
    if !created && !exposure_failed {
        return format!(
            "; RunPod pod {id} existed before this run, so it was left in place and stays \
             recorded (`amicu destroy --target {}` removes it)",
            record.deployment
        );
    }
    let origin = if created {
        "created by this run"
    } else {
        "which existed before this run"
    };
    match adapter.stop(&handle(&record.deployment, &id)).await {
        Ok(()) => {
            sync_runpod_record(stops, record, None, rp, true);
            format!(
                "; RunPod pod {id}, {origin}, was terminated (it stays recorded until a later \
                 apply or destroy confirms it is gone)"
            )
        }
        Err(e) => format!(
            "; terminating RunPod pod {id}, {origin}, FAILED ({e}); it stays recorded and \
             `amicu destroy --target {}` retries",
            record.deployment
        ),
    }
}

/// Terminates a pod (never just stops it) and deletes its secrets. Only a
/// confirmed terminate in the recorded account is `Ok`; anything else keeps
/// the record. A record without a pod id is resolved by exact name and owner
/// marker.
pub(super) async fn stop(
    record: &StopRecord,
    env: &Env<'_>,
    project_id: &str,
    stops: &mut [StopRecord],
) -> Result<(), CliError> {
    let client = env.runpod.client()?;
    let Some(active) = current_account(&client).await? else {
        return unknown_runpod_account(record, env);
    };
    check_account(
        record.deployment.as_str(),
        record.workspace.as_deref(),
        Some(&active),
    )?;
    let id = match &record.app_id {
        Some(id) => id.clone(),
        None => match owned_pod_by_name(&client, record, &active, project_id, env).await? {
            Some(id) => id,
            None => return delete_runpod_secrets(&client, record, env, stops).await,
        },
    };
    let supervisor = PodSupervisor::new(client, "", record.deployment.as_str())
        .with_recorded_secrets(record.secrets.clone())
        .with_confirmed_account(true);
    let kept = match supervisor.stop_pod(&handle(&record.deployment, &id)).await {
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

/// The id of the one live pod named `record.identity` that carries this
/// deployment's owner marker in `account`, or `None` when no pod has the
/// name. A foreign or duplicate name is refused, unless
/// `--forget-unconfirmed` (the foreign pod is then left untouched).
async fn owned_pod_by_name(
    client: &RunPodClient,
    record: &StopRecord,
    account: &str,
    project_id: &str,
    env: &Env<'_>,
) -> Result<Option<String>, CliError> {
    let lookup = |e: &RunPodError| {
        runpod_error(
            &format!(
                "{} has no pod id recorded and looking it up by name failed; it was kept in \
                 state",
                record.describe()
            ),
            e,
        )
    };
    let found = client
        .find_pods_by_name(&record.identity)
        .await
        .map_err(|e| lookup(&e))?;
    let one = match found.as_slice() {
        [] => return Ok(None),
        [one] => one,
        many => {
            return Err(CliError::Message(format!(
                "{} has no pod id recorded and several RunPod pods are named '{}' ({}); \
                 terminate the extra ones in the RunPod console. It was kept in state",
                record.describe(),
                record.identity,
                many.iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };
    let pod = client.get_pod(&one.id).await.map_err(|e| lookup(&e))?;
    let owner = amicu_runpod::owner_marker(Some(account), project_id, record.deployment.as_str());
    if pod.env.owner.as_deref() == Some(owner.as_str()) {
        return Ok(Some(pod.id));
    }
    if env.forget_unconfirmed {
        let _ = writeln!(
            std::io::stderr(),
            "warning: RunPod pod {} is named '{}' but does not carry this deployment's {} \
             marker; it was NOT terminated, and '{}' is forgotten as you asked \
             (--forget-unconfirmed)",
            pod.id,
            pod.name,
            amicu_runpod::OWNER_ENV_VAR,
            record.deployment
        );
        return Ok(None);
    }
    Err(CliError::Message(format!(
        "{} has no pod id recorded, and RunPod pod {} named '{}' does not carry this \
         deployment's {} marker, so amicu will not terminate it. Check it in the RunPod \
         console; if this deployment never created a pod, run `amicu destroy --target {} \
         --forget-unconfirmed`. It was kept in state",
        record.describe(),
        pod.id,
        pod.name,
        amicu_runpod::OWNER_ENV_VAR,
        record.deployment
    )))
}
