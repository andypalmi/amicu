//! A vLLM deployment on one Compute Engine VM: the record `apply` persists
//! before anything is created ([`GceRecord`]), the supervisor `apply` drives
//! ([`GceSupervisor`]), and the teardown `destroy` runs ([`teardown`]).
//!
//! The record names the project, instance, firewall rule, owner marker and
//! the zones that may hold the VM: the first is where the first create goes,
//! the rest are the fallbacks tried on a stockout. The zone that actually
//! took the VM replaces the list once known. Teardown checks every listed
//! zone, so a crash between the record and the create loses nothing.
//!
//! Serving is plain HTTP on the VM's external IP until TLS lands; the firewall
//! rule is the only network guard besides the bearer key.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use amicu_core::secret::SecretValue;
use amicu_llamacpp::remote::{self, HF_TOKEN_ENV_VAR, RemoteEnv};
use amicu_llamacpp::{LlamaCppError, ProcessSupervisor, ResolvedSecret};
use amicu_providers::{HttpMethod, HttpTransport, ReqwestTransport};
use amicu_runtime::exposure::validate_api_key_value;
use amicu_runtime::{
    EndpointExposure, GatewayAuth, LaunchPlan, LogOptions, LogStream, RuntimeHandle, RuntimeKind,
    RuntimeMetrics, RuntimeStatus,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::client::{COMPUTE_BASE_URL, GcpClient};
use crate::compute::{
    DEFAULT_IMAGE_FAMILY, DeleteOutcome, FirewallSpec, HealthTiming, StartupConfig, VmRecord,
    VmSpec, VmState, create, create_firewall, delete, delete_firewall, firewall_name,
    instance_name, prepare, status, wait_ready,
};
use crate::error::GcpError;
use crate::labels::{owner_labels, owner_marker};
use crate::operations::PollTiming;
use crate::placement::Shape;

/// Zones tried for one create before giving up.
pub const MAX_ZONE_ATTEMPTS: usize = 4;
/// How long a flex-start request waits for capacity.
const FLEX_WAIT: Duration = Duration::from_hours(1);
/// The image override, for the vLLM container.
pub const VLLM_IMAGE_ENV_VAR: &str = "AMICU_GCP_VLLM_IMAGE";

/// What amicu persists for one deployment's VM and firewall rule. Free of
/// secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GceRecord {
    pub project: String,
    /// Where the VM is, or the first zone a create goes to.
    pub zone: String,
    /// Other zones the VM may be in (a create that was not confirmed yet).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternates: Vec<String>,
    pub instance: String,
    pub firewall: String,
    /// The owner marker both resources carry.
    pub owner: String,
    /// The service account amicu acted as (for destroy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impersonate: Option<String>,
}

impl GceRecord {
    /// Every zone the VM may be in, most likely first.
    #[must_use]
    pub fn zones(&self) -> Vec<&str> {
        std::iter::once(self.zone.as_str())
            .chain(self.alternates.iter().map(String::as_str))
            .collect()
    }

    fn vm(&self, zone: &str) -> VmRecord {
        VmRecord {
            project: self.project.clone(),
            zone: zone.to_owned(),
            name: self.instance.clone(),
            owner: self.owner.clone(),
        }
    }

    /// The record once the VM is known to be in `zone`.
    #[must_use]
    pub fn landed(&self, zone: &str) -> Self {
        Self {
            zone: zone.to_owned(),
            alternates: Vec::new(),
            ..self.clone()
        }
    }
}

/// Everything the VM is configured from, resolved by the caller.
#[derive(Debug, Clone)]
pub struct GceLaunch {
    pub project: String,
    /// amicu's project and deployment ids (names and owner marker).
    pub project_id: String,
    pub deployment_id: String,
    /// Zones to try in order (at most [`MAX_ZONE_ATTEMPTS`] are used).
    pub zones: Vec<String>,
    pub shape: Shape,
    pub provisioning: crate::compute::Provisioning,
    pub max_run: Duration,
    pub boot_disk_gb: u32,
    pub service_account: Option<String>,
    pub source_ranges: Vec<String>,
    pub impersonate: Option<String>,
}

impl GceLaunch {
    fn owner(&self) -> String {
        owner_marker(None, &self.project_id, &self.deployment_id)
    }

    fn instance(&self) -> Result<String, GcpError> {
        instance_name(&self.project_id, &self.deployment_id, &self.owner()[..8])
    }

    /// The write-ahead record, before any request: no backend contact.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] for a bad name, project or zone.
    pub fn record(&self) -> Result<GceRecord, GcpError> {
        let mut zones = self.zones.iter().take(MAX_ZONE_ATTEMPTS);
        let zone = zones.next().ok_or(GcpError::Invalid {
            kind: "zone",
            value: String::new(),
            reason: "no zone to place the VM in",
        })?;
        let instance = self.instance()?;
        Ok(GceRecord {
            project: self.project.clone(),
            zone: zone.clone(),
            alternates: zones.cloned().collect(),
            firewall: firewall_name(&instance)?,
            instance,
            owner: self.owner(),
            impersonate: self.impersonate.clone(),
        })
    }
}

/// Waits and polls.
#[derive(Debug, Clone, Copy, Default)]
pub struct GceTiming {
    pub poll: PollTiming,
    pub health: HealthTiming,
}

type Sink = Arc<dyn Fn(&str) + Send + Sync>;

/// Starts, inspects and deletes one deployment's VM. See the module docs.
pub struct GceSupervisor {
    client: Arc<GcpClient>,
    probe: Arc<dyn HttpTransport>,
    launch: GceLaunch,
    timing: GceTiming,
    recorded: Option<GceRecord>,
    progress: Option<Sink>,
    landed: Arc<Mutex<Option<String>>>,
    created: Arc<AtomicBool>,
}

impl fmt::Debug for GceSupervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GceSupervisor")
            .field("launch", &self.launch)
            .finish_non_exhaustive()
    }
}

fn err(e: &GcpError) -> LlamaCppError {
    let hint = match e {
        GcpError::QuotaExceeded { .. } => {
            "; raise the GPU quota for the region in the Google Cloud console, or choose \
             another region or GPU"
        }
        GcpError::NotOwned { .. } => "; amicu leaves resources it did not create alone",
        _ => "",
    };
    LlamaCppError::Supervisor(format!("{e}{hint}"))
}

fn unsupported(what: &str) -> LlamaCppError {
    LlamaCppError::Supervisor(format!(
        "{what} are not yet supported for Compute Engine deployments"
    ))
}

impl GceSupervisor {
    #[must_use]
    pub fn new(client: GcpClient, launch: GceLaunch) -> Self {
        Self {
            client: Arc::new(client),
            probe: Arc::new(ReqwestTransport::new()),
            launch,
            timing: GceTiming::default(),
            recorded: None,
            progress: None,
            landed: Arc::default(),
            created: Arc::default(),
        }
    }

    /// Probe the VM through `transport` instead (tests inject a mock).
    #[must_use]
    pub fn with_probe_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.probe = transport;
        self
    }

    #[must_use]
    pub fn with_timing(mut self, timing: GceTiming) -> Self {
        self.timing = timing;
        self
    }

    #[must_use]
    pub fn with_progress(mut self, sink: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.progress = Some(Arc::new(sink));
        self
    }

    /// The record already in state: `start` deletes that VM first, since a
    /// VM's shape cannot be changed in place.
    #[must_use]
    pub fn with_recorded(mut self, record: GceRecord) -> Self {
        self.recorded = Some(record);
        self
    }

    /// The zone the VM is in, once a create succeeded.
    #[must_use]
    pub fn landed_zone(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.landed)
    }

    /// Set once `start` reached its first create (this run may own a VM).
    #[must_use]
    pub fn created(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.created)
    }

    /// What the record is now: the landed zone, else the write-ahead list.
    ///
    /// # Errors
    /// See [`GceLaunch::record`].
    pub fn record(&self) -> Result<GceRecord, GcpError> {
        let base = self.launch.record()?;
        Ok(match self.landed.lock().ok().and_then(|z| z.clone()) {
            Some(zone) => base.landed(&zone),
            None => base,
        })
    }

    fn say(&self, line: &str) {
        if let Some(p) = &self.progress {
            p(line);
        }
    }

    /// The container argument list, the key and the token for `plan`.
    fn prepare_startup(
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<StartupConfig, LlamaCppError> {
        if RuntimeKind::of(&plan.settings) != RuntimeKind::Vllm {
            return Err(LlamaCppError::Supervisor(
                "Compute Engine deployments run vLLM only".to_owned(),
            ));
        }
        let amicu_core::resource::Runtime::Vllm(settings) = &plan.settings else {
            unreachable!("checked above");
        };
        let port = settings.port;
        let mut rendered = remote::launch_argv(plan, None, "gcp")?;
        // The image's entrypoint is `vllm serve`; the startup script adds the
        // host, port and middleware itself.
        if rendered.starts_with(&["vllm".to_owned(), "serve".to_owned()]) {
            rendered.drain(..2);
        }
        let mut args = Vec::new();
        let mut it = rendered.into_iter();
        while let Some(a) = it.next() {
            if a == "--host" || a == "--port" {
                it.next();
            } else {
                args.push(a);
            }
        }
        let hf_fallback = std::env::var(HF_TOKEN_ENV_VAR).ok();
        let RemoteEnv { plain, mut secret } =
            remote::split_env(plan, secrets, hf_fallback.as_deref())?;
        if !plain.is_empty() {
            return Err(LlamaCppError::Supervisor(
                "the `env` block is not supported on Compute Engine yet: it would be readable \
                 in the instance metadata"
                    .to_owned(),
            ));
        }
        let key_var = RuntimeKind::Vllm.api_key_env_var();
        let api_key = secret
            .remove(key_var)
            .map(SecretValue::new)
            .ok_or_else(|| {
                LlamaCppError::Supervisor(
                    "Compute Engine deployments need an `api_key`: it is the only guard on the \
                 serving port besides the firewall"
                        .to_owned(),
                )
            })?;
        validate_api_key_value(&api_key)
            .map_err(|why| LlamaCppError::Supervisor(format!("the deployment's api_key {why}")))?;
        let hf_token = secret.remove(HF_TOKEN_ENV_VAR).map(SecretValue::new);
        if let Some(name) = secret.keys().next() {
            return Err(LlamaCppError::Supervisor(format!(
                "secret '{name}' cannot be delivered to a Compute Engine VM yet; only the \
                 `api_key` and `HF_TOKEN` are"
            )));
        }
        Ok(StartupConfig {
            container_image: std::env::var(VLLM_IMAGE_ENV_VAR)
                .unwrap_or_else(|_| remote::default_image(RuntimeKind::Vllm).to_owned()),
            vllm_args: args,
            serving_port: port,
            api_key,
            hf_token,
        })
    }

    fn spec(&self, zone: &str, record: &GceRecord, startup: &StartupConfig) -> VmSpec {
        let l = &self.launch;
        VmSpec {
            project: l.project.clone(),
            zone: zone.to_owned(),
            name: record.instance.clone(),
            machine_type: l.shape.machine_type.clone(),
            accelerator: l.shape.accelerator.clone(),
            provisioning: match l.provisioning {
                crate::compute::Provisioning::FlexStart { .. } => {
                    crate::compute::Provisioning::FlexStart { wait: FLEX_WAIT }
                }
                p => p,
            },
            max_run: l.max_run,
            boot_disk_gb: l.boot_disk_gb,
            image_family: DEFAULT_IMAGE_FAMILY.to_owned(),
            install_nvidia_driver: false,
            service_account: l.service_account.clone(),
            network_tag: record.instance.clone(),
            labels: owner_labels(None, &l.project_id, &l.deployment_id),
            startup: startup.clone(),
        }
    }

    /// Plans and checks everything `start` checks before its first write, so
    /// a bad launch fails before the record is written.
    ///
    /// # Errors
    /// Whatever `start` would refuse before writing.
    pub fn check_before_write(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<(), LlamaCppError> {
        let startup = Self::prepare_startup(plan, secrets)?;
        let record = self.launch.record().map_err(|e| err(&e))?;
        self.spec(&record.zone, &record, &startup)
            .validate()
            .map_err(|e| err(&e))
    }
}

#[async_trait]
impl ProcessSupervisor for GceSupervisor {
    async fn start(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        let startup = Self::prepare_startup(plan, secrets)?;
        let port = startup.serving_port;
        let record = self.launch.record().map_err(|e| err(&e))?;
        // Validate the first spec before touching anything.
        self.spec(&record.zone, &record, &startup)
            .validate()
            .map_err(|e| err(&e))?;

        if let Some(old) = &self.recorded {
            self.say(&format!("removing the previous VM {}", old.instance));
            teardown(&self.client, old, self.timing.poll)
                .await
                .map_err(|e| err(&e))?;
        }

        self.created.store(true, Ordering::SeqCst);
        self.say(&format!("creating firewall rule {}", record.firewall));
        create_firewall(
            &self.client,
            &FirewallSpec {
                project: self.launch.project.clone(),
                name: record.firewall.clone(),
                port,
                source_ranges: self.launch.source_ranges.clone(),
                network_tag: record.instance.clone(),
                owner: record.owner.clone(),
            },
            self.timing.poll,
        )
        .await
        .map_err(|e| err(&e))?;

        let mut tried = Vec::new();
        let mut last_stockout = String::new();
        let mut landed = None;
        for zone in record.zones() {
            let spec = self.spec(zone, &record, &startup);
            let vm = prepare(&spec).map_err(|e| err(&e))?;
            self.say(&format!("creating VM {} in {zone}", vm.name));
            match create(&self.client, &spec, &vm, self.timing.poll).await {
                Ok(_) => {
                    landed = Some(zone.to_owned());
                    break;
                }
                Err(e) if e.is_stockout() => {
                    self.say(&format!("{zone} has no capacity, trying the next zone"));
                    tried.push(zone.to_owned());
                    last_stockout = e.to_string();
                }
                Err(e) => return Err(err(&e)),
            }
        }
        let Some(zone) = landed else {
            return Err(LlamaCppError::Supervisor(format!(
                "no capacity in {}: {last_stockout}; try another `region`, `zone`, GPU or \
                 `provisioning`",
                tried.join(", ")
            )));
        };
        if let Ok(mut slot) = self.landed.lock() {
            *slot = Some(zone.clone());
        }
        self.say(&format!(
            "VM created in {zone}; waiting for vLLM (plain HTTP on port {port})"
        ));
        let vm = record.landed(&zone).vm(&zone);
        let ip = wait_ready(
            &self.client,
            Arc::clone(&self.probe),
            &vm,
            port,
            &startup.api_key,
            self.timing.health,
        )
        .await
        .map_err(|e| err(&e))?;
        Ok(RuntimeHandle {
            id: record.instance.clone(),
            deployment: plan.deployment.clone(),
            endpoint: format!("http://{ip}:{port}"),
            exposure: EndpointExposure::Public {
                gateway: GatewayAuth::GceBearer,
            },
        })
    }

    async fn status(&self, _handle: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError> {
        let record = self.record().map_err(|e| err(&e))?;
        let (zone, state) = vm_state(&self.client, &record).await.map_err(|e| err(&e))?;
        Ok(match state {
            VmState::Running { .. } => RuntimeStatus::Running {
                detail: format!("VM running in {zone}"),
            },
            VmState::Provisioning | VmState::Staging | VmState::Other(_) => {
                RuntimeStatus::Running {
                    detail: format!("VM starting in {zone}"),
                }
            }
            VmState::Stopping => RuntimeStatus::Running {
                detail: format!("VM stopping in {zone}"),
            },
            VmState::Gone => RuntimeStatus::Stopped {
                detail: GONE.to_owned(),
            },
            VmState::Terminated { preempted } => RuntimeStatus::Stopped {
                detail: if preempted {
                    "VM was preempted".to_owned()
                } else {
                    "VM is stopped".to_owned()
                },
            },
            VmState::Suspended => RuntimeStatus::Stopped {
                detail: "VM is suspended".to_owned(),
            },
        })
    }

    async fn metrics(&self, _handle: &RuntimeHandle) -> Result<RuntimeMetrics, LlamaCppError> {
        Err(unsupported("metrics"))
    }

    async fn logs(
        &self,
        _handle: &RuntimeHandle,
        _options: LogOptions,
    ) -> Result<LogStream, LlamaCppError> {
        Err(unsupported("logs"))
    }

    async fn stop(&self, _handle: &RuntimeHandle) -> Result<(), LlamaCppError> {
        let record = self.record().map_err(|e| err(&e))?;
        teardown(&self.client, &record, self.timing.poll)
            .await
            .map(|_| ())
            .map_err(|e| err(&e))
    }
}

/// What a missing VM most likely means.
pub const GONE: &str =
    "the VM is gone: preempted (Spot), past its max_run_duration, or deleted outside amicu";

/// Deletes the VM (in every zone it may be in) and then the firewall rule,
/// each only when it carries the record's owner marker, and confirms both are
/// gone. [`DeleteOutcome::AlreadyGone`] when the VM was absent everywhere.
///
/// # Errors
/// [`GcpError::NotOwned`] (nothing deleted) for a VM or rule of that name
/// that is not ours, or an API error. The firewall rule is left in place when
/// the VM could not be removed.
pub async fn teardown(
    client: &GcpClient,
    record: &GceRecord,
    timing: PollTiming,
) -> Result<DeleteOutcome, GcpError> {
    let mut outcome = DeleteOutcome::AlreadyGone;
    for zone in record.zones() {
        if delete(client, &record.vm(zone), timing).await? == DeleteOutcome::Deleted {
            outcome = DeleteOutcome::Deleted;
        }
    }
    delete_firewall(
        client,
        &record.project,
        &record.firewall,
        &record.owner,
        timing,
    )
    .await?;
    Ok(outcome)
}

/// The VM's state and the zone it is in; [`VmState::Gone`] in the first
/// listed zone when it is in none.
///
/// # Errors
/// [`GcpError::NotOwned`] for a VM of that name that is not ours, or an API error.
pub async fn vm_state(
    client: &GcpClient,
    record: &GceRecord,
) -> Result<(String, VmState), GcpError> {
    for zone in record.zones() {
        match status(client, &record.vm(zone)).await? {
            VmState::Gone => {}
            state => return Ok((zone.to_owned(), state)),
        }
    }
    Ok((record.zone.clone(), VmState::Gone))
}

/// An amicu-labelled VM found in a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedVm {
    pub zone: String,
    pub name: String,
    pub status: String,
    pub owner: String,
}

/// The VMs of `project` that carry an owner label (read-only).
///
/// # Errors
/// An API error.
pub async fn list_owned_instances(
    client: &GcpClient,
    project: &str,
) -> Result<Vec<OwnedVm>, GcpError> {
    crate::validate::validate_project_id(project)?;
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..50 {
        let mut url = format!(
            "{COMPUTE_BASE_URL}/projects/{project}/aggregated/instances?filter=labels.{}%3A*",
            crate::labels::OWNER_LABEL
        );
        if let Some(t) = &token {
            url = format!("{url}&pageToken={}", pct(t));
        }
        let page = client.request(HttpMethod::Get, &url, None).await?;
        if let Some(scopes) = page["items"].as_object() {
            for (scope, v) in scopes {
                let zone = scope.rsplit('/').next().unwrap_or_default();
                for i in v["instances"].as_array().into_iter().flatten() {
                    let owner = i["labels"][crate::labels::OWNER_LABEL].as_str();
                    if let (Some(name), Some(owner)) = (i["name"].as_str(), owner) {
                        out.push(OwnedVm {
                            zone: zone.to_owned(),
                            name: name.to_owned(),
                            status: i["status"].as_str().unwrap_or("UNKNOWN").to_owned(),
                            owner: owner.to_owned(),
                        });
                    }
                }
            }
        }
        token = page["nextPageToken"].as_str().map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    Ok(out)
}

/// The names and owner markers of `project`'s firewall rules that carry amicu's
/// description marker (read-only).
///
/// # Errors
/// An API error.
pub async fn list_owned_firewalls(
    client: &GcpClient,
    project: &str,
) -> Result<Vec<(String, String)>, GcpError> {
    crate::validate::validate_project_id(project)?;
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..50 {
        let mut url = format!("{COMPUTE_BASE_URL}/projects/{project}/global/firewalls");
        if let Some(t) = &token {
            url = format!("{url}?pageToken={}", pct(t));
        }
        let page = client.request(HttpMethod::Get, &url, None).await?;
        for r in page["items"].as_array().into_iter().flatten() {
            let owner = r["description"]
                .as_str()
                .and_then(|d| d.strip_prefix("amicu-owner="))
                .map(|d| d.split(';').next().unwrap_or_default().to_owned());
            if let (Some(name), Some(owner)) = (r["name"].as_str(), owner) {
                out.push((name.to_owned(), owner));
            }
        }
        token = page["nextPageToken"].as_str().map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    Ok(out)
}

/// Percent-encodes a page token for a query string.
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::Provisioning;

    fn launch() -> GceLaunch {
        GceLaunch {
            project: "my-proj".into(),
            project_id: "aaaa1111".into(),
            deployment_id: "chat".into(),
            zones: ["europe-west4-a", "europe-west4-b", "europe-west4-c"]
                .map(String::from)
                .to_vec(),
            shape: crate::resolve_shape("t4", 1, None, crate::CatalogProvisioning::Spot).unwrap(),
            provisioning: Provisioning::Spot,
            max_run: Duration::from_hours(2),
            boot_disk_gb: 100,
            service_account: None,
            source_ranges: vec!["203.0.113.4/32".into()],
            impersonate: None,
        }
    }

    #[test]
    fn the_write_ahead_record_names_everything_and_all_zones() {
        let r = launch().record().unwrap();
        assert_eq!(r.zone, "europe-west4-a");
        assert_eq!(
            r.zones(),
            ["europe-west4-a", "europe-west4-b", "europe-west4-c"]
        );
        assert!(
            r.instance.starts_with("amicu-aaaa1111-chat"),
            "{}",
            r.instance
        );
        assert_eq!(r.firewall, format!("{}-fw", r.instance));
        assert_eq!(r.owner, owner_marker(None, "aaaa1111", "chat"));
        assert_eq!(launch().record().unwrap(), r, "deterministic");
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<GceRecord>(&json).unwrap(), r);
        let landed = r.landed("europe-west4-b");
        assert_eq!(landed.zones(), ["europe-west4-b"]);
    }

    #[test]
    fn only_a_bounded_number_of_zones_are_recorded() {
        let mut l = launch();
        l.zones = (b'a'..=b'h')
            .map(|c| format!("europe-west4-{}", c as char))
            .collect();
        assert_eq!(l.record().unwrap().zones().len(), MAX_ZONE_ATTEMPTS);
    }
}
