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
}

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct World {
    state: Mutex<WorldState>,
    /// Fired once, just before the first instance insert.
    on_insert: Mutex<Option<Hook>>,
}

#[allow(clippy::unnecessary_wraps)]
fn json_reply(status: u16, body: &Value) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse::new(status, body.to_string().into_bytes()))
}

fn not_found() -> Result<HttpResponse, TransportError> {
    json_reply(
        404,
        &json!({"error": {"code": 404, "status": "NOT_FOUND", "message": "not found"}}),
    )
}

impl World {
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
        let m = format!("{method:?}");
        match (method, parts.as_slice()) {
            (HttpMethod::Post, ["projects", _, "secrets"]) => {
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
                        let labels = w.secrets.iter().find(|s| s.0 == id).unwrap().1.clone();
                        json_reply(200, &json!({"name": id, "labels": labels}))
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
