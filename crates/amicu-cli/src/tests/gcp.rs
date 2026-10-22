//! `provider = "gcp"` through apply, destroy, refresh and orphans, against an
//! in-memory Compute Engine API. No Google calls.

use std::sync::{Arc, Mutex};

use amicu_core::adapter::AdapterError;
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use amicu_gcp::{AccessToken, GcpClient, GcpError, TokenSource};
use amicu_providers::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, TransportError};
use serde_json::{Value, json};

use super::*;
use crate::gcp_ops::GcpOps;

const BASE: &str = "https://compute.googleapis.com/compute/v1";
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
}

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct World {
    state: Mutex<WorldState>,
    /// Fired once, just before the first instance insert.
    on_insert: Mutex<Option<Hook>>,
}

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
            (_, ["projects", _, "global", "operations", name, "wait"])
            | (_, ["projects", _, "zones", _, "operations", name, "wait"]) => {
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
    fn probe(&self) -> Arc<dyn HttpTransport> {
        self.world.clone()
    }
    async fn caller_ip(&self) -> Result<String, String> {
        self.ip.clone()
    }
    async fn verify_exposure(&self, endpoint: &str, _: &SecretValue) -> Result<(), String> {
        self.world
            .state
            .lock()
            .unwrap()
            .log
            .push(format!("verify {endpoint}"));
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
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", &gcp_config(extra_gcp));
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
    assert_eq!(state.endpoints[0].endpoint, format!("http://{IP}:8000"));
    assert!(log.contains(&format!("verify http://{IP}:8000")), "{log:?}");
    assert_eq!(the_record(&project).zone, g.zone);
}

#[test]
fn without_allowed_source_cidrs_the_firewall_opens_to_this_machine_only() {
    let td = TempDir::new();
    let project = project_with(&td, "");
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    let body = rig.gcp.world.state.lock().unwrap().firewall_bodies[0].clone();
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
    assert!(rig.gcp.world.firewall_names().is_empty());
    assert!(rig.gcp.world.vm_names().is_empty());
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

    assert!(rig.gcp.world.vm_names().is_empty());
    assert!(rig.gcp.world.firewall_names().is_empty());
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
    assert!(stops_for(&project, DEP).is_empty());
    let state = project.load_state().unwrap().unwrap();
    assert!(state.graph.deployments.is_empty());
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
    assert!(state.pending.is_empty());

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
    assert!(rig.gcp.world.firewall_names().is_empty());
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
    assert!(stops_for(&project, DEP).is_empty());
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
    assert!(rig.gcp.world.vm_names().is_empty());
    assert!(rig.gcp.world.firewall_names().is_empty());
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
