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
use amicu_gcp::{GceLaunch, GceRecord, GceSupervisor, GcpCatalog, candidate_zones, resolve_shape};
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
    secrets_touched: Arc<AtomicBool>,
    /// No record of this deployment existed before this run.
    fresh: bool,
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

/// The service account the VM runs as, for the preflight.
pub(super) fn service_account_for(
    graph: &Graph,
    catalog: &dyn Catalog,
    id: &DeploymentId,
) -> Result<Option<String>, CliError> {
    let (_, hardware) = deployment_hardware(graph, catalog, id)?;
    Ok(hardware.gcp.unwrap_or_default().service_account)
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

pub(super) fn residency_ok(region: &str, allowed: &[Residency]) -> bool {
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
            "deployment '{id}': `backend = \"cloud_run\"` deploys a Cloud Run service, not a VM"
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
        idle_shutdown: s
            .idle_shutdown_secs
            .map(|s| Duration::from_secs(u64::from(s))),
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
    let found = progress.stops.iter().find(|r| {
        r.deployment == *id && r.backend == Backend::Gce && r.identity == new_record.instance
    });
    let fresh = !progress
        .stops
        .iter()
        .any(|r| r.backend == Backend::Gce && r.identity == new_record.instance);
    let prior = found.and_then(|r| r.gcp.clone());
    // A record made for another project or impersonation target is not torn
    // down by the supervisor (it would act as the new identity): apply stops
    // it first and confirms, see `moved_records`.
    if let Some(prior) = prior
        && !moved(&prior, &new_record)
    {
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
        secrets_touched: supervisor.secrets_touched(),
        fresh,
    };
    Ok((supervisor, start))
}

/// Whether `old` was made for another Google project or impersonation target
/// than `new`: the same instance name (it does not depend on the project), but
/// resources that the new identity cannot reach.
fn moved(old: &GceRecord, new: &GceRecord) -> bool {
    old.project != new.project || old.impersonate != new.impersonate
}

/// The recorded VMs of this deployment that `new` would replace but that live
/// under another project or impersonation target. Apply stops them, and
/// confirms, before the write-ahead record overwrites them: the instance name
/// is the same, so the new record would otherwise take their place in state
/// and the old VM, firewall rule and secrets would no longer be findable.
pub(super) fn moved_records(stops: &[StopRecord], new: &StopRecord) -> Vec<StopRecord> {
    let Some(fresh) = new.gcp.as_ref() else {
        return Vec::new();
    };
    stops
        .iter()
        .filter(|r| r.deployment == new.deployment && r.same_target(new))
        .filter(|r| r.gcp.as_ref().is_some_and(|old| moved(old, fresh)))
        .cloned()
        .collect()
}

/// The write-ahead record, merged with the one already in state: the zones and
/// secrets of both stay listed, so an old VM (and the secrets of an old
/// delivery) are still found if this run stops halfway.
///
/// # Errors
/// When the record in state belongs to another project or impersonation target
/// (see [`moved_records`], which apply stops first): it is never overwritten
/// while it may still own resources.
pub(super) fn write_ahead(stops: &mut Vec<StopRecord>, new: &StopRecord) -> Result<(), CliError> {
    let Some(r) = stops.iter_mut().find(|r| r.same_target(new)) else {
        stops.push(new.clone());
        return Ok(());
    };
    let (Some(old), Some(fresh)) = (r.gcp.as_ref(), new.gcp.as_ref()) else {
        r.gcp.clone_from(&new.gcp);
        return Ok(());
    };
    if moved(old, fresh) {
        return Err(CliError::Message(format!(
            "the recorded VM {} belongs to project {} and cannot be replaced by one in project \
             {} before it is stopped; nothing was changed",
            old.instance, old.project, fresh.project
        )));
    }
    let mut merged = fresh.clone();
    for z in old.zones() {
        if !merged.zones().contains(&z) {
            merged.alternates.push(z.to_owned());
        }
    }
    for id in &old.secrets {
        if !merged.secrets.contains(id) {
            merged.secrets.push(id.clone());
        }
    }
    r.gcp = Some(merged);
    Ok(())
}

/// Records the zone the VM landed in (when known) in the matching record, and
/// that no secret was ever written when this run proves it (the record is then
/// new, the secrets step was reached and nothing was written).
pub(super) fn sync_record(stops: &mut [StopRecord], start: &GceStart) {
    let landed = start.landed.lock().ok().and_then(|z| z.clone());
    let unwritten = start.fresh
        && start.created.load(Ordering::SeqCst)
        && !start.secrets_touched.load(Ordering::SeqCst);
    if landed.is_none() && !unwritten {
        return;
    }
    if let Some(r) = stops.iter_mut().find(|r| r.same_target(&start.stop))
        && let Some(g) = &r.gcp
    {
        let mut g = landed.map_or_else(|| g.clone(), |zone| g.landed(&zone));
        g.secrets_unwritten |= unwritten && !g.secrets.is_empty();
        r.gcp = Some(g);
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
        Err(e) => {
            let (project, recorded_zone) = start
                .stop
                .gcp
                .as_ref()
                .map(|g| (g.project.clone(), g.zone.clone()))
                .unwrap_or_default();
            let zone = start
                .landed
                .lock()
                .ok()
                .and_then(|z| z.clone())
                .unwrap_or(recorded_zone);
            format!(
                "; deleting VM {id} and its firewall rule, created by this run, FAILED ({e}); \
                 the VM may still be running and billing in {zone}. They stay recorded and \
                 `amicu destroy --target {}` retries. While the VM exists, its boot log: \
                 gcloud compute instances get-serial-port-output {id} --zone {zone} \
                 --project {project}",
                start.stop.deployment,
            )
        }
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

/// What a client of one applied Compute Engine VM pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TlsNote {
    pub instance: String,
    /// The certificate file next to state.
    pub path: std::path::PathBuf,
    /// SHA-256 of the certificate's DER, as recorded in state.
    pub fingerprint: String,
    /// The `curl --pinnedpubkey` value, from the certificate file when it is
    /// there and matches the recorded fingerprint.
    pub spki_pin: Option<String>,
}

/// What a client of each applied Compute Engine VM pins.
pub(super) fn tls_notes(project: &crate::project::Project, stops: &[StopRecord]) -> Vec<TlsNote> {
    stops
        .iter()
        .filter_map(|r| r.gcp.as_ref())
        .filter_map(|g| {
            let fingerprint = g.tls_fingerprint.clone()?;
            let path = amicu_gcp::compute::cert_path(&project.state_dir(), &g.instance);
            let spki_pin = std::fs::read_to_string(&path)
                .ok()
                .filter(|pem| {
                    amicu_gcp::compute::fingerprint_of_pem(pem).is_ok_and(|f| f == fingerprint)
                })
                .and_then(|pem| amicu_gcp::compute::spki_pin_of_pem(&pem).ok());
            Some(TlsNote {
                instance: g.instance.clone(),
                path,
                fingerprint,
                spki_pin,
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record(project: &str, zone: &str, secrets: &[&str]) -> StopRecord {
        StopRecord::gce(
            &"chat".into(),
            GceRecord {
                project: project.to_owned(),
                zone: zone.to_owned(),
                alternates: Vec::new(),
                instance: "amicu-p-chat-0123abcd".to_owned(),
                firewall: "amicu-p-chat-0123abcd-fw".to_owned(),
                owner: "owner".to_owned(),
                impersonate: None,
                secrets: secrets.iter().map(|s| (*s).to_owned()).collect(),
                tls_fingerprint: None,
                secrets_unwritten: false,
            },
        )
    }

    fn gcp(r: &StopRecord) -> &GceRecord {
        r.gcp.as_ref().unwrap()
    }

    #[test]
    fn write_ahead_unions_zones_and_secrets_of_the_same_project() {
        let mut stops = vec![record("p", "europe-west4-a", &["s-api-key", "s-tls-key"])];
        write_ahead(&mut stops, &record("p", "europe-west4-b", &[])).unwrap();
        assert_eq!(stops.len(), 1);
        let g = gcp(&stops[0]);
        assert_eq!(g.zones(), ["europe-west4-b", "europe-west4-a"]);
        // The metadata delivery lists no secrets, but the old ones stay.
        assert_eq!(g.secrets, ["s-api-key", "s-tls-key"]);

        // Repeating adds nothing twice.
        write_ahead(&mut stops, &record("p", "europe-west4-b", &["s-api-key"])).unwrap();
        let g = gcp(&stops[0]);
        assert_eq!(g.zones(), ["europe-west4-b", "europe-west4-a"]);
        assert_eq!(g.secrets, ["s-api-key", "s-tls-key"]);
    }

    #[test]
    fn write_ahead_pushes_a_new_record_and_refuses_to_overwrite_another_project() {
        let mut stops = Vec::new();
        write_ahead(&mut stops, &record("p", "europe-west4-a", &[])).unwrap();
        assert_eq!(stops.len(), 1);

        let before = stops.clone();
        let e = write_ahead(&mut stops, &record("q", "europe-west4-a", &[])).unwrap_err();
        assert!(e.to_string().contains("belongs to project p"), "{e}");
        assert_eq!(stops, before, "the old record is untouched");

        // Another impersonation target is another identity too.
        let mut other = record("p", "europe-west4-a", &[]);
        other.gcp.as_mut().unwrap().impersonate = Some("sa@p.iam.gserviceaccount.com".into());
        assert!(write_ahead(&mut stops, &other).is_err());
    }

    #[test]
    fn moved_records_are_the_same_instance_under_another_project_or_account() {
        let new = record("q", "europe-west4-a", &[]);
        let same = record("q", "europe-west4-b", &[]);
        let old = record("p", "europe-west4-a", &["s-api-key"]);
        let mut other_dep = old.clone();
        other_dep.deployment = "other".into();
        let stops = vec![same.clone(), old.clone(), other_dep];
        assert_eq!(moved_records(&stops, &new), vec![old.clone()]);
        assert_eq!(moved_records(&[same], &new).len(), 0);
        assert_eq!(moved_records(&[], &new).len(), 0);

        let mut impersonated = new.clone();
        impersonated.gcp.as_mut().unwrap().impersonate =
            Some("sa@q.iam.gserviceaccount.com".into());
        assert_eq!(moved_records(&[new], &impersonated).len(), 1);
    }
}
