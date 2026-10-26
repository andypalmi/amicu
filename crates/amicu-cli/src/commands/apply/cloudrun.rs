//! Google Cloud Run (`provider = "gcp"`, `backend = "cloud_run"`) for apply and
//! destroy: one GPU service per deployment, scaled to zero when idle.
//!
//! The stop record (project, region, service, owner marker and the ids of its
//! Secret Manager secrets) is written to state before anything is created, so
//! a crash never leaves a service or secret that state cannot find.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{GcpAccess, GcpBackend, HardwareProfile};
use amicu_core::secret::SecretResolver;
use amicu_gcp::cloudrun::{
    CloudRunLaunch, CloudRunRecord, CloudRunSupervisor, ServiceAccess, teardown,
};
use amicu_llamacpp::Catalog;
use amicu_runtime::{
    EndpointExposure, GatewayAuth, LaunchPlan, RuntimeAdapter, RuntimeHandle, RuntimeKind,
};

use super::{Env, Progress, deployment_hardware, residency_allowlist, supervisor_err};
use crate::error::CliError;
use crate::gcp_ops::GcpOps;
use crate::stops::{Backend, StopRecord};

/// What a Cloud Run start began from and the supervisor's view of it.
pub(super) struct CloudRunStart {
    /// The write-ahead stop record.
    pub stop: StopRecord,
    created: Arc<AtomicBool>,
    secrets_touched: Arc<AtomicBool>,
    /// No record of this service was in state before this run.
    fresh: bool,
    public: bool,
}

/// Whether `hardware` selects Cloud Run.
pub(crate) fn hardware_is_cloud_run(hardware: &HardwareProfile) -> bool {
    hardware.provider == super::gce::GCP_PROVIDER
        && hardware
            .gcp
            .as_ref()
            .is_some_and(|g| g.backend == GcpBackend::CloudRun)
}

/// Whether deployment `id` runs on Cloud Run.
pub(crate) fn deployment_is_cloud_run(graph: &Graph, id: &DeploymentId) -> bool {
    graph
        .deployments
        .iter()
        .find(|d| &d.id == id)
        .and_then(|d| {
            graph
                .hardware_profiles
                .iter()
                .find(|h| h.id == d.hardware_profile)
        })
        .is_some_and(hardware_is_cloud_run)
}

/// A deployment's service as configured from the graph.
pub(crate) fn launch_for(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
    project_id: &str,
) -> Result<CloudRunLaunch, CliError> {
    let (deployment, hardware) = deployment_hardware(graph, catalog, id)?;
    let fail = |e: &dyn std::fmt::Display| CliError::Message(format!("deployment '{id}': {e}"));
    let s = hardware.gcp.clone().unwrap_or_default();
    let project = s
        .project
        .clone()
        .ok_or_else(|| fail(&"its hardware profile has no `gcp { project }`"))?;
    let region = hardware
        .region
        .as_ref()
        .map(|r| r.code.clone())
        .ok_or_else(|| fail(&"a Cloud Run deployment needs a `region`"))?;
    let gpu = hardware
        .accelerator
        .as_ref()
        .map(|a| a.model.clone())
        .ok_or_else(|| fail(&"a Cloud Run deployment needs a GPU accelerator"))?;
    let service_account = s
        .service_account
        .clone()
        .ok_or_else(|| fail(&"a Cloud Run deployment needs a `service_account`"))?;
    if !super::gce::residency_ok(&region, &residency_allowlist(graph)) {
        return Err(fail(&format!(
            "region {region} is not in a jurisdiction the project's data_class allows"
        )));
    }
    let scaling = deployment.scaling.clone().unwrap_or_default();
    let min = amicu_gcp::GcpCatalog::new()
        .cloud_run_gpu(&gpu)
        .ok_or_else(|| fail(&format!("`{gpu}` is not offered on Cloud Run")))?;
    let memory_gib = u32::try_from(hardware.memory.bytes().div_ceil(1 << 30)).unwrap_or(u32::MAX);
    let cores = hardware.cpu.as_ref().map(|c| c.cores);
    Ok(CloudRunLaunch {
        project,
        region,
        project_id: project_id.to_owned(),
        deployment_id: id.as_str().to_owned(),
        gpu,
        service_account,
        max_instances: scaling.max_replicas.unwrap_or(1),
        concurrency: scaling.target_concurrency,
        cpu: cores.filter(|c| *c > min.min_vcpus),
        memory_gib: Some(memory_gib).filter(|m| *m > min.min_memory_gib),
        access: match s.access.unwrap_or_default() {
            GcpAccess::Public => ServiceAccess::PublicApiKey,
            GcpAccess::Iam => ServiceAccess::IamProtected,
        },
        impersonate: s.impersonate_service_account,
    })
}

/// Runs [`crate::gcp_ops::preflight_cloud_run`] once per project, region and
/// identity of the Cloud Run launches, before anything is written.
pub(super) async fn preflight(
    result: &crate::planner::PlanResult,
    graph: &Graph,
    catalog: &dyn Catalog,
    env: &Env<'_>,
) -> Result<(), CliError> {
    let mut checked: Vec<(String, String, Option<String>, String)> = Vec::new();
    for item in result
        .items
        .iter()
        .filter(|i| i.launch.is_some() && deployment_is_cloud_run(graph, &i.deployment))
    {
        let l = launch_for(graph, catalog, &item.deployment, "preflight")?;
        let key = (
            l.project.clone(),
            l.region.clone(),
            l.impersonate.clone(),
            l.service_account.clone(),
        );
        if checked.contains(&key) {
            continue;
        }
        crate::gcp_ops::preflight_cloud_run(
            env.gcp,
            &l.project,
            &l.region,
            l.impersonate.as_deref(),
            &l.service_account,
        )
        .await?;
        checked.push(key);
    }
    Ok(())
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

/// The supervisor for one deployment and what its start is recorded with.
/// Everything that can refuse does so here, before the caller writes state.
pub(super) async fn prepare(
    graph: &Graph,
    catalog: &dyn Catalog,
    launch: &LaunchPlan,
    id: &DeploymentId,
    env: &Env<'_>,
    progress: &Progress,
) -> Result<(CloudRunSupervisor, CloudRunStart), CliError> {
    if RuntimeKind::of(&launch.settings) != RuntimeKind::Vllm {
        return Err(CliError::Message(format!(
            "deployment '{id}': Cloud Run deployments run vLLM only"
        )));
    }
    let spec = launch_for(graph, catalog, id, &progress.project_id)?;
    let public = spec.access == ServiceAccess::PublicApiKey;
    if public {
        let _ = writeln!(
            std::io::stderr(),
            "note: '{id}': the Cloud Run service is public; any request, even one without the \
             api_key, can start a billed GPU instance (up to {} at a time) until it scales back \
             to zero. Set `access = \"iam\"` in its `gcp {{}}` block so only callers with a \
             Google identity token reach it",
            spec.max_instances
        );
    }
    let client = env.gcp.client(spec.impersonate.as_deref())?;
    let new_record = spec
        .record()
        .map_err(|e| CliError::Message(format!("deployment '{id}': {e}")))?;
    let mut supervisor = CloudRunSupervisor::new(client, spec)
        .with_probe_transport(env.gcp.service_transport())
        .with_timing(env.gcp.run_timing())
        .with_progress(env.progress);
    let fresh = !progress
        .stops
        .iter()
        .any(|r| r.backend == Backend::CloudRun && r.identity == new_record.service);
    let prior = progress
        .stops
        .iter()
        .find(|r| {
            r.deployment == *id
                && r.backend == Backend::CloudRun
                && r.identity == new_record.service
        })
        .and_then(|r| r.cloud_run.clone());
    // A record for another project, region or identity is stopped by apply
    // first (as that identity), see `moved_records`.
    if let Some(prior) = prior
        && !moved(&prior, &new_record)
    {
        supervisor = supervisor.with_recorded(prior);
    }
    let secrets = amicu_llamacpp::resolve_launch_secrets(launch, env.resolver).await?;
    supervisor
        .check_before_write(launch, &secrets)
        .map_err(supervisor_err)?;
    let start = CloudRunStart {
        stop: StopRecord::cloud_run(id, new_record),
        created: supervisor.created(),
        secrets_touched: supervisor.secrets_touched(),
        fresh,
        public,
    };
    Ok((supervisor, start))
}

/// The service name depends only on amicu's ids, so a record under another
/// Google project, region or impersonation target has the same identity but
/// names resources the new run cannot reach.
fn moved(old: &CloudRunRecord, new: &CloudRunRecord) -> bool {
    old.project != new.project || old.region != new.region || old.impersonate != new.impersonate
}

/// The recorded services of this deployment that `new` would replace but that
/// live elsewhere (see [`moved`]). Apply stops them, and confirms, before the
/// write-ahead record takes their place in state.
pub(super) fn moved_records(stops: &[StopRecord], new: &StopRecord) -> Vec<StopRecord> {
    let Some(fresh) = new.cloud_run.as_ref() else {
        return Vec::new();
    };
    stops
        .iter()
        .filter(|r| r.deployment == new.deployment && r.same_target(new))
        .filter(|r| r.cloud_run.as_ref().is_some_and(|old| moved(old, fresh)))
        .cloned()
        .collect()
}

/// The write-ahead record, merged with the one already in state: the secrets of
/// both stay listed until a stop confirms them gone.
///
/// # Errors
/// When the record in state belongs to another project, region or identity:
/// it is never overwritten while it may still own resources.
pub(super) fn write_ahead(stops: &mut Vec<StopRecord>, new: &StopRecord) -> Result<(), CliError> {
    let Some(r) = stops.iter_mut().find(|r| r.same_target(new)) else {
        stops.push(new.clone());
        return Ok(());
    };
    let (Some(old), Some(fresh)) = (r.cloud_run.as_ref(), new.cloud_run.as_ref()) else {
        r.cloud_run.clone_from(&new.cloud_run);
        return Ok(());
    };
    if moved(old, fresh) {
        return Err(CliError::Message(format!(
            "the recorded Cloud Run service {} is in project {} ({}) and cannot be replaced by \
             one in project {} ({}) before it is stopped; nothing was changed",
            old.service, old.project, old.region, fresh.project, fresh.region
        )));
    }
    let mut merged = fresh.clone();
    for id in &old.secrets {
        if !merged.secrets.contains(id) {
            merged.secrets.push(id.clone());
        }
    }
    r.cloud_run = Some(merged);
    Ok(())
}

/// After a failed start or exposure check: record that no secret was ever
/// written when this run proves it, then delete what this run created
/// (nothing was serving). Returns a note for the error. The record stays, and
/// the deployment stays pending, until a later apply or destroy confirms the
/// stop.
pub(super) async fn failed_start(
    start: &CloudRunStart,
    adapter: &dyn RuntimeAdapter,
    stops: &mut [StopRecord],
) -> String {
    if !start.created.load(Ordering::SeqCst) {
        return String::new();
    }
    if start.fresh
        && !start.secrets_touched.load(Ordering::SeqCst)
        && let Some(c) = stops
            .iter_mut()
            .find(|r| r.same_target(&start.stop))
            .and_then(|r| r.cloud_run.as_mut())
    {
        c.secrets_unwritten = true;
    }
    let id = start.stop.identity.clone();
    match adapter.stop(&handle(&start.stop.deployment, &id)).await {
        Ok(()) => format!(
            "; Cloud Run service {id} and its secrets, created by this run, were deleted (the \
             record stays until a later apply or destroy confirms it)"
        ),
        Err(e) => format!(
            "; deleting Cloud Run service {id} and its secrets, created by this run, FAILED \
             ({e}); they stay recorded and `amicu destroy --target {}` retries",
            start.stop.deployment
        ),
    }
}

/// The post-deploy check: an anonymous request is refused and, on a public
/// service, the deployment's key is accepted.
pub(super) async fn verify(
    start: &CloudRunStart,
    handle: &RuntimeHandle,
    launch: &LaunchPlan,
    resolver: &dyn SecretResolver,
    gcp: &dyn GcpOps,
) -> Result<(), String> {
    if !start.public {
        let _ = writeln!(
            std::io::stderr(),
            "verifying that {} refuses requests without a Google identity token ...",
            handle.endpoint
        );
        return super::report_verify(
            crate::gcp_ops::verify_service_refuses(gcp, &handle.endpoint).await,
        );
    }
    let Some(key_ref) = &launch.api_key else {
        return Err("the service is public but the deployment declares no `api_key`".to_owned());
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
    super::report_verify(crate::gcp_ops::verify_service(gcp, &handle.endpoint, &key).await)
}

/// Deletes the service, then its secrets, each only if it carries the
/// record's owner marker; `Ok` only once all are confirmed gone.
pub(super) async fn stop(record: &StopRecord, env: &Env<'_>) -> Result<(), CliError> {
    let Some(c) = &record.cloud_run else {
        return Err(CliError::Message(format!(
            "{} has no recorded project or region, so amicu cannot find it; check the Google \
             Cloud console for service '{}'. It was kept in state",
            record.describe(),
            record.identity
        )));
    };
    let client = Arc::new(env.gcp.client(c.impersonate.as_deref())?);
    teardown(&client, c, env.gcp.run_timing().poll)
        .await
        .map(|_| ())
        .map_err(|e| {
            CliError::Message(format!(
                "{}: {e}; it was kept in state, and nothing owned by someone else was touched",
                record.describe()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(project: &str, region: &str, secrets: &[&str]) -> StopRecord {
        StopRecord::cloud_run(
            &"chat".into(),
            CloudRunRecord {
                project: project.to_owned(),
                region: region.to_owned(),
                service: "amicu-p-chat-0123456789ab".to_owned(),
                owner: "owner".to_owned(),
                impersonate: None,
                secrets: secrets.iter().map(|s| (*s).to_owned()).collect(),
                secrets_unwritten: false,
            },
        )
    }

    #[test]
    fn write_ahead_keeps_old_secrets_and_refuses_another_project_or_region() {
        let mut stops = vec![record("p", "europe-west4", &["s-api-key", "s-hf-token"])];
        write_ahead(&mut stops, &record("p", "europe-west4", &["s-api-key"])).unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(
            stops[0].cloud_run.as_ref().unwrap().secrets,
            ["s-api-key", "s-hf-token"]
        );
        let before = stops.clone();
        assert!(write_ahead(&mut stops, &record("q", "europe-west4", &[])).is_err());
        assert!(write_ahead(&mut stops, &record("p", "us-central1", &[])).is_err());
        assert_eq!(stops, before);
    }

    #[test]
    fn moved_records_are_the_same_service_elsewhere() {
        let new = record("p", "us-central1", &[]);
        let old = record("p", "europe-west4", &[]);
        assert_eq!(moved_records(std::slice::from_ref(&old), &new), vec![old]);
        assert_eq!(moved_records(std::slice::from_ref(&new), &new), Vec::new());
    }
}
