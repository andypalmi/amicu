//! `provider = "runpod"` through apply and destroy, against an in-memory
//! RunPod v2 API.

use std::sync::{Arc, Mutex};

use amicu_core::adapter::AdapterError;
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use amicu_providers::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, TransportError};
use amicu_runpod::{ApiKey, RunPodClient};
use serde_json::{Value, json};

use super::*;
use crate::runpod_ops::RunPodOps;

const BASE: &str = "https://rp.test/v2";
const PROJECT: &str = "aaaa1111";
const DEP: &str = "support_local";
const MARKER_ID: &str = "s0";
const ACCOUNT: &str = "personal:s0";
const MANAGEMENT_KEY: &str = "rpa_MANAGEMENT";

#[derive(Clone)]
struct Ep {
    id: String,
    name: String,
    owner: Option<String>,
    max_workers: Option<u64>,
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct WorldState {
    /// `Some`: an organization account with this id.
    org: Option<String>,
    secrets: Vec<(String, String)>,
    endpoints: Vec<Ep>,
    next: u32,
    reject_key: bool,
    /// Health answer of every endpoint (0: 200).
    health: u16,
    delete_fails: bool,
    secret_delete_fails: bool,
    /// The endpoint create goes through but its response is lost.
    lose_create_response: bool,
    /// `METHOD path` of every request, in order.
    log: Vec<String>,
    /// `(id, status)` of the workers every endpoint reports.
    workers: Vec<(String, String)>,
    /// What `/metrics` answers through the gateway.
    metrics: String,
}

type Hook = Box<dyn FnOnce() + Send>;

/// A stateful fake of the RunPod endpoints amicu calls.
#[derive(Default)]
pub(super) struct World {
    state: Mutex<WorldState>,
    /// Fired once, just before the first endpoint create.
    on_create: Mutex<Option<Hook>>,
}

#[allow(clippy::unnecessary_wraps)]
fn reply(status: u16, body: &Value) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse::new(status, body.to_string().into_bytes()))
}

fn ep_json(e: &Ep) -> Value {
    let mut env = serde_json::Map::new();
    if let Some(o) = &e.owner {
        env.insert(amicu_runpod::OWNER_ENV_VAR.to_owned(), json!(o));
    }
    json!({"id": e.id, "name": e.name, "type": "LOAD_BALANCER", "env": env,
        "gpu": {"pools": ["AMPERE_24"], "count": 1},
        "workers": {"min": 0, "max": e.max_workers.unwrap_or(1)},
        "dataCenterIds": ["EU-RO-1", "EU-SE-1"]})
}

fn owner_in(body: Option<&Value>) -> Option<String> {
    body?
        .get("env")?
        .get(amicu_runpod::OWNER_ENV_VAR)?
        .as_str()
        .map(str::to_owned)
}

#[async_trait::async_trait]
impl HttpTransport for World {
    #[allow(clippy::too_many_lines)]
    async fn execute(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        if let Some(id) = req
            .url
            .strip_prefix("https://")
            .and_then(|u| u.strip_suffix(".api.runpod.ai/health"))
        {
            let mut w = self.state.lock().unwrap();
            w.log.push(format!("health {id}"));
            let status = if w.health == 0 { 200 } else { w.health };
            return reply(status, &json!({}));
        }
        if let Some(id) = req
            .url
            .strip_prefix("https://")
            .and_then(|u| u.strip_suffix(".api.runpod.ai/metrics"))
        {
            let mut w = self.state.lock().unwrap();
            w.log.push(format!(
                "metrics {id} bearer={}",
                req.bearer.as_deref().unwrap_or("")
            ));
            return Ok(HttpResponse::new(200, w.metrics.clone().into_bytes()));
        }
        let path = req
            .url
            .strip_prefix(BASE)
            .expect("RunPod base URL")
            .to_owned();
        if req.method == HttpMethod::Post && path == "/serverless" {
            let hook = self.on_create.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let mut w = self.state.lock().unwrap();
        w.log.push(format!("{:?} {path}", req.method));
        let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (req.method, seg.as_slice()) {
            (HttpMethod::Get, ["organization"]) => match &w.org {
                Some(id) => reply(200, &json!({"id": id, "name": "acme"})),
                None => reply(
                    403,
                    &json!({"detail": "Organizations are not enabled for this user"}),
                ),
            },
            (HttpMethod::Get, ["serverless?limit=1"]) if w.reject_key => {
                reply(401, &json!({"detail": "unauthorized"}))
            }
            (HttpMethod::Get, ["serverless" | "serverless?limit=1"]) => {
                let eps: Vec<Value> = w.endpoints.iter().map(ep_json).collect();
                reply(
                    200,
                    &json!({"endpoints": eps, "pagination": {"hasNextPage": false}}),
                )
            }
            (HttpMethod::Post, ["serverless"]) => {
                w.next += 1;
                let ep = Ep {
                    id: format!("ep{}", w.next),
                    name: req.body.as_ref().unwrap()["name"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                    owner: owner_in(req.body.as_ref()),
                    max_workers: None,
                };
                w.endpoints.push(ep.clone());
                if w.lose_create_response {
                    return Err(TransportError::Timeout);
                }
                reply(201, &ep_json(&ep))
            }
            (HttpMethod::Get, ["serverless", id, "workers"]) => {
                if !w.endpoints.iter().any(|e| e.id == *id) {
                    return reply(404, &json!({"detail": "not found"}));
                }
                let running = w.workers.iter().filter(|x| x.1 == "RUNNING").count();
                let idle = w.workers.iter().filter(|x| x.1 == "IDLE").count();
                let list: Vec<Value> = w
                    .workers
                    .iter()
                    .map(|(id, status)| json!({"id": id, "status": status}))
                    .collect();
                reply(
                    200,
                    &json!({"workers": list, "summary": {"running": running, "idle": idle,
                        "total": list.len()}}),
                )
            }
            (HttpMethod::Get, ["serverless", _, "workers", wid, logs])
                if logs.starts_with("logs?tail=") =>
            {
                let body = format!(
                    "data: {}\n\ndata: {}\n\n",
                    json!({"ts": format!("2026-01-01T00:00:0{}Z", wid.len()), "source": "container", "line": format!("hello from {wid}")}),
                    json!({"ts": "2026-01-01T00:00:09Z", "source": "system", "line": "done"}),
                );
                Ok(HttpResponse::new(200, body.into_bytes()))
            }
            (method, ["serverless", id]) => {
                let Some(i) = w.endpoints.iter().position(|e| e.id == *id) else {
                    return reply(404, &json!({"detail": "not found"}));
                };
                match method {
                    HttpMethod::Get => reply(200, &ep_json(&w.endpoints[i])),
                    HttpMethod::Patch => {
                        if let Some(o) = owner_in(req.body.as_ref()) {
                            w.endpoints[i].owner = Some(o);
                        }
                        if let Some(max) = req
                            .body
                            .as_ref()
                            .and_then(|b| b.get("workers")?.get("max")?.as_u64())
                        {
                            w.endpoints[i].max_workers = Some(max);
                        }
                        reply(200, &ep_json(&w.endpoints[i]))
                    }
                    HttpMethod::Delete if w.delete_fails => {
                        reply(500, &json!({"detail": "internal error"}))
                    }
                    HttpMethod::Delete => {
                        w.endpoints.remove(i);
                        Ok(HttpResponse::new(204, Vec::new()))
                    }
                    HttpMethod::Post => reply(405, &json!({})),
                }
            }
            (HttpMethod::Get, ["catalog", "gpus"]) => reply(
                200,
                &json!({"gpus": [{"id": "NVIDIA L4", "name": "L4", "pool": "AMPERE_24", "memory": 24}]}),
            ),
            (HttpMethod::Get, ["account", q]) if q.starts_with("secrets?name=") => {
                let name = q.trim_start_matches("secrets?name=");
                let found: Vec<Value> = w
                    .secrets
                    .iter()
                    .filter(|(_, n)| n == name)
                    .map(|(id, n)| json!({"id": id, "name": n}))
                    .collect();
                reply(200, &json!({"secrets": found}))
            }
            (HttpMethod::Post, ["account", "secrets"]) => {
                w.next += 1;
                let id = format!("s{}", w.next);
                let name = req.body.as_ref().unwrap()["name"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                w.secrets.push((id.clone(), name.clone()));
                reply(201, &json!({"id": id, "name": name}))
            }
            (HttpMethod::Delete, ["account", "secrets", _]) if w.secret_delete_fails => {
                reply(500, &json!({"detail": "internal error"}))
            }
            (HttpMethod::Delete, ["account", "secrets", id]) => {
                let before = w.secrets.len();
                w.secrets.retain(|(i, _)| i != id);
                if w.secrets.len() == before {
                    reply(404, &json!({"detail": "not found"}))
                } else {
                    Ok(HttpResponse::new(204, Vec::new()))
                }
            }
            _ => reply(500, &json!({"detail": format!("unrouted {path}")})),
        }
    }
}

impl World {
    fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }
    fn writes(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|l| !l.starts_with("Get") && !l.starts_with("health"))
            .collect()
    }
    fn endpoint_ids(&self) -> Vec<String> {
        let w = self.state.lock().unwrap();
        w.endpoints.iter().map(|e| e.id.clone()).collect()
    }
    fn secret_names(&self) -> Vec<String> {
        let w = self.state.lock().unwrap();
        w.secrets.iter().map(|(_, n)| n.clone()).collect()
    }
}

pub(super) struct FakeRunPod {
    world: Arc<World>,
    no_key: bool,
    exposure_fails: bool,
}

impl Default for FakeRunPod {
    fn default() -> Self {
        let world = World::default();
        world.state.lock().unwrap().secrets.push((
            MARKER_ID.to_owned(),
            amicu_runpod::ACCOUNT_MARKER_SECRET.to_owned(),
        ));
        Self {
            world: Arc::new(world),
            no_key: false,
            exposure_fails: false,
        }
    }
}

#[async_trait::async_trait]
impl RunPodOps for FakeRunPod {
    fn client(&self) -> Result<RunPodClient, crate::error::CliError> {
        if self.no_key {
            return Err(crate::runpod_ops::missing_key());
        }
        Ok(RunPodClient::with_transport(
            ApiKey::new(MANAGEMENT_KEY),
            self.world.clone(),
            BASE,
        ))
    }
    fn is_management_key(&self, key: &SecretValue) -> bool {
        key.expose_secret() == MANAGEMENT_KEY.as_bytes()
    }
    fn probe(&self) -> Arc<dyn HttpTransport> {
        self.world.clone()
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
}

struct AnyKey;

#[async_trait::async_trait]
impl SecretResolver for AnyKey {
    async fn resolve(&self, _: &SecretRef) -> Result<SecretValue, AdapterError> {
        Ok(SecretValue::new("rpa_ENDPOINTKEY".to_owned()))
    }
}

fn runpod_config() -> String {
    let model = VALID_CONFIG
        .split("resource \"hardware_profile\"")
        .next()
        .unwrap();
    format!(
        r#"{model}
resource "hardware_profile" "runpod_l4" {{
  provider = "runpod"
  accelerator {{
    kind   = "gpu"
    model  = "L4"
    count  = 1
    memory = "24GiB"
  }}
  memory = "32GiB"
}}

resource "deployment" "support_local" {{
  model    = model.support_chat.id
  hardware = hardware_profile.runpod_l4.id
  runtime  = "llama_cpp"
  api_key  = secret("RUNPOD_TEST_ENDPOINT_KEY")
  llama_cpp {{
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
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

fn endpoint_name() -> String {
    amicu_runpod::project_endpoint_name(PROJECT, DEP)
}

fn owner() -> String {
    amicu_runpod::owner_marker(Some(ACCOUNT), PROJECT, DEP)
}

/// A project with the RunPod config and, when `record` is given, a state
/// holding it (and the deployment as pending).
fn project_with(td: &TempDir, record: Option<StopRecord>) -> Project {
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", &runpod_config());
    let mut state = state_with(amicu_core::graph::Graph::new());
    state.project_id = Some(PROJECT.to_owned());
    if let Some(r) = record {
        state.graph = project.load_graph().unwrap();
        state.pending = vec![DEP.into()];
        state.stops = vec![r];
    }
    project.save_state(&state).unwrap();
    project
}

fn record(id: Option<&str>, account: &str, secrets: &[&str]) -> StopRecord {
    StopRecord::runpod_in(
        PROJECT,
        &DEP.into(),
        id.map(str::to_owned),
        Some(account),
        secrets.iter().map(|s| (*s).to_owned()).collect(),
    )
}

fn add_endpoint(rig: &Rig, id: &str, owner: Option<String>) {
    rig.runpod.world.state.lock().unwrap().endpoints.push(Ep {
        id: id.to_owned(),
        name: endpoint_name(),
        owner,
        max_workers: None,
    });
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

#[test]
fn apply_records_the_endpoint_ahead_then_its_id_and_secrets() {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let rig = rig();
    let seen: Arc<Mutex<Option<StopRecord>>> = Arc::default();
    {
        let seen = seen.clone();
        let p = Project::new(&td.path);
        *rig.runpod.world.on_create.lock().unwrap() = Some(Box::new(move || {
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
    assert_eq!(ahead.backend, Backend::Runpod);
    assert_eq!(ahead.identity, endpoint_name());
    assert_eq!(ahead.app_id, None);
    assert_eq!(ahead.workspace.as_deref(), Some(ACCOUNT));

    let log = rig.runpod.world.log();
    assert_eq!(log[0], "Get /serverless?limit=1", "{log:?}");
    let stops = stops_for(&project, DEP);
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].app_id.as_deref(), Some("ep1"));
    assert_eq!(stops[0].secrets, ahead.secrets);
    let state = project.load_state().unwrap().unwrap();
    assert_eq!(state.pending.len(), 0);
    assert_eq!(state.endpoints[0].id, "ep1");
    assert!(
        log.contains(&"verify https://ep1.api.runpod.ai".to_owned()),
        "{log:?}"
    );
}

#[test]
fn an_endpoint_created_before_a_crash_is_adopted_by_name_not_created_again() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(None, ACCOUNT, &[])));
    let rig = rig();
    add_endpoint(&rig, "ep9", Some(owner()));
    apply_with(&project, &rig).unwrap();

    let writes = rig.runpod.world.writes();
    assert!(
        !writes.iter().any(|w| w == "Post /serverless"),
        "{writes:?}"
    );
    assert!(
        writes.contains(&"Patch /serverless/ep9".to_owned()),
        "{writes:?}"
    );
    assert_eq!(rig.runpod.world.endpoint_ids(), ["ep9"]);
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("ep9"));
}

#[test]
fn a_recorded_endpoint_is_updated_by_id_without_a_name_lookup() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &[])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    apply_with(&project, &rig).unwrap();

    let log = rig.runpod.world.log();
    assert!(!log.contains(&"Get /serverless".to_owned()), "{log:?}");
    assert!(log.contains(&"Patch /serverless/ep7".to_owned()), "{log:?}");
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("ep7"));
}

#[test]
fn a_record_from_another_account_is_refused_before_anything_is_written() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), "org:other", &["x"])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("lives in RunPod account org:other"), "{err}");
    assert_eq!(rig.runpod.world.writes().len(), 0);
    assert_eq!(
        stops_for(&project, DEP),
        [record(Some("ep7"), "org:other", &["x"])]
    );
}

#[test]
fn a_rejected_management_key_stops_apply_before_anything_is_written() {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let rig = rig();
    rig.runpod.world.state.lock().unwrap().reject_key = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("RUNPOD_API_KEY"), "{err}");
    assert_eq!(rig.runpod.world.log(), ["Get /serverless?limit=1"]);
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn a_missing_management_key_stops_apply_before_any_call() {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let mut rig = rig();
    rig.runpod.no_key = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("RUNPOD_API_KEY"), "{err}");
    assert_eq!(rig.runpod.world.log().len(), 0);
}

#[test]
fn a_failed_exposure_check_tears_the_new_endpoint_down_and_keeps_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let mut rig = rig();
    rig.runpod.exposure_fails = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("unauthenticated request"), "{err}");
    assert_eq!(rig.runpod.world.endpoint_ids().len(), 0);
    let stops = stops_for(&project, DEP);
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].app_id.as_deref(), Some("ep1"));
    // A later destroy confirms it is gone and drops the record.
    rig.runpod.exposure_fails = false;
    destroy(&project, &rig).unwrap();
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn a_failed_readiness_wait_tears_down_a_new_endpoint_and_keeps_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let rig = rig();
    rig.runpod.world.state.lock().unwrap().health = 401;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("torn down"), "{err}");
    assert_eq!(rig.runpod.world.endpoint_ids().len(), 0);
    let secrets = rig.runpod.world.secret_names();
    assert_eq!(
        secrets,
        [amicu_runpod::ACCOUNT_MARKER_SECRET],
        "{secrets:?}"
    );
    let stops = stops_for(&project, DEP);
    assert_eq!(stops[0].app_id.as_deref(), Some("ep1"));
    assert!(stops[0].secrets.is_empty(), "{stops:?}");
    let state = project.load_state().unwrap().unwrap();
    assert!(state.pending.iter().any(|d| d.as_str() == DEP));
}

#[test]
fn a_failed_readiness_wait_leaves_an_already_recorded_endpoint_in_place() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &[])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    rig.runpod.world.state.lock().unwrap().health = 401;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("left in place"), "{err}");
    assert_eq!(rig.runpod.world.endpoint_ids(), ["ep7"]);
    let stops = stops_for(&project, DEP);
    assert_eq!(stops[0].app_id.as_deref(), Some("ep7"));
}

#[test]
fn a_redeploy_retires_the_recorded_secrets_and_forgets_them() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &["amicu-old"])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    rig.runpod
        .world
        .state
        .lock()
        .unwrap()
        .secrets
        .push(("s5".to_owned(), "amicu-old".to_owned()));
    apply_with(&project, &rig).unwrap();
    assert!(
        !rig.runpod
            .world
            .secret_names()
            .contains(&"amicu-old".to_owned())
    );
    assert!(
        !stops_for(&project, DEP)[0]
            .secrets
            .contains(&"amicu-old".to_owned())
    );
}

#[test]
fn a_failed_start_keeps_every_recorded_secret() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &["amicu-old"])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    rig.runpod.world.state.lock().unwrap().health = 401;
    apply_with(&project, &rig).unwrap_err();
    // The endpoint was switched over, so the old secret is gone, but a
    // record keeps listing what it cannot prove deleted.
    assert!(
        stops_for(&project, DEP)[0]
            .secrets
            .contains(&"amicu-old".to_owned())
    );
}

#[test]
fn destroy_deletes_the_endpoint_and_its_secrets_then_drops_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &["amicu-x"])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    rig.runpod
        .world
        .state
        .lock()
        .unwrap()
        .secrets
        .push(("s5".to_owned(), "amicu-x".to_owned()));
    destroy(&project, &rig).unwrap();
    assert_eq!(rig.runpod.world.endpoint_ids().len(), 0);
    assert_eq!(
        rig.runpod.world.secret_names(),
        [amicu_runpod::ACCOUNT_MARKER_SECRET]
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn destroy_of_an_endpoint_already_gone_in_the_same_account_is_confirmed() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &[])));
    destroy(&project, &rig()).unwrap();
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn destroy_with_a_key_of_another_account_keeps_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), "personal:s99", &[])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("personal:s99"), "{err}");
    assert_eq!(rig.runpod.world.endpoint_ids(), ["ep7"]);
    assert_eq!(stops_for(&project, DEP).len(), 1);
}

#[test]
fn an_unconfirmed_delete_keeps_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &[])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    rig.runpod.world.state.lock().unwrap().delete_fails = true;
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("kept in state"), "{err}");
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("ep7"));
}

fn add_secret(rig: &Rig, id: &str, name: &str) {
    rig.runpod
        .world
        .state
        .lock()
        .unwrap()
        .secrets
        .push((id.to_owned(), name.to_owned()));
}

#[test]
fn an_ambiguous_create_adopts_the_endpoint_by_name_without_a_second_create() {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let rig = rig();
    rig.runpod.world.state.lock().unwrap().lose_create_response = true;
    apply_with(&project, &rig).unwrap();
    let posts = rig
        .runpod
        .world
        .writes()
        .into_iter()
        .filter(|w| w == "Post /serverless")
        .count();
    assert_eq!(posts, 1);
    assert_eq!(rig.runpod.world.endpoint_ids(), ["ep1"]);
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("ep1"));
}

#[test]
fn an_ambiguous_create_that_then_fails_readiness_is_torn_down_as_created() {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let rig = rig();
    {
        let mut w = rig.runpod.world.state.lock().unwrap();
        w.lose_create_response = true;
        w.health = 401;
    }
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("created by this run, was torn down"), "{err}");
    assert_eq!(rig.runpod.world.endpoint_ids().len(), 0);
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("ep1"));
}

#[test]
fn a_failed_exposure_check_leaves_an_adopted_endpoint_in_place() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(None, ACCOUNT, &[])));
    let mut rig = rig();
    add_endpoint(&rig, "ep9", Some(owner()));
    rig.runpod.exposure_fails = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("existed before this run"), "{err}");
    assert!(err.contains("scaled to zero workers"), "{err}");
    assert!(err.contains("unauthenticated request"), "{err}");
    assert_eq!(rig.runpod.world.endpoint_ids(), ["ep9"]);
    assert_eq!(
        rig.runpod.world.state.lock().unwrap().endpoints[0].max_workers,
        Some(0)
    );
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("ep9"));
}

#[test]
fn the_management_key_is_refused_as_the_endpoint_api_key() {
    struct ManagementKey;
    #[async_trait::async_trait]
    impl SecretResolver for ManagementKey {
        async fn resolve(&self, _: &SecretRef) -> Result<SecretValue, AdapterError> {
            Ok(SecretValue::new(MANAGEMENT_KEY.to_owned()))
        }
    }
    let td = TempDir::new();
    let project = project_with(&td, None);
    let mut rig = rig();
    rig.resolver = Box::new(ManagementKey);
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("management key"), "{err}");
    assert!(err.contains("restricted to invoking"), "{err}");
    assert!(!err.contains(MANAGEMENT_KEY), "{err}");
    assert_eq!(rig.runpod.world.log().len(), 0);
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn a_failed_secret_delete_on_destroy_keeps_the_record_with_what_remains() {
    let td = TempDir::new();
    let project = project_with(
        &td,
        Some(record(Some("ep7"), ACCOUNT, &["amicu-x", "amicu-y"])),
    );
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    add_secret(&rig, "s5", "amicu-x");
    add_secret(&rig, "s6", "amicu-y");
    rig.runpod.world.state.lock().unwrap().secret_delete_fails = true;
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("amicu-x") && err.contains("amicu-y"), "{err}");
    assert!(err.contains("kept in state"), "{err}");
    assert_eq!(rig.runpod.world.endpoint_ids().len(), 0);
    let stops = stops_for(&project, DEP);
    assert_eq!(stops[0].app_id.as_deref(), Some("ep7"));
    assert_eq!(stops[0].secrets, ["amicu-x", "amicu-y"]);
    // A retry once RunPod accepts the deletes confirms it and drops it.
    rig.runpod.world.state.lock().unwrap().secret_delete_fails = false;
    destroy(&project, &rig).unwrap();
    assert_eq!(stops_for(&project, DEP).len(), 0);
    assert_eq!(
        rig.runpod.world.secret_names(),
        [amicu_runpod::ACCOUNT_MARKER_SECRET]
    );
}

#[test]
fn a_gone_endpoint_with_leftover_secrets_deletes_them_then_drops_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &["amicu-x"])));
    let rig = rig();
    add_secret(&rig, "s5", "amicu-x");
    destroy(&project, &rig).unwrap();
    assert_eq!(
        rig.runpod.world.secret_names(),
        [amicu_runpod::ACCOUNT_MARKER_SECRET]
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn a_gone_endpoint_whose_secrets_cannot_be_deleted_keeps_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &["amicu-x"])));
    let rig = rig();
    add_secret(&rig, "s5", "amicu-x");
    rig.runpod.world.state.lock().unwrap().secret_delete_fails = true;
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("amicu-x"), "{err}");
    assert_eq!(stops_for(&project, DEP)[0].secrets, ["amicu-x"]);
}

#[test]
fn destroy_without_an_endpoint_id_deletes_the_owned_endpoint_found_by_name() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(None, ACCOUNT, &["amicu-x"])));
    let rig = rig();
    add_endpoint(&rig, "ep9", Some(owner()));
    add_secret(&rig, "s5", "amicu-x");
    destroy(&project, &rig).unwrap();
    assert_eq!(rig.runpod.world.endpoint_ids().len(), 0);
    assert_eq!(
        rig.runpod.world.secret_names(),
        [amicu_runpod::ACCOUNT_MARKER_SECRET]
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn destroy_without_an_endpoint_id_and_no_endpoint_deletes_the_secrets() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(None, ACCOUNT, &["amicu-x"])));
    let rig = rig();
    add_secret(&rig, "s5", "amicu-x");
    destroy(&project, &rig).unwrap();
    assert_eq!(
        rig.runpod.world.secret_names(),
        [amicu_runpod::ACCOUNT_MARKER_SECRET]
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn destroy_without_an_endpoint_id_and_a_secret_delete_failure_keeps_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(None, ACCOUNT, &["amicu-x"])));
    let rig = rig();
    add_secret(&rig, "s5", "amicu-x");
    rig.runpod.world.state.lock().unwrap().secret_delete_fails = true;
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(
        err.contains("no endpoint exists") && err.contains("amicu-x"),
        "{err}"
    );
    assert_eq!(stops_for(&project, DEP)[0].secrets, ["amicu-x"]);
}

#[test]
fn destroy_without_an_endpoint_id_refuses_an_endpoint_of_another_owner() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(None, ACCOUNT, &["amicu-x"])));
    let mut rig = rig();
    add_endpoint(&rig, "ep9", Some("someone-else".to_owned()));
    add_secret(&rig, "s5", "amicu-x");
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("AMICU_OWNER"), "{err}");
    assert!(err.contains("--forget-unconfirmed"), "{err}");
    assert_eq!(rig.runpod.world.endpoint_ids(), ["ep9"]);
    assert_eq!(stops_for(&project, DEP).len(), 1);
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());

    // Forgetting leaves the foreign endpoint alone but deletes the secrets.
    rig.forget = true;
    destroy(&project, &rig).unwrap();
    assert_eq!(rig.runpod.world.endpoint_ids(), ["ep9"]);
    assert_eq!(
        rig.runpod.world.secret_names(),
        [amicu_runpod::ACCOUNT_MARKER_SECRET]
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn a_deleted_account_marker_is_an_actionable_error_and_forget_drops_the_record() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &["amicu-x"])));
    let mut rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    rig.runpod.world.state.lock().unwrap().secrets.clear();
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains(amicu_runpod::ACCOUNT_MARKER_SECRET), "{err}");
    assert!(err.contains("ep7") && err.contains("amicu-x"), "{err}");
    assert!(err.contains("--forget-unconfirmed"), "{err}");
    assert_eq!(stops_for(&project, DEP).len(), 1);

    rig.forget = true;
    destroy(&project, &rig).unwrap();
    assert_eq!(stops_for(&project, DEP).len(), 0);
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

#[test]
fn the_write_ahead_keeps_unknown_fields_of_the_existing_record() {
    let td = TempDir::new();
    let mut rec = record(Some("ep7"), ACCOUNT, &[]);
    rec.extra.insert("future".to_owned(), json!(1));
    let project = project_with(&td, Some(rec));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    apply_with(&project, &rig).unwrap();
    assert_eq!(stops_for(&project, DEP)[0].extra["future"], json!(1));
}

#[test]
fn a_failed_endpoint_delete_keeps_every_recorded_secret() {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &["amicu-x"])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    add_secret(&rig, "s5", "amicu-x");
    rig.runpod.world.state.lock().unwrap().delete_fails = true;
    destroy(&project, &rig).unwrap_err();
    assert_eq!(stops_for(&project, DEP)[0].secrets, ["amicu-x"]);

    rig.runpod.world.state.lock().unwrap().delete_fails = false;
    destroy(&project, &rig).unwrap();
    assert_eq!(rig.runpod.world.endpoint_ids().len(), 0);
    assert_eq!(
        rig.runpod.world.secret_names(),
        [amicu_runpod::ACCOUNT_MARKER_SECRET]
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

// --- amicu runpod status / logs / metrics / stop -----------------------------

fn running_endpoint(rig: &Rig) -> (TempDir, Project) {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &[])));
    add_endpoint(rig, "ep7", Some(owner()));
    (td, project)
}

#[test]
fn status_shows_the_endpoint_workers_gpu_and_data_centers() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    rig.runpod.world.state.lock().unwrap().workers = vec![
        ("w1".into(), "RUNNING".into()),
        ("w22".into(), "IDLE".into()),
    ];
    let mut out = Vec::new();
    let code =
        commands::runpod::status(&project, DEP, OutputFormat::Human, &mut out, &rig.env()).unwrap();
    assert_eq!(code, 0);
    let text = utf8(out);
    assert!(text.contains("RunPod endpoint ep7"), "{text}");
    assert!(text.contains("https://ep7.api.runpod.ai"), "{text}");
    assert!(
        text.contains("running=1 idle=1 initializing=0 throttled=0 unhealthy=0 (min 0, max 1)"),
        "{text}"
    );
    assert!(text.contains("gpu:          AMPERE_24 x1"), "{text}");
    assert!(text.contains("data centers: EU-RO-1, EU-SE-1"), "{text}");

    let mut out = Vec::new();
    commands::runpod::status(&project, DEP, OutputFormat::Json, &mut out, &rig.env()).unwrap();
    let doc: Value = serde_json::from_str(&utf8(out)).unwrap();
    assert_eq!(doc["endpoint_id"], "ep7");
    assert_eq!(doc["workers"]["running"], 1);
    assert_eq!(doc["data_centers"], json!(["EU-RO-1", "EU-SE-1"]));
}

#[test]
fn status_reports_scaled_to_zero_and_refuses_unknown_or_unrecorded_deployments() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    let mut out = Vec::new();
    commands::runpod::status(&project, DEP, OutputFormat::Human, &mut out, &rig.env()).unwrap();
    assert!(utf8(out).contains("scaled to zero"));

    let err = commands::runpod::status(
        &project,
        "nope",
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no RunPod deployment named 'nope'"), "{err}");

    let td = TempDir::new();
    let unrecorded = project_with(&td, Some(record(None, ACCOUNT, &[])));
    let err = commands::runpod::status(
        &unrecorded,
        DEP,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no RunPod endpoint id recorded"), "{err}");
}

#[test]
fn status_of_an_endpoint_gone_remotely_says_so() {
    let rig = rig();
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &[])));
    let err = commands::runpod::status(
        &project,
        DEP,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("ep7") && err.contains("not found"), "{err}");
}

#[test]
fn logs_merge_every_worker_with_its_prefix() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    rig.runpod.world.state.lock().unwrap().workers = vec![
        ("w22".into(), "RUNNING".into()),
        ("w1".into(), "IDLE".into()),
    ];
    let mut out = Vec::new();
    commands::runpod::logs(&project, DEP, Some(20), &mut out, &rig.env()).unwrap();
    let text = utf8(out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "[w1] hello from w1",
            "[w22] hello from w22",
            "[w22] (system) done",
            "[w1] (system) done"
        ],
        "{text}"
    );
    let log = rig.runpod.world.log();
    assert!(
        log.contains(&"Get /serverless/ep7/workers/w1/logs?tail=20".to_owned()),
        "{log:?}"
    );
}

#[test]
fn logs_of_a_scaled_to_zero_endpoint_say_there_are_no_workers() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    let mut out = Vec::new();
    let code = commands::runpod::logs(&project, DEP, None, &mut out, &rig.env()).unwrap();
    assert_eq!(code, 0);
    assert!(utf8(out).contains("no RunPod workers are running"));
}

#[test]
fn metrics_go_through_the_gateway_with_the_deployment_key() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    rig.runpod.world.state.lock().unwrap().metrics =
        "vllm:num_requests_running 3\nvllm:generation_tokens_total 10\n".to_owned();
    let mut out = Vec::new();
    commands::runpod::metrics(
        &project,
        DEP,
        false,
        5,
        OutputFormat::Human,
        &mut out,
        &rig.env(),
    )
    .unwrap();
    let text = utf8(out);
    assert!(text.contains(&format!("metrics {DEP} (vLLM)")), "{text}");
    assert!(text.contains("running=3"), "{text}");
    assert!(
        text.contains("hardware: n/a (not available on RunPod)"),
        "{text}"
    );
    let log = rig.runpod.world.log();
    assert!(
        log.contains(&"metrics ep7 bearer=rpa_ENDPOINTKEY".to_owned()),
        "{log:?}"
    );

    let mut out = Vec::new();
    commands::runpod::metrics(
        &project,
        DEP,
        false,
        5,
        OutputFormat::Json,
        &mut out,
        &rig.env(),
    )
    .unwrap();
    let doc: Value = serde_json::from_str(&utf8(out)).unwrap();
    assert_eq!(doc["runtime"]["requests_running"], 3);
    assert_eq!(doc["hardware"]["containers"], json!([]));
}

#[test]
fn stop_deletes_the_endpoint_then_drops_the_record() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    commands::runpod::stop(
        &project,
        DEP,
        None,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap();
    assert_eq!(rig.runpod.world.endpoint_ids().len(), 0);
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn an_unconfirmed_stop_keeps_the_record() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    rig.runpod.world.state.lock().unwrap().delete_fails = true;
    let err = commands::runpod::stop(
        &project,
        DEP,
        None,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("kept in state"), "{err}");
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("ep7"));
}

#[test]
fn stop_refuses_a_deployment_without_a_runpod_record() {
    let rig = rig();
    let td = TempDir::new();
    let project = project_with(&td, None);
    let err = commands::runpod::stop(
        &project,
        DEP,
        None,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no RunPod deployment named"), "{err}");
    assert_eq!(rig.runpod.world.log().len(), 0);
}
