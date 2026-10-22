//! Google Compute Engine (`provider = "gcp"`) for apply and destroy: one VM
//! and one firewall rule per deployment.
//!
//! The stop record (project, zones, instance, firewall rule, owner marker) is
//! written to state before anything is created, so a crash never leaves a VM
//! that state cannot find. The record also names the deployment's Secret
//! Manager secrets and the VM's certificate fingerprint, so destroy removes the
//! secrets even after a crash. The VM serves HTTPS with a self-signed
//! certificate that every request pins.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{GcpBackend, GcpProvisioning, Residency};
use amicu_gcp::compute::{Provisioning as VmProvisioning, SecretDelivery};
use amicu_gcp::gce::teardown;
use amicu_gcp::{GceLaunch, GceSupervisor, GcpCatalog, candidate_zones, resolve_shape};
use amicu_llamacpp::Catalog;
use amicu_runtime::{
    EndpointExposure, GatewayAuth, LaunchPlan, RuntimeAdapter, RuntimeHandle, RuntimeKind,
};

use super::{Env, Progress, deployment_hardware, residency_allowlist, supervisor_err};
use crate::error::CliError;
use crate::stops::{Backend, StopRecord};

pub(super) const GCP_PROVIDER: &str = "gcp";

/// A flex-start request waits this long for capacity.
const FLEX_WAIT: Duration = Duration::from_hours(1);
const DEFAULT_BOOT_DISK_GB: u32 = 100;

/// What a Compute Engine start began from and the supervisor's view of it.
pub(super) struct GceStart {
    /// The write-ahead stop record.
    pub stop: StopRecord,
    /// The VM's certificate (public), written next to state once it serves.
    pub cert_pem: Option<String>,
    landed: Arc<Mutex<Option<String>>>,
    created: Arc<AtomicBool>,
}

/// The Google project and impersonation target a deployment acts with.
pub(super) fn access_for(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
) -> Result<(String, Option<String>), CliError> {
    let (_, hardware) = deployment_hardware(graph, catalog, id)?;
    let settings = hardware.gcp.unwrap_or_default();
    let project = settings.project.ok_or_else(|| {
        CliError::Message(format!(
            "deployment '{id}': its hardware profile has no `gcp {{ project }}`"
        ))
    })?;
    Ok((project, settings.impersonate_service_account))
}

/// Whether the deployment opted into delivering its secrets as instance
/// metadata.
pub(super) fn uses_metadata_secrets(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
) -> Result<bool, CliError> {
    let (_, hardware) = deployment_hardware(graph, catalog, id)?;
    Ok(hardware.gcp.unwrap_or_default().insecure_metadata_secrets)
}

fn residency_ok(region: &str, allowed: &[Residency]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let catalog = GcpCatalog::new();
    let Some(r) = catalog.region(region) else {
        return false;
    };
    allowed.iter().any(|a| match a {
        Residency::Eu => r.eu,
        Residency::Uk => r.country == "GB",
        Residency::Us => r.country == "US",
        Residency::Ca => r.country == "CA",
        Residency::Apac | Residency::Other(_) => false,
    })
}

/// A deployment's VM as configured from the graph: GPU, machine, zones in
/// order, provisioning, lifetime. `source_ranges` is who may reach the port.
pub(crate) fn launch_for(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
    project_id: &str,
    source_ranges: Vec<String>,
) -> Result<GceLaunch, CliError> {
    let (_, hardware) = deployment_hardware(graph, catalog, id)?;
    let s = hardware.gcp.clone().unwrap_or_default();
    let project = s.project.clone().ok_or_else(|| {
        CliError::Message(format!(
            "deployment '{id}': its hardware profile has no `gcp {{ project }}`"
        ))
    })?;
    if s.backend == GcpBackend::CloudRun {
        return Err(CliError::Message(format!(
            "deployment '{id}': `backend = \"cloud_run\"` is not supported by apply yet; use \
             `backend = \"compute\"`"
        )));
    }
    let acc = hardware.accelerator.as_ref().ok_or_else(|| {
        CliError::Message(format!(
            "deployment '{id}': a GCP VM needs a GPU accelerator"
        ))
    })?;
    let fail = |e: &dyn std::fmt::Display| CliError::Message(format!("deployment '{id}': {e}"));
    let provisioning = amicu_engine::gcp_checks::catalog_provisioning(s.provisioning);
    let shape = resolve_shape(
        &acc.model,
        acc.count,
        s.machine_type.as_deref(),
        provisioning,
    )
    .map_err(|e| fail(&e))?;
    let fixed = GcpCatalog::new()
        .machine_type(&shape.machine_type)
        .map(|m| m.name);
    let region = hardware.region.as_ref().map(|r| r.code.as_str());
    let mut zones =
        candidate_zones(&acc.model, fixed, region, s.zone.as_deref()).map_err(|e| fail(&e))?;
    let allowed = residency_allowlist(graph);
    zones.retain(|z| amicu_gcp::zone_region(z).is_ok_and(|r| residency_ok(r, &allowed)));
    if zones.is_empty() {
        return Err(fail(
            &"no candidate zone lies in a jurisdiction the project's data_class allows",
        ));
    }
    GceLaunch {
        project,
        project_id: project_id.to_owned(),
        deployment_id: id.as_str().to_owned(),
        zones: zones.into_iter().map(str::to_owned).collect(),
        shape,
        provisioning: match s.provisioning {
            GcpProvisioning::Spot => VmProvisioning::Spot,
            GcpProvisioning::FlexStart => VmProvisioning::FlexStart { wait: FLEX_WAIT },
            GcpProvisioning::Standard => VmProvisioning::Standard,
        },
        max_run: Duration::from_secs(u64::from(s.max_run_secs())),
        boot_disk_gb: s.boot_disk_gb.unwrap_or(DEFAULT_BOOT_DISK_GB),
        service_account: s.service_account,
        source_ranges,
        impersonate: s.impersonate_service_account,
        delivery: if s.insecure_metadata_secrets {
            SecretDelivery::InstanceMetadata
        } else {
            SecretDelivery::SecretManager
        },
        tls: None,
    }
    .with_tls()
    .map_err(|e| fail(&e))
}

/// Who may reach the serving port: the profile's list, else this machine's
/// public IP as a `/32`. Never everyone, unless the profile says so.
async fn source_ranges(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
    env: &Env<'_>,
) -> Result<Vec<String>, CliError> {
    let (_, hardware) = deployment_hardware(graph, catalog, id)?;
    let listed = hardware.gcp.unwrap_or_default().allowed_source_cidrs;
    if !listed.is_empty() {
        return Ok(listed);
    }
    let ip = env.gcp.caller_ip().await.map_err(|e| {
        CliError::Message(format!(
            "deployment '{id}': {e}; set `allowed_source_cidrs` in its `gcp {{}}` block. \
             Nothing was started"
        ))
    })?;
    let _ = writeln!(
        std::io::stderr(),
        "note: '{id}': the firewall rule will allow only {ip}/32 (this machine's public IP); \
         set `allowed_source_cidrs` to change it"
    );
    Ok(vec![format!("{ip}/32")])
}

fn handle(deployment: &DeploymentId, id: &str) -> RuntimeHandle {
    RuntimeHandle {
        id: id.to_owned(),
        deployment: deployment.clone(),
        endpoint: String::new(),
        exposure: EndpointExposure::Public {
            gateway: GatewayAuth::GceBearer,
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
) -> Result<(GceSupervisor, GceStart), CliError> {
    if RuntimeKind::of(&launch.settings) != RuntimeKind::Vllm {
        return Err(CliError::Message(format!(
            "deployment '{id}': Compute Engine deployments run vLLM only"
        )));
    }
    let ranges = source_ranges(graph, catalog, id, env).await?;
    let spec = launch_for(graph, catalog, id, &progress.project_id, ranges)?;
    let client = env.gcp.client(spec.impersonate.as_deref())?;
    let new_record = spec
        .record()
        .map_err(|e| CliError::Message(e.to_string()))?;
    let cert_pem = spec.cert_pem().map(str::to_owned);
    let probe = env
        .gcp
        .probe(&new_record.vm(&new_record.zone))
        .map_err(|e| CliError::Message(e.to_string()))?;
    let mut supervisor = GceSupervisor::new(client, spec)
        .with_probe_transport(probe)
        .with_timing(env.gcp.timing())
        .with_progress(env.progress);
    let prior = progress
        .stops
        .iter()
        .find(|r| {
            r.deployment == *id && r.backend == Backend::Gce && r.identity == new_record.instance
        })
        .and_then(|r| r.gcp.clone());
    if let Some(prior) = prior {
        supervisor = supervisor.with_recorded(prior);
    }
    let secrets = amicu_llamacpp::resolve_launch_secrets(launch, env.resolver).await?;
    supervisor
        .check_before_write(launch, &secrets)
        .map_err(supervisor_err)?;
    let start = GceStart {
        stop: StopRecord::gce(id, new_record),
        cert_pem,
        landed: supervisor.landed_zone(),
        created: supervisor.created(),
    };
    Ok((supervisor, start))
}

/// The write-ahead record, merged with the one already in state: the zones of
/// both stay listed, so an old VM is still found if this run stops halfway.
pub(super) fn write_ahead(stops: &mut Vec<StopRecord>, new: &StopRecord) {
    let Some(r) = stops.iter_mut().find(|r| r.same_target(new)) else {
        stops.push(new.clone());
        return;
    };
    let (Some(old), Some(fresh)) = (r.gcp.as_ref(), new.gcp.as_ref()) else {
        r.gcp.clone_from(&new.gcp);
        return;
    };
    let mut merged = fresh.clone();
    for z in old.zones() {
        if !merged.zones().contains(&z) {
            merged.alternates.push(z.to_owned());
        }
    }
    r.gcp = Some(merged);
}

/// Records the zone the VM landed in (when known) in the matching record.
pub(super) fn sync_record(stops: &mut [StopRecord], start: &GceStart) {
    let Some(zone) = start.landed.lock().ok().and_then(|z| z.clone()) else {
        return;
    };
    if let Some(r) = stops.iter_mut().find(|r| r.same_target(&start.stop))
        && let Some(g) = &r.gcp
    {
        r.gcp = Some(g.landed(&zone));
    }
}

/// After a failed start or exposure check: record where the VM landed, and
/// delete what this run created (nothing was serving). Returns a note for the
/// error. The record stays until a later apply or destroy confirms the stop.
pub(super) async fn failed_start(
    start: &GceStart,
    adapter: &dyn RuntimeAdapter,
    stops: &mut [StopRecord],
) -> String {
    sync_record(stops, start);
    if !start.created.load(Ordering::SeqCst) {
        return String::new();
    }
    let id = start.stop.identity.clone();
    match adapter.stop(&handle(&start.stop.deployment, &id)).await {
        Ok(()) => format!(
            "; VM {id} and its firewall rule, created by this run, were deleted (the record \
             stays until a later apply or destroy confirms it)"
        ),
        Err(e) => format!(
            "; deleting VM {id} and its firewall rule, created by this run, FAILED ({e}); they \
             stay recorded and `amicu destroy --target {}` retries",
            start.stop.deployment
        ),
    }
}

/// Writes the VM's certificate next to state, for clients to pin. A failure
/// is a note, not an error: the VM is serving and the fingerprint is in state.
pub(super) fn write_cert(project: &crate::project::Project, start: &GceStart) {
    let (Some(pem), Some(g)) = (&start.cert_pem, &start.stop.gcp) else {
        return;
    };
    if let Err(e) = amicu_gcp::compute::write_cert_pem(&project.state_dir(), &g.instance, pem) {
        let _ = writeln!(
            std::io::stderr(),
            "note: could not write the certificate of {} next to state ({e}); clients can pin \
             its fingerprint instead",
            g.instance
        );
    }
}

/// What a client of each applied Compute Engine VM pins: the VM's id, the
/// certificate file next to state and the certificate's fingerprint.
pub(super) fn tls_notes(
    project: &crate::project::Project,
    stops: &[StopRecord],
) -> Vec<(String, std::path::PathBuf, String)> {
    stops
        .iter()
        .filter_map(|r| r.gcp.as_ref())
        .filter_map(|g| {
            let f = g.tls_fingerprint.clone()?;
            let path = amicu_gcp::compute::cert_path(&project.state_dir(), &g.instance);
            Some((g.instance.clone(), path, f))
        })
        .collect()
}

/// Deletes the VM, then the firewall rule, then the deployment's secrets, each
/// only if it carries the record's owner marker; `Ok` only once all are
/// confirmed gone.
pub(super) async fn stop(record: &StopRecord, env: &Env<'_>) -> Result<(), CliError> {
    let Some(g) = &record.gcp else {
        return Err(CliError::Message(format!(
            "{} has no recorded project or zone, so amicu cannot find it; check the Google Cloud \
             console for VM '{}'. It was kept in state",
            record.describe(),
            record.identity
        )));
    };
    let client = env.gcp.client(g.impersonate.as_deref())?;
    teardown(&client, g, env.gcp.timing().poll)
        .await
        .map(|_| ())
        .map_err(|e| {
            CliError::Message(format!(
                "{}: {e}; it was kept in state, and nothing owned by someone else was touched",
                record.describe()
            ))
        })
}
