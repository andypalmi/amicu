//! `provider = "gcp"` through apply, destroy, refresh and orphans, against an
//! in-memory Compute Engine API. No Google calls.

use std::sync::{Arc, Mutex};

use amicu_core::adapter::AdapterError;
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use amicu_gcp::compute::VmRecord;
use amicu_gcp::{AccessToken, GcpClient, GcpError, TokenSource};
use amicu_providers::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, TransportError};
use serde_json::{Value, json};

use super::*;
use crate::gcp_ops::GcpOps;

const BASE: &str = "https://compute.googleapis.com/compute/v1";
const SM_BASE: &str = "https://secretmanager.googleapis.com/v1";
const SERVICE_ACCOUNT: &str = "vm@my-proj.iam.gserviceaccount.com";
const PROJECT: &str = "aaaa1111";
const DEP: &str = "chat";
const GOOGLE_PROJECT: &str = "my-proj";
const IP: &str = "198.51.100.7";

#[derive(Clone)]
struct Vm {
    zone: String,
    name: String,
    status: &'static str,
    labels: Value,
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct WorldState {
    vms: Vec<Vm>,
    /// `(name, description)` of every firewall rule.
    firewalls: Vec<(String, String)>,
    /// Zones whose instance inserts fail with a stockout.
    stockout: Vec<String>,
    /// Operation names that finish with an error: `(op, code)`.
    failing_ops: Vec<(String, String)>,
    next_op: u32,
    /// `METHOD path` of every request, in order.
    log: Vec<String>,
    /// Body of every firewall create.
    firewall_bodies: Vec<Value>,
    /// Zone of every instance insert, in order.
    inserts: Vec<String>,
    /// Body of every instance insert.
    insert_bodies: Vec<Value>,
    /// `(id, labels, replication)` of every Secret Manager secret.
    secrets: Vec<(String, Value, Value)>,
    /// `(id, member)` of every accessor grant.
    grants: Vec<(String, String)>,
    /// The fingerprint (if any) of the transport asked for the VM.
    probe_pins: Vec<Option<String>>,
    /// The fingerprint (if any) of the record the exposure check ran against.
    verify_pins: Vec<Option<String>>,
    /// The project has no `default` network.
    no_default_network: bool,
    /// Secret Manager answers a plain 403 permission error.
    secretmanager_denied: bool,
    /// The caller lacks `iam.serviceAccounts.actAs`.
    act_as_denied: bool,
}

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct World {
    state: Mutex<WorldState>,
    /// Secret Manager answers 403 `SERVICE_DISABLED`.
    sm_disabled: Arc<std::sync::atomic::AtomicBool>,
    /// Fired once, just before the first instance insert.
    on_insert: Mutex<Option<Hook>>,
    /// Fired once, just before the first secret create.
    on_secret_create: Mutex<Option<Hook>>,
    /// Fired once, just before the first firewall create.
    on_firewall: Mutex<Option<Hook>>,
    /// The Cloud Run API and the services' own URLs.
    run: Mutex<cloudrun::RunWorld>,
}

#[allow(clippy::unnecessary_wraps)]
fn json_reply(status: u16, body: &Value) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse::new(status, body.to_string().into_bytes()))
}

fn sm_disabled_reply() -> Result<HttpResponse, TransportError> {
    json_reply(
        403,
        &json!({"error": {"code": 403, "status": "PERMISSION_DENIED", "message": "Secret Manager API has not been used in project 1 before or it is disabled.", "details": [{"reason": "SERVICE_DISABLED"}]}}),
    )
}

fn not_found() -> Result<HttpResponse, TransportError> {
    json_reply(
        404,
        &json!({"error": {"code": 404, "status": "NOT_FOUND", "message": "not found"}}),
    )
}

impl World {
    fn sm_is_disabled(&self) -> bool {
        self.sm_disabled.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn disable_secret_manager(&self) {
        self.sm_disabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The Secret Manager API: secrets, versions and per-secret IAM.
    fn secret_manager(
        &self,
        method: HttpMethod,
        path: &str,
        req: &HttpRequest,
    ) -> Result<HttpResponse, TransportError> {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let mut w = self.state.lock().unwrap();
        if self.sm_is_disabled() {
            return sm_disabled_reply();
        }
        if w.secretmanager_denied {
            return json_reply(
                403,
                &json!({"error": {"code": 403, "status": "PERMISSION_DENIED", "message": "Permission denied on resource"}}),
            );
        }
        let m = format!("{method:?}");
        match (method, parts.as_slice()) {
            (HttpMethod::Get, ["projects", _, "secrets"]) => {
                let items: Vec<Value> = w
                    .secrets
                    .iter()
                    .map(|(id, labels, _)| {
                        json!({"name": format!("projects/1/secrets/{id}"), "labels": labels})
                    })
                    .collect();
                json_reply(200, &json!({"secrets": items}))
            }
            (HttpMethod::Post, ["projects", _, "secrets"]) => {
                if let Some(hook) = self.on_secret_create.lock().unwrap().take() {
                    hook();
                }
                if self.sm_is_disabled() {
                    return sm_disabled_reply();
                }
                let id = query.strip_prefix("secretId=").unwrap_or_default();
                if w.secrets.iter().any(|s| s.0 == id) {
                    return json_reply(
                        409,
                        &json!({"error": {"code": 409, "status": "ALREADY_EXISTS", "message": "exists"}}),
                    );
                }
                let body = req.body.clone().unwrap_or_default();
                w.log.push(format!("secret-create {id}"));
                w.secrets.push((
                    id.to_owned(),
                    body["labels"].clone(),
                    body["replication"].clone(),
                ));
                json_reply(200, &json!({}))
            }
            (_, ["projects", _, "secrets", last]) => {
                let (id, action) = last.split_once(':').unwrap_or((last, ""));
                let exists = w.secrets.iter().any(|s| s.0 == id);
                if !exists {
                    return not_found();
                }
                match (method, action) {
                    (HttpMethod::Get, "") => {
                        let found = w.secrets.iter().find(|s| s.0 == id).unwrap();
                        json_reply(
                            200,
                            &json!({"name": id, "labels": found.1, "replication": found.2}),
                        )
                    }
                    (HttpMethod::Post, "addVersion") => {
                        w.log.push(format!("secret-version {id}"));
                        json_reply(200, &json!({}))
                    }
                    (HttpMethod::Get, "getIamPolicy") => json_reply(200, &json!({"etag": "e"})),
                    (HttpMethod::Post, "setIamPolicy") => {
                        let body = req.body.clone().unwrap_or_default();
                        for b in body["policy"]["bindings"].as_array().into_iter().flatten() {
                            assert_eq!(b["role"], "roles/secretmanager.secretAccessor");
                            for member in b["members"].as_array().into_iter().flatten() {
                                let member = member.as_str().unwrap_or_default().to_owned();
                                w.grants.push((id.to_owned(), member));
                            }
                        }
                        json_reply(200, &json!({}))
                    }
                    (HttpMethod::Delete, "") => {
                        w.log.push(format!("secret-delete {id}"));
                        w.secrets.retain(|s| s.0 != id);
                        json_reply(200, &json!({}))
                    }
                    _ => json_reply(
                        400,
                        &json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": format!("unexpected {m} {path}")}}),
                    ),
                }
            }
            _ => json_reply(
                400,
                &json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": format!("unexpected {m} {path}")}}),
            ),
        }
    }

    fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }
    fn writes(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|l| !l.starts_with("Get") && !l.starts_with("probe"))
            .collect()
    }
    fn vm_names(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .vms
            .iter()
            .map(|v| v.name.clone())
            .collect()
    }
    fn vm_zone(&self, name: &str) -> Option<String> {
        let w = self.state.lock().unwrap();
        w.vms
            .iter()
            .find(|v| v.name == name)
            .map(|v| v.zone.clone())
    }
    fn firewall_names(&self) -> Vec<String> {
        let w = self.state.lock().unwrap();
        w.firewalls.iter().map(|f| f.0.clone()).collect()
    }
    fn secret_ids(&self) -> Vec<String> {
        let w = self.state.lock().unwrap();
        w.secrets.iter().map(|s| s.0.clone()).collect()
    }
    fn add_secret(&self, id: &str, labels: Value) {
        self.state
            .lock()
            .unwrap()
            .secrets
            .push((id.to_owned(), labels, json!({})));
    }
    fn position(&self, prefix: &str) -> Option<usize> {
        self.log().iter().position(|l| l.starts_with(prefix))
    }
    fn add_vm(&self, zone: &str, name: &str, labels: Value) {
        self.state.lock().unwrap().vms.push(Vm {
            zone: zone.to_owned(),
            name: name.to_owned(),
            status: "RUNNING",
            labels,
        });
    }
    fn remove_vm(&self, name: &str) {
        self.state.lock().unwrap().vms.retain(|v| v.name != name);
    }
}

fn vm_json(v: &Vm) -> Value {
    json!({
        "name": v.name,
        "status": v.status,
        "labels": v.labels,
        "scheduling": {"provisioningModel": "SPOT"},
        "networkInterfaces": [{"accessConfigs": [{"natIP": IP}]}],
    })
}

fn op(w: &mut WorldState, prefix: &str) -> Value {
    w.next_op += 1;
    json!({"name": format!("{prefix}-{}", w.next_op), "status": "RUNNING"})
}

#[async_trait::async_trait]
impl HttpTransport for World {
    #[allow(clippy::too_many_lines)]
    async fn execute(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = req.method;
        let m = format!("{method:?}");
        if let Some(path) = req.url.strip_prefix(SM_BASE) {
            return self.secret_manager(method, path, &req);
        }
        if let Some(reply) = self.cloud_run(&req) {
            return reply;
        }
        if req.url.starts_with("https://iam.googleapis.com/") {
            let denied = self.state.lock().unwrap().act_as_denied;
            let granted: Vec<&str> = if denied {
                Vec::new()
            } else {
                vec!["iam.serviceAccounts.actAs"]
            };
            return json_reply(200, &json!({ "permissions": granted }));
        }
        let Some(path) = req.url.strip_prefix(BASE) else {
            // The readiness probe to the VM itself.
            self.state
                .lock()
                .unwrap()
                .log
                .push(format!("probe {} {}", m, req.url));
            return Ok(HttpResponse::new(200, "ok"));
        };
        let path = path.split('?').next().unwrap_or_default().to_owned();
        self.state.lock().unwrap().log.push(format!("{m} {path}"));
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            (HttpMethod::Get, ["projects", _]) => json_reply(200, &json!({})),
            (HttpMethod::Get, ["projects", _, "global", "networks", "default"]) => {
                if self.state.lock().unwrap().no_default_network {
                    not_found()
                } else {
                    json_reply(200, &json!({"name": "default"}))
                }
            }
            (
                _,
                ["projects", _, "global", "operations", name, "wait"]
                | ["projects", _, "zones", _, "operations", name, "wait"],
            ) => {
                let w = self.state.lock().unwrap();
                let mut body = json!({"name": name, "status": "DONE"});
                if let Some((_, code)) = w.failing_ops.iter().find(|(n, _)| n == name) {
                    body["error"] = json!({"errors": [{"code": code, "message": format!("{code}: no capacity")}]});
                }
                json_reply(200, &body)
            }
            (HttpMethod::Post, ["projects", _, "global", "firewalls"]) => {
                if let Some(hook) = self.on_firewall.lock().unwrap().take() {
                    hook();
                }
                let body = req.body.clone().unwrap_or_default();
                let name = body["name"].as_str().unwrap_or_default().to_owned();
                let mut w = self.state.lock().unwrap();
                if w.firewalls.iter().any(|f| f.0 == name) {
                    return json_reply(
                        409,
                        &json!({"error": {"code": 409, "status": "ALREADY_EXISTS", "message": "exists"}}),
                    );
                }
                let desc = body["description"].as_str().unwrap_or_default().to_owned();
                w.firewalls.push((name, desc));
                w.firewall_bodies.push(body);
                let o = op(&mut w, "g");
                json_reply(200, &o)
            }
            (HttpMethod::Get, ["projects", _, "global", "firewalls"]) => {
                let w = self.state.lock().unwrap();
                let items: Vec<Value> = w
                    .firewalls
                    .iter()
                    .map(|(n, d)| json!({"name": n, "description": d}))
                    .collect();
                json_reply(200, &json!({"items": items}))
            }
            (HttpMethod::Get, ["projects", _, "global", "firewalls", name]) => {
                let w = self.state.lock().unwrap();
                match w.firewalls.iter().find(|f| f.0 == *name) {
                    Some((n, d)) => json_reply(200, &json!({"name": n, "description": d})),
                    None => not_found(),
                }
            }
            (HttpMethod::Delete, ["projects", _, "global", "firewalls", name]) => {
                let mut w = self.state.lock().unwrap();
                w.firewalls.retain(|f| f.0 != *name);
                let o = op(&mut w, "g");
                json_reply(200, &o)
            }
            (HttpMethod::Post, ["projects", _, "zones", zone, "instances"]) => {
                if let Some(hook) = self.on_insert.lock().unwrap().take() {
                    hook();
                }
                let body = req.body.clone().unwrap_or_default();
                let name = body["name"].as_str().unwrap_or_default().to_owned();
                let mut w = self.state.lock().unwrap();
                w.inserts.push((*zone).to_owned());
                w.insert_bodies.push(body.clone());
                if w.vms.iter().any(|v| v.name == name) {
                    return json_reply(
                        409,
                        &json!({"error": {"code": 409, "status": "ALREADY_EXISTS", "message": "exists"}}),
                    );
                }
                let o = op(&mut w, "z");
                if w.stockout.iter().any(|z| z == zone) {
                    let n = o["name"].as_str().unwrap().to_owned();
                    w.failing_ops
                        .push((n, "ZONE_RESOURCE_POOL_EXHAUSTED".to_owned()));
                } else {
                    w.vms.push(Vm {
                        zone: (*zone).to_owned(),
                        name,
                        status: "RUNNING",
                        labels: body["labels"].clone(),
                    });
                }
                json_reply(200, &o)
            }
            (HttpMethod::Get, ["projects", _, "zones", zone, "instances", name]) => {
                let w = self.state.lock().unwrap();
                match w.vms.iter().find(|v| v.name == *name && v.zone == *zone) {
                    Some(v) => json_reply(200, &vm_json(v)),
                    None => not_found(),
                }
            }
            (HttpMethod::Delete, ["projects", _, "zones", zone, "instances", name]) => {
                let mut w = self.state.lock().unwrap();
                w.vms.retain(|v| !(v.name == *name && v.zone == *zone));
                let o = op(&mut w, "z");
                json_reply(200, &o)
            }
            (HttpMethod::Get, ["projects", _, "aggregated", "instances"]) => {
                let w = self.state.lock().unwrap();
                let mut items = serde_json::Map::new();
                for v in w
                    .vms
                    .iter()
                    .filter(|v| v.labels.get("amicu-owner").is_some())
                {
                    let scope = format!("zones/{}", v.zone);
                    let entry = items
                        .entry(scope)
                        .or_insert_with(|| json!({"instances": []}));
                    entry["instances"].as_array_mut().unwrap().push(vm_json(v));
                }
                json_reply(200, &json!({"items": items}))
            }
            _ => json_reply(
                400,
                &json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": format!("unexpected {m} {path}")}}),
            ),
        }
    }
}

struct Tokens;

#[async_trait::async_trait]
impl TokenSource for Tokens {
    async fn token(&self) -> Result<AccessToken, GcpError> {
        Ok(AccessToken::new("ya29.fake"))
    }
}

pub(super) struct FakeGcp {
    world: Arc<World>,
    no_credentials: bool,
    exposure_fails: bool,
    ip: Result<String, String>,
}

impl Default for FakeGcp {
    fn default() -> Self {
        Self {
            world: Arc::new(World::default()),
            no_credentials: false,
            exposure_fails: false,
            ip: Ok("203.0.113.9".to_owned()),
        }
    }
}

#[async_trait::async_trait]
impl GcpOps for FakeGcp {
    fn client(&self, _: Option<&str>) -> Result<GcpClient, crate::error::CliError> {
        if self.no_credentials {
            return Err(crate::error::CliError::Message(
                GcpError::MissingCredentials.to_string(),
            ));
        }
        Ok(GcpClient::with_transport(
            Arc::new(Tokens),
            self.world.clone(),
        ))
    }
    fn probe(&self, record: &VmRecord) -> Result<Arc<dyn HttpTransport>, GcpError> {
        self.world
            .state
            .lock()
            .unwrap()
            .probe_pins
            .push(record.tls_fingerprint.clone());
        Ok(self.world.clone())
    }
    async fn caller_ip(&self) -> Result<String, String> {
        self.ip.clone()
    }
    async fn verify_exposure(
        &self,
        endpoint: &str,
        record: &VmRecord,
        _: &SecretValue,
    ) -> Result<(), String> {
        {
            let mut w = self.world.state.lock().unwrap();
            w.log.push(format!("verify {endpoint}"));
            w.verify_pins.push(record.tls_fingerprint.clone());
        }
        if self.exposure_fails {
            Err("endpoint answered an unauthenticated request".to_owned())
        } else {
            Ok(())
        }
    }
    fn service_transport(&self) -> Arc<dyn HttpTransport> {
        self.world.clone()
    }
    fn run_timing(&self) -> amicu_gcp::cloudrun::CloudRunTiming {
        amicu_gcp::cloudrun::CloudRunTiming {
            poll: amicu_gcp::PollTiming {
                interval: std::time::Duration::from_millis(1),
                timeout: std::time::Duration::from_secs(5),
            },
            health: amicu_gcp::cloudrun::HealthTiming {
                timeout: std::time::Duration::from_secs(5),
                initial_backoff: std::time::Duration::from_millis(1),
                max_backoff: std::time::Duration::from_millis(1),
            },
        }
    }
    fn timing(&self) -> amicu_gcp::GceTiming {
        amicu_gcp::GceTiming {
            poll: amicu_gcp::PollTiming {
                interval: std::time::Duration::from_millis(1),
                timeout: std::time::Duration::from_secs(5),
            },
            health: amicu_gcp::compute::HealthTiming {
                interval: std::time::Duration::from_millis(1),
                timeout: std::time::Duration::from_secs(5),
            },
        }
    }
}

struct AnyKey;

#[async_trait::async_trait]
impl SecretResolver for AnyKey {
    async fn resolve(&self, _: &SecretRef) -> Result<SecretValue, AdapterError> {
        Ok(SecretValue::new("sk-gce-test-key".to_owned()))
    }
}

fn gcp_config(extra_gcp: &str) -> String {
    gcp_config_with(&format!(
        "service_account = \"{SERVICE_ACCOUNT}\"\n    {extra_gcp}"
    ))
}

fn gcp_config_with(extra_gcp: &str) -> String {
    format!(
        r#"
resource "model" "qwen" {{
  source {{
    registry  = "hugging_face"
    reference = "Qwen/Qwen3-0.6B"
  }}
  architecture = "qwen3"
  modalities   = ["text"]
  artifact {{
    format       = "safetensors"
    quantization = "f16"
  }}
  context_length = 4096
  n_layers       = 28
  n_kv_heads     = 8
  head_dim       = 128
}}

resource "hardware_profile" "t4" {{
  provider = "gcp"
  region   = "europe-west4"
  accelerator {{
    kind   = "gpu"
    model  = "t4"
    count  = 1
    memory = "16GiB"
  }}
  memory = "15GiB"
  gcp {{
    project          = "{GOOGLE_PROJECT}"
    machine_type     = "n1-standard-4"
    max_run_duration = "2h"
    {extra_gcp}
  }}
}}

resource "deployment" "chat" {{
  model    = model.qwen.id
  hardware = hardware_profile.t4.id
  runtime  = "vllm"
  api_key  = secret("GCP_TEST_KEY")
  vllm {{
    context_length = 4096
    port           = 8000
  }}
}}
"#
    )
}

fn rig() -> Rig {
    let mut rig = Rig::new();
    rig.resolver = Box::new(AnyKey);
    rig
}

fn project_with(td: &TempDir, extra_gcp: &str) -> Project {
    project_from(td, &gcp_config(extra_gcp))
}

fn project_from(td: &TempDir, config: &str) -> Project {
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", config);
    let mut state = state_with(amicu_core::graph::Graph::new());
    state.project_id = Some(PROJECT.to_owned());
    project.save_state(&state).unwrap();
    project
}

fn instance() -> String {
    let owner = amicu_gcp::owner_marker(None, PROJECT, DEP);
    amicu_gcp::compute::instance_name(PROJECT, DEP, &owner[..8]).unwrap()
}

fn destroy(project: &Project, rig: &Rig) -> Result<u8, crate::error::CliError> {
    run_destroy(
        project,
        &[DEP.to_owned()],
        true,
        OutputFormat::Human,
        "",
        rig,
    )
    .0
}

fn the_record(project: &Project) -> amicu_gcp::GceRecord {
    stops_for(project, DEP)[0]
        .gcp
        .clone()
        .expect("a Compute Engine record")
}

#[test]
fn apply_records_the_vm_ahead_creates_the_firewall_first_and_returns_the_endpoint() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    let seen: Arc<Mutex<Option<StopRecord>>> = Arc::default();
    {
        let seen = seen.clone();
        let p = Project::new(&td.path);
        *rig.gcp.world.on_insert.lock().unwrap() = Some(Box::new(move || {
            let state = p.load_state().unwrap().unwrap();
            assert!(state.pending.iter().any(|d| d.as_str() == DEP));
            *seen.lock().unwrap() = state
                .stops
                .into_iter()
                .find(|r| r.deployment.as_str() == DEP);
        }));
    }
    apply_with(&project, &rig).unwrap();

    let ahead = seen
        .lock()
        .unwrap()
        .clone()
        .expect("recorded before the create");
    assert_eq!(ahead.backend, Backend::Gce);
    assert_eq!(ahead.identity, instance());
    let g = ahead.gcp.expect("record");
    assert_eq!(g.project, GOOGLE_PROJECT);
    assert_eq!(g.instance, instance());
    assert_eq!(g.firewall, format!("{}-fw", instance()));
    assert_eq!(g.owner, amicu_gcp::owner_marker(None, PROJECT, DEP));
    assert!(g.zone.starts_with("europe-west4-"), "{g:?}");

    let log = rig.gcp.world.log();
    assert_eq!(log[0], format!("Get /projects/{GOOGLE_PROJECT}"), "{log:?}");
    let fw = log
        .iter()
        .position(|l| l.starts_with("Post") && l.ends_with("/global/firewalls"))
        .unwrap();
    let vm = log
        .iter()
        .position(|l| l.ends_with("/instances") && l.starts_with("Post"))
        .unwrap();
    assert!(fw < vm, "{log:?}");
    let body = rig.gcp.world.state.lock().unwrap().firewall_bodies[0].clone();
    assert_eq!(body["sourceRanges"], json!(["203.0.113.4/32"]));

    let state = project.load_state().unwrap().unwrap();
    assert_eq!(state.pending.len(), 0);
    assert_eq!(state.endpoints[0].endpoint, format!("https://{IP}:8000"));
    assert!(
        log.contains(&format!("verify https://{IP}:8000")),
        "{log:?}"
    );
    assert_eq!(the_record(&project).zone, g.zone);
}

#[test]
fn without_allowed_source_cidrs_the_firewall_opens_to_this_machine_only() {
    let td = TempDir::new();
    let project = project_with(&td, "");
    let found = rig();
    apply_with(&project, &found).unwrap();
    let body = found.gcp.world.state.lock().unwrap().firewall_bodies[0].clone();
    assert_eq!(body["sourceRanges"], json!(["203.0.113.9/32"]));

    // When the address cannot be found, nothing is created and nothing opens.
    let td = TempDir::new();
    let project = project_with(&td, "");
    let mut rig = rig();
    rig.gcp.ip = Err("could not look up this machine's public IP".to_owned());
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("allowed_source_cidrs"), "{err}");
    assert_eq!(rig.gcp.world.writes(), Vec::<String>::new());
}

#[test]
fn a_stockout_falls_through_to_the_next_zone_and_the_record_follows() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    let zones = amicu_gcp::candidate_zones("t4", None, Some("europe-west4"), None).unwrap();
    assert!(zones.len() >= 2, "{zones:?}");
    rig.gcp.world.state.lock().unwrap().stockout = vec![zones[0].to_owned()];
    apply_with(&project, &rig).unwrap();

    let inserts = rig.gcp.world.state.lock().unwrap().inserts.clone();
    assert_eq!(inserts, [zones[0], zones[1]]);
    assert_eq!(
        rig.gcp.world.vm_zone(&instance()).as_deref(),
        Some(zones[1])
    );
    let g = the_record(&project);
    assert_eq!(g.zone, zones[1]);
    assert!(g.alternates.is_empty(), "{g:?}");
}

#[test]
fn when_every_zone_is_out_of_capacity_apply_fails_and_cleans_up_what_it_made() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    let zones = amicu_gcp::candidate_zones("t4", None, Some("europe-west4"), None).unwrap();
    rig.gcp.world.state.lock().unwrap().stockout = zones.iter().map(|z| (*z).to_owned()).collect();
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("no capacity"), "{err}");
    assert_eq!(rig.gcp.world.firewall_names(), Vec::<String>::new());
    assert_eq!(rig.gcp.world.vm_names(), Vec::<String>::new());
    // The record stays, listing every zone, until a stop is confirmed.
    let g = the_record(&project);
    assert_eq!(
        g.zones().len(),
        zones.len().min(amicu_gcp::gce::MAX_ZONE_ATTEMPTS)
    );
}

#[test]
fn a_vm_without_our_label_is_never_adopted_or_deleted() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    let zones = amicu_gcp::candidate_zones("t4", None, Some("europe-west4"), None).unwrap();
    rig.gcp
        .world
        .add_vm(zones[0], &instance(), json!({"env": "prod"}));
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(
        err.contains("left alone") || err.contains("owner label"),
        "{err}"
    );
    assert_eq!(rig.gcp.world.vm_names(), [instance()]);
    assert!(
        !rig.gcp
            .world
            .writes()
            .iter()
            .any(|w| w.starts_with("Delete") && w.contains("/instances/")),
        "{:?}",
        rig.gcp.world.writes()
    );
}

#[test]
fn destroy_deletes_the_vm_then_the_firewall_and_confirms_both_are_gone() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    assert_eq!(rig.gcp.world.vm_names(), [instance()]);
    destroy(&project, &rig).unwrap();

    assert_eq!(rig.gcp.world.vm_names(), Vec::<String>::new());
    assert_eq!(rig.gcp.world.firewall_names(), Vec::<String>::new());
    let log = rig.gcp.world.log();
    let del_vm = log
        .iter()
        .position(|l| l.starts_with("Delete") && l.contains("/instances/"))
        .unwrap();
    let del_fw = log
        .iter()
        .position(|l| l.starts_with("Delete") && l.contains("/firewalls/"))
        .unwrap();
    assert!(del_vm < del_fw, "{log:?}");
    // Each delete is followed by a read that ends in a 404.
    assert!(
        log[del_vm..]
            .iter()
            .any(|l| l.starts_with("Get") && l.contains("/instances/"))
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
    let state = project.load_state().unwrap().unwrap();
    assert_eq!(state.graph.deployments.len(), 0);
}

#[test]
fn destroy_refuses_a_vm_that_lost_our_label_and_keeps_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    let zone = the_record(&project).zone;
    rig.gcp.world.remove_vm(&instance());
    rig.gcp
        .world
        .add_vm(&zone, &instance(), json!({"env": "prod"}));

    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("owner label"), "{err}");
    assert_eq!(rig.gcp.world.vm_names(), [instance()], "left alone");
    assert_eq!(rig.gcp.world.firewall_names().len(), 1, "firewall kept too");
    assert_eq!(stops_for(&project, DEP).len(), 1, "record kept");
}

#[test]
fn refresh_marks_a_preempted_or_expired_vm_pending_with_a_clear_note() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();

    let (r, state) = refresh(&project, &rig);
    assert_eq!(
        r,
        crate::refresh::Refresh::default(),
        "a live VM has no notes"
    );
    assert_eq!(state.pending.len(), 0);

    rig.gcp.world.remove_vm(&instance());
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 1, "{r:?}");
    assert!(r.notes[0].contains("preempted"), "{}", r.notes[0]);
    assert!(r.notes[0].contains("max_run_duration"), "{}", r.notes[0]);
    assert!(
        r.notes[0].contains("apply will recreate it"),
        "{}",
        r.notes[0]
    );
    assert!(state.pending.iter().any(|d| d.as_str() == DEP));
    assert!(r.pending_added);
    // Destroy still finds the record and cleans up the firewall rule.
    assert_eq!(rig.gcp.world.firewall_names().len(), 1);
    destroy(&project, &rig).unwrap();
    assert_eq!(rig.gcp.world.firewall_names(), Vec::<String>::new());
}

fn refresh(project: &Project, rig: &Rig) -> (crate::refresh::Refresh, LocalState) {
    let mut state = project.load_state().unwrap().unwrap();
    let refresh =
        crate::refresh::refresh_state_mode(&mut state, &rig.modal, &rig.runpod, &rig.gcp, true);
    (refresh, state)
}

#[test]
fn missing_credentials_stop_apply_before_anything_is_written() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let mut rig = rig();
    rig.gcp.no_credentials = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("no Google Cloud credentials found"), "{err}");
    assert!(
        err.contains("gcloud auth application-default login"),
        "{err}"
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
    assert_eq!(rig.gcp.world.log(), Vec::<String>::new());
}

#[test]
fn a_failed_exposure_check_deletes_what_this_run_created() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let mut rig = rig();
    rig.gcp.exposure_fails = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("failed its exposure check"), "{err}");
    assert_eq!(rig.gcp.world.vm_names(), Vec::<String>::new());
    assert_eq!(rig.gcp.world.firewall_names(), Vec::<String>::new());
}

#[test]
fn orphans_lists_owned_vms_and_firewall_rules_read_only() {
    use crate::commands::orphans::{
        Env, GcpOpsListing, Kind, Ownership, Platform, Tracked, discover,
    };
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    // An old deployment's VM that state no longer records.
    let other = amicu_gcp::owner_labels(None, PROJECT, "old");
    rig.gcp.world.add_vm(
        "europe-west4-a",
        &format!("amicu-{PROJECT}-old-0123abcd"),
        json!(other),
    );
    rig.gcp
        .world
        .add_vm("europe-west4-a", "unrelated", json!({"env": "prod"}));
    rig.gcp.world.add_secret(
        &format!("amicu-{PROJECT}-old-0123abcd-api-key"),
        json!(other),
    );
    rig.gcp
        .world
        .add_secret("someone-elses-secret", json!({"env": "prod"}));
    let writes_before = rig.gcp.world.writes();

    let state = project.load_state().unwrap();
    let tracked =
        Tracked::from_state(state.as_ref()).with_config(project.load_graph().ok().as_ref());
    let report = discover(
        &Env {
            platform: Platform::Other,
            exec: &amicu_llamacpp::HostExec,
            modal: &NoModalApps,
            runpod: &NoRunPodListing,
            gcp: &GcpOpsListing(&rig.gcp),
            agents_dir: None,
        },
        &tracked,
    );
    let gcp = report.sources.iter().find(|s| s.name == "gcp").unwrap();
    assert_eq!(gcp.status, "ok", "{:?}", gcp.detail);
    let vms: Vec<_> = report
        .resources
        .iter()
        .filter(|r| r.kind == Kind::GceVm)
        .map(|r| (r.tracked, r.ownership, r.identity.clone()))
        .collect();
    assert_eq!(vms.len(), 2, "{vms:?}");
    assert!(vms.iter().any(|(t, _, i)| *t && i.starts_with(&instance())));
    assert!(
        vms.iter()
            .any(|(t, o, i)| !*t && *o == Ownership::ThisProject && i.contains("-old-"))
    );
    assert!(!vms.iter().any(|(_, _, i)| i.contains("unrelated")));
    let stop = report.resources.iter().find(|r| !r.tracked).unwrap();
    assert!(
        stop.stop_command
            .starts_with("gcloud compute instances delete"),
        "{}",
        stop.stop_command
    );
    assert_eq!(rig.gcp.world.writes(), writes_before);
}

/// Reads the stop record of `DEP` from state each time it is called.
fn record_taker(td: &TempDir, slot: &Arc<Mutex<Option<StopRecord>>>) -> Hook {
    let slot = slot.clone();
    let p = Project::new(&td.path);
    Box::new(move || {
        let state = p.load_state().unwrap().unwrap();
        assert!(state.pending.iter().any(|d| d.as_str() == DEP));
        *slot.lock().unwrap() = state
            .stops
            .into_iter()
            .find(|r| r.deployment.as_str() == DEP);
    })
}

#[test]
fn the_record_is_saved_before_the_first_secret_the_firewall_and_the_vm() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    let at_secret: Arc<Mutex<Option<StopRecord>>> = Arc::default();
    let at_firewall: Arc<Mutex<Option<StopRecord>>> = Arc::default();
    let at_vm: Arc<Mutex<Option<StopRecord>>> = Arc::default();
    *rig.gcp.world.on_secret_create.lock().unwrap() = Some(record_taker(&td, &at_secret));
    *rig.gcp.world.on_firewall.lock().unwrap() = Some(record_taker(&td, &at_firewall));
    *rig.gcp.world.on_insert.lock().unwrap() = Some(record_taker(&td, &at_vm));
    apply_with(&project, &rig).unwrap();

    let expected = the_record(&project);
    for (when, seen) in [
        ("first secret", at_secret),
        ("firewall rule", at_firewall),
        ("vm", at_vm),
    ] {
        let g = seen
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| panic!("no record before the {when} create"))
            .gcp
            .unwrap();
        assert_eq!(g.instance, expected.instance, "{when}");
        assert_eq!(g.firewall, expected.firewall, "{when}");
        assert_eq!(g.secrets, secret_ids(), "{when}");
        assert_eq!(g.project, GOOGLE_PROJECT, "{when}");
    }
}

fn with_project(td: &TempDir, project: &str) {
    let config = gcp_config("allowed_source_cidrs = [\"203.0.113.4/32\"]").replace(
        &format!("project          = \"{GOOGLE_PROJECT}\""),
        &format!("project          = \"{project}\""),
    );
    assert!(config.contains(project), "{config}");
    write(&td.path, "amicu.hcl", &config);
}

#[test]
fn a_project_change_stops_the_old_vm_first_and_never_overwrites_its_record() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    assert_eq!(the_record(&project).project, GOOGLE_PROJECT);

    // Move the deployment to another Google project; the instance name does
    // not depend on it.
    with_project(&td, "other-proj");
    let at_secret: Arc<Mutex<Option<StopRecord>>> = Arc::default();
    *rig.gcp.world.on_secret_create.lock().unwrap() = Some(record_taker(&td, &at_secret));
    let before = rig.gcp.world.log().len();
    apply_with(&project, &rig).unwrap();

    let log = rig.gcp.world.log()[before..].to_vec();
    let old = format!("/projects/{GOOGLE_PROJECT}/");
    let new = "/projects/other-proj/".to_owned();
    let last_old_delete = log
        .iter()
        .rposition(|l| l.starts_with("Delete") && l.contains(&old))
        .unwrap_or_else(|| panic!("the old VM was not deleted: {log:?}"));
    let first_new_write = log
        .iter()
        .position(|l| l.starts_with("Post") && l.contains(&new) || l.starts_with("secret-create"))
        .unwrap();
    assert!(last_old_delete < first_new_write, "{log:?}");
    // Its firewall rule and secrets went with it.
    assert!(
        log.iter()
            .any(|l| l.starts_with("Delete") && l.contains(&old) && l.contains("/firewalls/")),
        "{log:?}"
    );
    assert!(
        log.iter().any(|l| l.starts_with("secret-delete")),
        "{log:?}"
    );
    // By the first new write the state names only the new project.
    let g = at_secret.lock().unwrap().clone().unwrap().gcp.unwrap();
    assert_eq!(g.project, "other-proj");
    let stops = stops_for(&project, DEP);
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(the_record(&project).project, "other-proj");
}

#[test]
fn a_project_change_with_a_failed_old_teardown_keeps_the_old_record_and_creates_nothing() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    // The old VM lost our label: its teardown is refused.
    let zone = the_record(&project).zone;
    rig.gcp.world.remove_vm(&instance());
    rig.gcp
        .world
        .add_vm(&zone, &instance(), json!({"env": "prod"}));

    with_project(&td, "other-proj");
    let before = rig.gcp.world.log().len();
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("owner label"), "{err}");
    let log = rig.gcp.world.log()[before..].to_vec();
    assert!(
        !log.iter()
            .any(|l| l.starts_with("Post") && l.contains("/projects/other-proj/")),
        "{log:?}"
    );
    // The old record is untouched, so destroy still finds the old resources.
    let stops = stops_for(&project, DEP);
    assert_eq!(stops.len(), 1, "{stops:?}");
    let g = the_record(&project);
    assert_eq!(g.project, GOOGLE_PROJECT);
    assert_eq!(g.secrets, secret_ids());
    assert_eq!(rig.gcp.world.firewall_names().len(), 1);
}

#[test]
fn switching_to_the_metadata_delivery_keeps_the_old_secrets_recorded() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    assert_eq!(the_record(&project).secrets, secret_ids());

    // Same project, same instance, but no Secret Manager any more. The old
    // secrets must stay on the record until they are deleted.
    write(
        &td.path,
        "amicu.hcl",
        &gcp_config_with(
            "allowed_source_cidrs = [\"203.0.113.4/32\"]\ninsecure_metadata_secrets = true",
        ),
    );
    let at_vm: Arc<Mutex<Option<StopRecord>>> = Arc::default();
    *rig.gcp.world.on_insert.lock().unwrap() = Some(record_taker(&td, &at_vm));
    apply_with(&project, &rig).unwrap();
    let g = at_vm.lock().unwrap().clone().unwrap().gcp.unwrap();
    assert_eq!(g.secrets, secret_ids(), "{g:?}");
}

struct NoModalApps;

impl crate::commands::orphans::ModalApps for NoModalApps {
    fn list(&self) -> Result<Vec<crate::commands::orphans::ModalApp>, String> {
        Ok(Vec::new())
    }
}

struct NoRunPodListing;

impl crate::commands::orphans::RunPodResources for NoRunPodListing {
    fn list(&self) -> Result<crate::commands::orphans::RunPodListing, String> {
        panic!("RunPod must not be queried")
    }
}

const SECRET_SUFFIXES: [&str; 3] = ["api-key", "hf-token", "tls-key"];

fn secret_ids() -> Vec<String> {
    SECRET_SUFFIXES
        .iter()
        .map(|s| format!("{}-{s}", instance()))
        .collect()
}

fn metadata_keys(body: &Value) -> Vec<String> {
    body["metadata"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn secrets_are_created_before_the_vm_and_recorded_with_the_fingerprint() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    let seen: Arc<Mutex<Option<StopRecord>>> = Arc::default();
    {
        let seen = seen.clone();
        let p = Project::new(&td.path);
        *rig.gcp.world.on_insert.lock().unwrap() = Some(Box::new(move || {
            *seen.lock().unwrap() = p
                .load_state()
                .unwrap()
                .unwrap()
                .stops
                .into_iter()
                .find(|r| r.deployment.as_str() == DEP);
        }));
    }
    apply_with(&project, &rig).unwrap();

    // Recorded before the create: the ids and the pin.
    let ahead = seen.lock().unwrap().clone().unwrap().gcp.unwrap();
    assert_eq!(ahead.secrets, secret_ids());
    let fingerprint = ahead.tls_fingerprint.clone().expect("a fingerprint");
    assert_eq!(fingerprint.len(), 64, "{fingerprint}");
    assert_eq!(the_record(&project).tls_fingerprint, Some(fingerprint));

    // The api key and the TLS key (and, with no token set, nothing else) were
    // stored before the VM, each readable by the service account alone.
    let log = rig.gcp.world.log();
    let vm = log
        .iter()
        .position(|l| l.starts_with("Post") && l.ends_with("/instances"))
        .unwrap();
    for id in [&secret_ids()[0], &secret_ids()[2]] {
        let created = rig.gcp.world.position(&format!("secret-create {id}"));
        let version = rig.gcp.world.position(&format!("secret-version {id}"));
        assert!(created.unwrap() < version.unwrap(), "{log:?}");
        assert!(version.unwrap() < vm, "{log:?}");
    }
    let w = rig.gcp.world.state.lock().unwrap();
    assert_eq!(w.secrets.len(), 2, "{:?}", w.secrets);
    for (id, labels, replication) in &w.secrets {
        assert!(secret_ids().contains(id), "{id}");
        assert!(labels.get("amicu-owner").is_some(), "{labels}");
        assert_eq!(
            replication,
            &json!({"userManaged": {"replicas": [{"location": "europe-west4"}]}})
        );
    }
    assert!(
        w.grants
            .iter()
            .all(|(_, m)| m == &format!("serviceAccount:{SERVICE_ACCOUNT}")),
        "{:?}",
        w.grants
    );
    assert_eq!(w.grants.len(), 2);

    // No secret in the instance metadata; the certificate (public) is.
    let keys = metadata_keys(&w.insert_bodies[0]);
    assert!(keys.contains(&"startup-script".to_owned()), "{keys:?}");
    assert!(keys.contains(&"amicu-tls-cert".to_owned()), "{keys:?}");
    assert!(
        !keys
            .iter()
            .any(|k| k == "amicu-api-key" || k == "amicu-tls-key"),
        "{keys:?}"
    );
    assert_eq!(
        w.insert_bodies[0]["serviceAccounts"][0]["email"],
        SERVICE_ACCOUNT
    );
}

#[test]
fn the_pinned_transport_is_used_and_the_certificate_is_written_and_printed() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    let mut out = Vec::new();
    crate::commands::apply::run_with(
        &project,
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &rig.env(),
    )
    .unwrap();
    let out = utf8(out);
    let g = the_record(&project);
    let fingerprint = g.tls_fingerprint.clone().unwrap();

    let w = rig.gcp.world.state.lock().unwrap();
    assert_eq!(w.probe_pins, [Some(fingerprint.clone())]);
    assert_eq!(w.verify_pins, [Some(fingerprint.clone())]);
    assert!(
        w.log
            .contains(&format!("probe Get https://{IP}:8000/health")),
        "{:?}",
        w.log
    );
    assert!(!w.log.iter().any(|l| l.contains("http://")), "{:?}", w.log);
    drop(w);

    let cert = project.state_dir().join(format!("{}.tls.crt", instance()));
    let pem = std::fs::read_to_string(&cert).expect("the certificate next to state");
    assert_eq!(
        amicu_gcp::compute::fingerprint_of_pem(&pem).unwrap(),
        fingerprint
    );
    assert!(out.contains(&format!("https://{IP}:8000")), "{out}");
    assert!(out.contains("pinned self-signed certificate"), "{out}");
    assert!(out.contains(&cert.display().to_string()), "{out}");
    assert!(out.contains(&fingerprint), "{out}");
    assert!(!out.contains("plain HTTP"), "{out}");
}

#[test]
fn destroy_deletes_the_vm_then_the_firewall_then_the_secrets() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    assert_eq!(rig.gcp.world.secret_ids().len(), 2);
    destroy(&project, &rig).unwrap();

    assert_eq!(rig.gcp.world.vm_names(), Vec::<String>::new());
    assert_eq!(rig.gcp.world.firewall_names(), Vec::<String>::new());
    assert_eq!(rig.gcp.world.secret_ids(), Vec::<String>::new());
    let log = rig.gcp.world.log();
    let at = |p: &str| log.iter().position(|l| l.starts_with(p)).unwrap();
    let del_vm = log
        .iter()
        .position(|l| l.starts_with("Delete") && l.contains("/instances/"))
        .unwrap();
    let del_fw = log
        .iter()
        .position(|l| l.starts_with("Delete") && l.contains("/firewalls/"))
        .unwrap();
    assert!(del_vm < del_fw && del_fw < at("secret-delete"), "{log:?}");
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn teardown_is_idempotent_and_never_deletes_a_secret_that_is_not_ours() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    let record = the_record(&project);
    let client = rig.gcp.client(None).unwrap();
    let timing = rig.gcp.timing().poll;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(amicu_gcp::gce::teardown(&client, &record, timing))
        .unwrap();
    assert_eq!(rig.gcp.world.secret_ids(), Vec::<String>::new());
    // Again, as after a crash that kept the record: nothing left, no error.
    rt.block_on(amicu_gcp::gce::teardown(&client, &record, timing))
        .unwrap();

    // A secret of that name without our label is refused and left alone.
    rig.gcp
        .world
        .add_secret(&secret_ids()[0], json!({"env": "prod"}));
    let err = rt
        .block_on(amicu_gcp::gce::teardown(&client, &record, timing))
        .unwrap_err()
        .to_string();
    assert!(err.contains("owner label"), "{err}");
    assert_eq!(rig.gcp.world.secret_ids(), [secret_ids()[0].clone()]);
}

#[test]
fn a_foreign_secret_of_our_name_stops_apply_before_the_vm_exists() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    rig.gcp
        .world
        .add_secret(&secret_ids()[0], json!({"env": "prod"}));
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("without amicu's owner label"), "{err}");
    assert_eq!(rig.gcp.world.vm_names(), Vec::<String>::new());
    assert_eq!(rig.gcp.world.secret_ids(), [secret_ids()[0].clone()]);
}

#[test]
fn a_gcp_block_without_a_service_account_is_a_diagnostic_and_nothing_is_written() {
    let td = TempDir::new();
    let project = project_from(
        &td,
        &gcp_config_with("allowed_source_cidrs = [\"203.0.113.4/32\"]"),
    );
    let rig = rig();
    let mut out = Vec::new();
    let code = crate::commands::apply::run_with(
        &project,
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &rig.env(),
    )
    .unwrap();
    let err = utf8(out);
    assert_ne!(code, 0, "{err}");
    assert!(err.contains("service_account"), "{err}");
    assert!(err.contains("insecure_metadata_secrets"), "{err}");
    assert!(err.contains("AMICU-CAP-016"), "{err}");
    assert_eq!(rig.gcp.world.log(), Vec::<String>::new());
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn the_metadata_opt_in_skips_secret_manager_and_says_so_in_the_instance_metadata() {
    let td = TempDir::new();
    let project = project_from(
        &td,
        &gcp_config_with(
            "allowed_source_cidrs = [\"203.0.113.4/32\"]\n    insecure_metadata_secrets = true",
        ),
    );
    let rig = rig();
    apply_with(&project, &rig).unwrap();

    assert_eq!(rig.gcp.world.secret_ids(), Vec::<String>::new());
    assert!(
        !rig.gcp.world.log().iter().any(|l| l.starts_with("secret-")),
        "{:?}",
        rig.gcp.world.log()
    );
    let g = the_record(&project);
    assert_eq!(g.secrets, Vec::<String>::new());
    assert!(g.tls_fingerprint.is_some(), "TLS stays on");
    let w = rig.gcp.world.state.lock().unwrap();
    let keys = metadata_keys(&w.insert_bodies[0]);
    assert!(keys.contains(&"amicu-api-key".to_owned()), "{keys:?}");
    assert!(keys.contains(&"amicu-tls-key".to_owned()), "{keys:?}");
    assert!(w.insert_bodies[0]["serviceAccounts"].is_array());
    drop(w);

    destroy(&project, &rig).unwrap();
    assert_eq!(rig.gcp.world.vm_names(), Vec::<String>::new());
    assert_eq!(rig.gcp.world.firewall_names(), Vec::<String>::new());
}

#[test]
fn preflight_stops_before_any_write_when_secret_manager_is_not_enabled() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    rig.gcp.world.disable_secret_manager();
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(
        err.contains("gcloud services enable secretmanager.googleapis.com"),
        "{err}"
    );
    assert_eq!(rig.gcp.world.writes(), Vec::<String>::new());
}

#[test]
fn preflight_stops_before_any_write_when_the_default_network_is_missing() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    rig.gcp.world.state.lock().unwrap().no_default_network = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("`default` network"), "{err}");
    assert!(
        err.contains("gcloud compute networks create default"),
        "{err}"
    );
    assert_eq!(rig.gcp.world.writes(), Vec::<String>::new());
}

#[test]
fn preflight_names_the_role_when_the_caller_cannot_act_as_the_service_account() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    rig.gcp.world.state.lock().unwrap().act_as_denied = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("roles/iam.serviceAccountUser"), "{err}");
    assert!(err.contains(SERVICE_ACCOUNT), "{err}");
    assert_eq!(rig.gcp.world.writes(), Vec::<String>::new());
}

#[test]
fn metadata_secrets_do_not_need_the_secret_manager_api() {
    let td = TempDir::new();
    let project = project_from(
        &td,
        &gcp_config_with(
            "allowed_source_cidrs = [\"203.0.113.4/32\"]\n    insecure_metadata_secrets = true",
        ),
    );
    let rig = rig();
    rig.gcp.world.disable_secret_manager();
    apply_with(&project, &rig).unwrap();
}

#[test]
fn a_start_that_never_wrote_a_secret_is_released_when_secret_manager_is_disabled() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    let off = rig.gcp.world.sm_disabled.clone();
    *rig.gcp.world.on_secret_create.lock().unwrap() = Some(Box::new(move || {
        off.store(true, std::sync::atomic::Ordering::SeqCst);
    }));
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("gcloud services enable"), "{err}");
    assert_eq!(rig.gcp.world.vm_names(), Vec::<String>::new());
    assert!(the_record(&project).secrets_unwritten);
    destroy(&project, &rig).unwrap();
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn a_preempted_vm_with_a_disabled_secret_manager_keeps_its_record() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    assert!(!the_record(&project).secrets_unwritten);
    rig.gcp.world.remove_vm(&instance());
    rig.gcp.world.disable_secret_manager();
    assert!(destroy(&project, &rig).is_err());
    assert_eq!(stops_for(&project, DEP).len(), 1);
    assert_ne!(rig.gcp.world.secret_ids(), Vec::<String>::new());
}

#[test]
fn a_disabled_secret_manager_is_an_error_while_the_vm_may_still_exist() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    let record = the_record(&project);
    let client = rig.gcp.client(None).unwrap();
    let timing = rig.gcp.timing().poll;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rig.gcp.world.disable_secret_manager();
    let err = rt
        .block_on(amicu_gcp::gce::teardown(&client, &record, timing))
        .unwrap_err();
    assert!(err.is_service_disabled(), "{err:?}");
    assert!(err.to_string().contains("gcloud services enable"), "{err}");
}

#[test]
fn a_permission_denied_secret_delete_names_the_missing_permission_and_keeps_state() {
    let td = TempDir::new();
    let project = project_with(&td, "allowed_source_cidrs = [\"203.0.113.4/32\"]");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    let record = the_record(&project);
    let client = rig.gcp.client(None).unwrap();
    let timing = rig.gcp.timing().poll;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rig.gcp.world.remove_vm(&instance());
    rig.gcp.world.state.lock().unwrap().secretmanager_denied = true;
    let err = rt
        .block_on(amicu_gcp::gce::teardown(&client, &record, timing))
        .unwrap_err()
        .to_string();
    assert!(err.contains("secretmanager.secrets.delete"), "{err}");
    assert_eq!(stops_for(&project, DEP).len(), 1);
}

#[test]
fn idle_shutdown_puts_the_watchdog_in_the_startup_script() {
    let td = TempDir::new();
    let project = project_with(
        &td,
        "allowed_source_cidrs = [\"203.0.113.4/32\"]\n    idle_shutdown = \"30m\"",
    );
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    let body = rig.gcp.world.state.lock().unwrap().insert_bodies[0].clone();
    let script = body["metadata"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == "startup-script")
        .unwrap()["value"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(script.contains("window=1800"), "{script}");
    assert!(script.contains("shutdown -h now"), "{script}");
}

mod cloudrun;
