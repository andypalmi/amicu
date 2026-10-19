//! `provider = "runpod"` through apply, destroy, refresh and orphans,
//! against an in-memory RunPod v2 API.

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

#[derive(Clone)]
struct PodRec {
    id: String,
    /// The create body, with PATCHes merged in.
    body: Value,
    status: &'static str,
    started: u32,
}

fn pod_json(p: &PodRec) -> Value {
    let mut v = p.body.clone();
    let m = v.as_object_mut().unwrap();
    m.insert("id".into(), json!(p.id));
    m.insert("status".into(), json!(p.status));
    m.insert("startedAt".into(), json!(format!("t{}", p.started)));
    v
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct WorldState {
    /// `Some`: an organization account with this id.
    org: Option<String>,
    secrets: Vec<(String, String)>,
    endpoints: Vec<Ep>,
    pods: Vec<PodRec>,
    /// Status a new or (re)started pod reports (default `RUNNING`).
    pod_status: Option<&'static str>,
    /// Status the pod proxy answers an anonymous request with (0: 401).
    anon: u16,
    /// Pod routes that answer an anonymous request with 200.
    open_routes: Vec<&'static str>,
    next: u32,
    next_pod: u32,
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
    /// Body of every endpoint create.
    created: Vec<Value>,
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
    let mut ep = json!({"id": e.id, "name": e.name, "type": "LOAD_BALANCER", "env": env,
        "gpu": {"pools": ["AMPERE_24"], "count": 1},
        "dataCenterIds": ["EU-RO-1", "EU-SE-1"]});
    if let Some(max) = e.max_workers {
        ep["workers"] = json!({"min": 0, "max": max});
    }
    ep
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
        if let Some(host) = req
            .url
            .strip_prefix("https://")
            .and_then(|u| u.strip_suffix(".proxy.runpod.net/metrics"))
        {
            let mut w = self.state.lock().unwrap();
            w.log.push(format!(
                "proxy metrics {host} bearer={}",
                req.bearer.as_deref().unwrap_or("")
            ));
            return Ok(HttpResponse::new(200, w.metrics.clone().into_bytes()));
        }
        if let Some((id, path)) = req.url.strip_prefix("https://").and_then(|u| {
            u.split_once("-8080.proxy.runpod.net")
                .or_else(|| u.split_once("-8000.proxy.runpod.net"))
        }) {
            let mut w = self.state.lock().unwrap();
            w.log.push(format!("proxy {:?} {id}{path}", req.method));
            let status = match path {
                "/health" if w.health != 0 => w.health,
                p if req.bearer.is_none() && w.open_routes.contains(&p) => 200,
                "/v1/chat/completions" | "/invocations" | "/docs" if req.bearer.is_none() => {
                    if w.anon == 0 { 401 } else { w.anon }
                }
                _ => 200,
            };
            return reply(status, &json!({}));
        }
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
        if req.method == HttpMethod::Post && (path == "/serverless" || path == "/pods") {
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
                w.created.extend(req.body.clone());
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
            (HttpMethod::Get, ["pods"]) => {
                let pods: Vec<Value> = w.pods.iter().map(pod_json).collect();
                reply(
                    200,
                    &json!({"pods": pods, "pagination": {"hasNextPage": false}}),
                )
            }
            (HttpMethod::Post, ["pods"]) => {
                w.next_pod += 1;
                let pod = PodRec {
                    id: format!("pod{}", w.next_pod),
                    body: req.body.clone().unwrap(),
                    status: w.pod_status.unwrap_or("RUNNING"),
                    started: 0,
                };
                w.pods.push(pod.clone());
                if w.lose_create_response {
                    return Err(TransportError::Timeout);
                }
                reply(201, &pod_json(&pod))
            }
            (HttpMethod::Post, ["pods", id, "action"]) => {
                let action = req.body.as_ref().unwrap()["action"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                w.log.push(format!("action {id} {action}"));
                let Some(i) = w.pods.iter().position(|p| p.id == *id) else {
                    return reply(404, &json!({"detail": "not found"}));
                };
                match action.as_str() {
                    "terminate" if w.delete_fails => {
                        reply(500, &json!({"detail": "internal error"}))
                    }
                    "terminate" => {
                        w.pods.remove(i);
                        Ok(HttpResponse::new(204, Vec::new()))
                    }
                    _ => {
                        let status = w.pod_status.unwrap_or("RUNNING");
                        let p = &mut w.pods[i];
                        p.started += 1;
                        p.status = status;
                        let body = pod_json(p);
                        reply(200, &body)
                    }
                }
            }
            (HttpMethod::Get, ["pods", id, logs]) if logs.starts_with("logs?tail=") => {
                if !w.pods.iter().any(|p| p.id == *id) {
                    return reply(404, &json!({"detail": "not found"}));
                }
                let body = format!(
                    "data: {}\n\ndata: {}\n\n",
                    json!({"ts": "2026-01-01T00:00:02Z", "source": "system", "line": "pulled"}),
                    json!({"ts": "2026-01-01T00:00:05Z", "source": "container", "line": format!("hello from {id}")}),
                );
                Ok(HttpResponse::new(200, body.into_bytes()))
            }
            (method, ["pods", id]) => {
                let Some(i) = w.pods.iter().position(|p| p.id == *id) else {
                    return reply(404, &json!({"detail": "not found"}));
                };
                if method == HttpMethod::Patch {
                    let update = req.body.clone().unwrap();
                    let body = w.pods[i].body.as_object_mut().unwrap();
                    for (k, v) in update.as_object().unwrap() {
                        body.insert(k.clone(), v.clone());
                    }
                }
                reply(200, &pod_json(&w.pods[i]))
            }
            (HttpMethod::Get, ["catalog", "gpus"]) => reply(
                200,
                &json!({"gpus": [{"id": "NVIDIA L4", "name": "L4", "pool": "AMPERE_24", "memory": 24}]}),
            ),
            (HttpMethod::Get, ["account", "secrets"]) => {
                let all: Vec<Value> = w
                    .secrets
                    .iter()
                    .map(|(id, n)| json!({"id": id, "name": n}))
                    .collect();
                reply(200, &json!({"secrets": all}))
            }
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
    add_named_endpoint(rig, id, &endpoint_name(), owner);
}

fn add_named_endpoint(rig: &Rig, id: &str, name: &str, owner: Option<String>) {
    rig.runpod.world.state.lock().unwrap().endpoints.push(Ep {
        id: id.to_owned(),
        name: name.to_owned(),
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

/// A project whose state holds `record` for a confirmed (not pending)
/// deployment, as after a successful apply.
fn applied_with(td: &TempDir, record: StopRecord) -> Project {
    let project = project_with(td, Some(record));
    let mut state = project.load_state().unwrap().unwrap();
    state.pending.clear();
    project.save_state(&state).unwrap();
    project
}

fn refresh(project: &Project, rig: &Rig) -> (crate::refresh::Refresh, LocalState) {
    let mut state = project.load_state().unwrap().unwrap();
    let refresh = crate::refresh::refresh_state_mode(&mut state, &rig.modal, &rig.runpod, true);
    (refresh, state)
}

#[test]
fn refresh_keeps_a_live_endpoint_without_notes() {
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), ACCOUNT, &[]));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r, crate::refresh::Refresh::default());
    assert_eq!(state.pending.len(), 0);
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

#[test]
fn refresh_marks_an_endpoint_gone_in_the_recorded_account_pending() {
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), ACCOUNT, &["amicu-x"]));
    let rig = rig();
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 1, "{r:?}");
    assert!(
        r.notes[0].contains("ep7") && r.notes[0].contains(ACCOUNT),
        "{r:?}"
    );
    assert!(r.notes[0].contains("kept in state as pending"), "{r:?}");
    assert!(state.pending.iter().any(|d| d.as_str() == DEP));
    // The stop identity is never dropped.
    assert_eq!(state.stops, [record(Some("ep7"), ACCOUNT, &["amicu-x"])]);
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

#[test]
fn refresh_never_calls_an_endpoint_gone_from_another_account() {
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), "personal:s99", &[]));
    let rig = rig();
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 0, "{r:?}");
    let warning = r.warning.unwrap();
    assert!(
        warning.contains("personal:s99") && warning.contains(ACCOUNT),
        "{warning}"
    );
    assert!(
        warning.contains("cannot tell whether it is gone"),
        "{warning}"
    );
    assert_eq!(state.pending.len(), 0);
}

#[test]
fn refresh_with_an_unknown_account_never_calls_an_endpoint_gone() {
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), ACCOUNT, &[]));
    let rig = rig();
    rig.runpod.world.state.lock().unwrap().secrets.clear();
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 0, "{r:?}");
    assert!(r.warning.unwrap().contains("account amicu has not seen"));
    assert_eq!(state.pending.len(), 0);
    // Reading the identity never creates the marker.
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

#[test]
fn refresh_without_a_management_key_warns_and_keeps_state() {
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), ACCOUNT, &[]));
    let mut rig = rig();
    rig.runpod.no_key = true;
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 0);
    assert!(r.warning.unwrap().contains("RUNPOD_API_KEY"));
    assert_eq!(state.pending.len(), 0);
    assert_eq!(rig.runpod.world.log().len(), 0);
}

#[test]
fn refresh_finds_an_id_less_record_by_name_and_owner_marker() {
    let td = TempDir::new();
    let project = applied_with(&td, record(None, ACCOUNT, &[]));
    let rig = rig();
    add_endpoint(&rig, "ep9", Some(owner()));
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r, crate::refresh::Refresh::default());
    assert_eq!(state.pending.len(), 0);
}

#[test]
fn refresh_never_takes_a_foreign_endpoint_for_an_id_less_record() {
    let td = TempDir::new();
    let project = applied_with(&td, record(None, ACCOUNT, &[]));
    let rig = rig();
    add_endpoint(&rig, "ep9", Some("someone-else".to_owned()));
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 0, "{r:?}");
    let warning = r.warning.unwrap();
    assert!(
        warning.contains("ep9") && warning.contains("AMICU_OWNER"),
        "{warning}"
    );
    assert_eq!(state.pending.len(), 0);
}

#[test]
fn refresh_marks_an_id_less_record_with_no_endpoint_pending() {
    let td = TempDir::new();
    let project = applied_with(&td, record(None, ACCOUNT, &[]));
    let rig = rig();
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 1, "{r:?}");
    assert!(state.pending.iter().any(|d| d.as_str() == DEP));
}

#[test]
fn refresh_reports_drift_of_a_live_endpoint_without_touching_it() {
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), ACCOUNT, &[]));
    let rig = rig();
    rig.runpod.world.state.lock().unwrap().endpoints.push(Ep {
        id: "ep7".to_owned(),
        name: endpoint_name(),
        owner: Some(owner()),
        max_workers: Some(7),
    });
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 1, "{r:?}");
    let note = &r.notes[0];
    assert!(
        note.contains("RunPod endpoint") && note.contains("ep7"),
        "{note}"
    );
    assert!(note.contains("workers 0-7 (applied 0-1)"), "{note}");
    assert!(note.contains("revert it in the RunPod console"), "{note}");
    assert_eq!(state.pending.len(), 0);
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

#[test]
fn plan_after_an_endpoint_is_gone_shows_a_create() {
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), ACCOUNT, &[]));
    let rig = rig();
    let text = plan_text(&project, &rig);
    assert!(
        text.contains("drift: deployment support_local: RunPod endpoint"),
        "{text}"
    );
    assert!(
        text.split("|pending=").nth(1).unwrap().contains(DEP),
        "{text}"
    );
}

struct NoModal;

impl crate::commands::orphans::ModalApps for NoModal {
    fn list(&self) -> Result<Vec<crate::commands::orphans::ModalApp>, String> {
        Ok(Vec::new())
    }
}

fn orphans(project: &Project, rig: &Rig) -> crate::commands::orphans::OrphansReport {
    use crate::commands::orphans::{Env, OpsListing, Platform, Tracked, discover};
    let state = project.load_state().unwrap();
    let tracked =
        Tracked::from_state(state.as_ref()).with_config(project.load_graph().ok().as_ref());
    discover(
        &Env {
            platform: Platform::Other,
            exec: &amicu_llamacpp::HostExec,
            modal: &NoModal,
            runpod: &OpsListing(&rig.runpod),
            agents_dir: None,
        },
        &tracked,
    )
}

#[test]
fn orphans_lists_untracked_owned_endpoints_and_amicu_secrets_read_only() {
    use crate::commands::orphans::{Kind, Ownership};
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), ACCOUNT, &["amicu-x"]));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    let stale = amicu_runpod::project_endpoint_name(PROJECT, "old");
    add_named_endpoint(&rig, "ep8", &stale, Some("marker".to_owned()));
    let foreign = amicu_runpod::project_endpoint_name("bbbb2222", "chat");
    add_named_endpoint(&rig, "ep9", &foreign, Some("marker".to_owned()));
    add_named_endpoint(&rig, "ep10", "not-amicu", None);
    add_secret(&rig, "s5", "amicu-x");
    add_secret(&rig, "s6", &format!("amicu-{PROJECT}-old-api-key-1-2"));
    add_secret(&rig, "s7", "hf-token");

    let report = orphans(&project, &rig);
    let runpod = report.sources.iter().find(|s| s.name == "runpod").unwrap();
    assert_eq!(runpod.status, "ok", "{:?}", runpod.detail);
    let untracked: Vec<(Kind, &str, Ownership)> = report
        .resources
        .iter()
        .filter(|r| !r.tracked)
        .map(|r| (r.kind, r.identity.as_str(), r.ownership))
        .collect();
    let stale_id = format!("{stale} (ep8)");
    let foreign_id = format!("{foreign} (ep9)");
    let secret_id = format!("amicu-{PROJECT}-old-api-key-1-2 (s6)");
    assert_eq!(
        untracked,
        [
            (
                Kind::RunpodEndpoint,
                stale_id.as_str(),
                Ownership::ThisProject
            ),
            (
                Kind::RunpodSecret,
                secret_id.as_str(),
                Ownership::ThisProject
            ),
            (
                Kind::RunpodEndpoint,
                foreign_id.as_str(),
                Ownership::OtherProject
            ),
        ]
    );
    let tracked: Vec<&str> = report
        .resources
        .iter()
        .filter(|r| r.tracked)
        .map(|r| r.identity.as_str())
        .collect();
    assert_eq!(tracked.len(), 2, "{tracked:?}");
    assert!(
        !report
            .resources
            .iter()
            .any(|r| r.identity.contains(amicu_runpod::ACCOUNT_MARKER_SECRET)),
        "the account marker is never listed"
    );
    let stop = &report.resources[0].stop_command;
    assert!(
        stop.contains("DELETE") && stop.ends_with("/v2/serverless/ep8"),
        "{stop}"
    );
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

#[test]
fn orphans_skips_runpod_for_a_project_that_does_not_use_it() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let rig = rig();
    let report = orphans(&project, &rig);
    let runpod = report.sources.iter().find(|s| s.name == "runpod").unwrap();
    assert_eq!(runpod.status, "skipped");
    assert_eq!(rig.runpod.world.log().len(), 0);
}

#[test]
fn orphans_reports_a_runpod_listing_failure_as_a_source_error() {
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), ACCOUNT, &[]));
    let mut rig = rig();
    rig.runpod.no_key = true;
    let report = orphans(&project, &rig);
    let runpod = report.sources.iter().find(|s| s.name == "runpod").unwrap();
    assert_eq!(runpod.status, "error");
    assert!(runpod.detail.as_deref().unwrap().contains("RUNPOD_API_KEY"));
}

#[test]
fn orphans_after_apply_finds_no_untracked_runpod_resource() {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    let report = orphans(&project, &rig);
    let untracked: Vec<&str> = report
        .resources
        .iter()
        .filter(|r| !r.tracked)
        .map(|r| r.identity.as_str())
        .collect();
    assert_eq!(untracked, Vec::<&str>::new());
    assert!(
        report
            .resources
            .iter()
            .any(|r| r.identity.ends_with("(ep1)"))
    );
}

#[test]
fn orphans_after_a_pod_apply_lists_no_untracked_secret() {
    let td = TempDir::new();
    let project = pod_project(&td, None);
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    let report = orphans(&project, &rig);
    let untracked: Vec<&str> = report
        .resources
        .iter()
        .filter(|r| !r.tracked)
        .map(|r| r.identity.as_str())
        .collect();
    assert_eq!(untracked, Vec::<&str>::new());
}

#[test]
fn orphans_shows_a_same_named_duplicate_of_a_recorded_endpoint_as_untracked() {
    let td = TempDir::new();
    let project = applied_with(&td, record(Some("ep7"), ACCOUNT, &[]));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    add_endpoint(&rig, "ep8", Some(owner()));
    let report = orphans(&project, &rig);
    let untracked: Vec<&str> = report
        .resources
        .iter()
        .filter(|r| !r.tracked)
        .map(|r| r.identity.as_str())
        .collect();
    assert_eq!(untracked, [format!("{} (ep8)", endpoint_name())]);
}

// --- pod mode ---------------------------------------------------------------

fn pod_config(extra: &str) -> String {
    runpod_config().replace(
        "  memory = \"32GiB\"\n",
        &format!("  memory = \"32GiB\"\n  runpod {{\n    mode = \"pod\"\n{extra}  }}\n"),
    )
}

fn pod_project(td: &TempDir, record: Option<StopRecord>) -> Project {
    let project = project_with(td, record);
    write(&td.path, "amicu.hcl", &pod_config(""));
    if project.load_state().unwrap().unwrap().stops.is_empty() {
        return project;
    }
    let mut state = project.load_state().unwrap().unwrap();
    state.graph = project.load_graph().unwrap();
    project.save_state(&state).unwrap();
    project
}

fn pod_record(id: Option<&str>, account: &str, secrets: &[&str]) -> StopRecord {
    StopRecord::runpod_pod_in(
        PROJECT,
        &DEP.into(),
        id.map(str::to_owned),
        Some(account),
        secrets.iter().map(|s| (*s).to_owned()).collect(),
    )
}

/// A live pod named like this deployment's, with `gpu` and `owner`.
fn add_pod(rig: &Rig, id: &str, gpu: &str, owner: Option<String>) {
    let mut env = serde_json::Map::new();
    if let Some(o) = owner {
        env.insert(amicu_runpod::OWNER_ENV_VAR.to_owned(), json!(o));
    }
    rig.runpod.world.state.lock().unwrap().pods.push(PodRec {
        id: id.to_owned(),
        body: json!({"name": endpoint_name(), "gpu": {"id": gpu, "count": 1},
            "cloud": "SECURE", "env": env}),
        status: "RUNNING",
        started: 0,
    });
}

fn pod_ids(rig: &Rig) -> Vec<String> {
    let w = rig.runpod.world.state.lock().unwrap();
    w.pods.iter().map(|p| p.id.clone()).collect()
}

/// Pod writes in order: creates, PATCHes and actions.
fn pod_writes(rig: &Rig) -> Vec<String> {
    rig.runpod
        .world
        .log()
        .into_iter()
        .filter(|l| l == "Post /pods" || l.starts_with("Patch /pods") || l.starts_with("action"))
        .collect()
}

#[test]
fn pod_apply_records_the_pod_ahead_then_its_id_and_secrets() {
    let td = TempDir::new();
    let project = pod_project(&td, None);
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
        .expect("recorded before create");
    assert_eq!(ahead.backend, Backend::RunpodPod);
    assert_eq!(ahead.identity, endpoint_name());
    assert_eq!(ahead.app_id, None);
    assert_eq!(ahead.workspace.as_deref(), Some(ACCOUNT));
    assert!(!ahead.secrets.is_empty(), "{ahead:?}");

    let stops = stops_for(&project, DEP);
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].backend, Backend::RunpodPod);
    assert_eq!(stops[0].app_id.as_deref(), Some("pod1"));
    assert_eq!(stops[0].secrets, ahead.secrets);
    let state = project.load_state().unwrap().unwrap();
    assert_eq!(state.pending.len(), 0);
    assert_eq!(state.version, "v6");
    assert_eq!(
        state.endpoints[0].endpoint,
        "https://pod1-8080.proxy.runpod.net"
    );
    let log = rig.runpod.world.log();
    assert!(
        log.contains(&"proxy Post pod1/v1/chat/completions".to_owned()),
        "{log:?}"
    );
    let w = rig.runpod.world.state.lock().unwrap();
    assert_eq!(w.pods[0].body["cloud"], "SECURE");
    assert_eq!(w.pods[0].body["gpu"]["id"], "NVIDIA L4");
    assert!(w.endpoints.is_empty());
}

#[test]
fn pod_apply_honours_the_community_cloud() {
    let td = TempDir::new();
    let project = pod_project(&td, None);
    write(
        &td.path,
        "amicu.hcl",
        &pod_config("    cloud = \"community\"\n"),
    );
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    assert_eq!(
        rig.runpod.world.state.lock().unwrap().pods[0].body["cloud"],
        "COMMUNITY"
    );
}

#[test]
fn a_pod_created_before_a_crash_is_adopted_by_name_not_created_again() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(None, ACCOUNT, &[])));
    let rig = rig();
    add_pod(&rig, "pod9", "NVIDIA L4", Some(owner()));
    apply_with(&project, &rig).unwrap();
    let writes = pod_writes(&rig);
    assert!(!writes.contains(&"Post /pods".to_owned()), "{writes:?}");
    assert!(
        writes.contains(&"Patch /pods/pod9".to_owned()),
        "{writes:?}"
    );
    assert_eq!(pod_ids(&rig), ["pod9"]);
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("pod9"));
}

#[test]
fn a_rerun_against_a_matching_pod_writes_nothing() {
    let td = TempDir::new();
    let project = pod_project(&td, None);
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    let mut state = project.load_state().unwrap().unwrap();
    state.pending = vec![DEP.into()];
    project.save_state(&state).unwrap();
    rig.runpod.world.state.lock().unwrap().log.clear();

    apply_with(&project, &rig).unwrap();
    assert_eq!(pod_writes(&rig), Vec::<String>::new());
    assert_eq!(pod_ids(&rig), ["pod1"]);
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("pod1"));
}

#[test]
fn a_recorded_pod_on_another_gpu_is_terminated_then_replaced() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), ACCOUNT, &[])));
    let rig = rig();
    add_pod(&rig, "pod7", "NVIDIA A40", Some(owner()));
    apply_with(&project, &rig).unwrap();
    assert_eq!(
        pod_writes(&rig),
        ["action pod7 terminate", "Post /pods"],
        "terminated first, then created"
    );
    let ids = pod_ids(&rig);
    assert_eq!(ids.len(), 1);
    assert_ne!(ids[0], "pod7");
    let stops = stops_for(&project, DEP);
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].app_id.as_deref(), Some(ids[0].as_str()));
}

#[test]
fn a_switch_from_serverless_to_pod_deletes_the_endpoint_first() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(record(Some("ep7"), ACCOUNT, &[])));
    let rig = rig();
    add_endpoint(&rig, "ep7", Some(owner()));
    apply_with(&project, &rig).unwrap();
    assert_eq!(rig.runpod.world.endpoint_ids().len(), 0);
    let log = rig.runpod.world.log();
    let deleted = log.iter().position(|l| l == "Delete /serverless/ep7");
    let created = log.iter().position(|l| l == "Post /pods");
    assert!(deleted.is_some() && deleted < created, "{log:?}");
    let stops = stops_for(&project, DEP);
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].backend, Backend::RunpodPod);
}

#[test]
fn a_new_pod_that_fails_readiness_is_terminated_and_stays_recorded() {
    let td = TempDir::new();
    let project = pod_project(&td, None);
    let rig = rig();
    rig.runpod.world.state.lock().unwrap().pod_status = Some("EXITED");
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("created by this run, was terminated"), "{err}");
    assert_eq!(pod_ids(&rig).len(), 0);
    let stops = stops_for(&project, DEP);
    assert_eq!(stops[0].app_id.as_deref(), Some("pod1"));
    assert!(stops[0].secrets.is_empty(), "{stops:?}");
    // A later destroy confirms it is gone and drops the record.
    destroy(&project, &rig).unwrap();
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn a_recorded_pod_that_fails_readiness_is_left_in_place() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), ACCOUNT, &[])));
    let rig = rig();
    add_pod(&rig, "pod7", "NVIDIA L4", Some(owner()));
    rig.runpod.world.state.lock().unwrap().pod_status = Some("EXITED");
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("left in place"), "{err}");
    assert_eq!(pod_ids(&rig), ["pod7"]);
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("pod7"));
}

#[test]
fn an_open_pod_is_terminated_by_the_supervisor() {
    let td = TempDir::new();
    let project = pod_project(&td, None);
    let rig = rig();
    rig.runpod.world.state.lock().unwrap().anon = 200;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("open to anyone"), "{err}");
    assert_eq!(pod_ids(&rig).len(), 0);
    assert_eq!(stops_for(&project, DEP).len(), 1);
}

#[test]
fn a_pod_whose_models_route_is_public_is_kept() {
    let td = TempDir::new();
    let project = pod_project(&td, None);
    let mut rig = rig();
    // The generic check (anonymous GET /v1/models) would fail on llama-server.
    rig.runpod.exposure_fails = true;
    apply_with(&project, &rig).unwrap();
    assert_eq!(pod_ids(&rig), ["pod1"]);
    let log = rig.runpod.world.log();
    assert!(!log.iter().any(|l| l.starts_with("verify")), "{log:?}");
}

fn vllm_pod_project(td: &TempDir) -> Project {
    let project = pod_project(td, None);
    let config = pod_config("");
    let (head, deployment) = config.split_once("resource \"deployment\"").unwrap();
    assert!(deployment.contains("llama_cpp {"));
    write(
        &td.path,
        "amicu.hcl",
        &format!(
            r#"{head}
resource "model" "small" {{
  source {{
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-0.5B-Instruct"
  }}
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {{
    format       = "safetensors"
    quantization = "bf16"
  }}
  context_length = 8192
  n_layers       = 24
  n_kv_heads     = 2
  head_dim       = 64
}}

resource "deployment" "support_local" {{
  model    = model.small.id
  hardware = hardware_profile.runpod_l4.id
  runtime  = "vllm"
  api_key  = secret("RUNPOD_TEST_ENDPOINT_KEY")
  vllm {{
    context_length = 8192
    port           = 8000
  }}
}}
"#
        ),
    );
    project
}

#[test]
fn a_vllm_pod_apply_probes_the_routes_vllm_leaves_open() {
    let td = TempDir::new();
    let project = vllm_pod_project(&td);
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    assert_eq!(pod_ids(&rig), ["pod1"]);
    let log = rig.runpod.world.log();
    for probe in [
        "proxy Post pod1/v1/chat/completions",
        "proxy Post pod1/invocations",
        "proxy Get pod1/docs",
    ] {
        assert!(log.contains(&probe.to_owned()), "{probe}: {log:?}");
    }
    let w = rig.runpod.world.state.lock().unwrap();
    assert_eq!(w.pods[0].body["entrypoint"], json!(["/bin/sh", "-c"]));
    let script = w.pods[0].body["cmd"][0].as_str().unwrap();
    assert!(
        script.contains("--middleware amicu_auth.RequireKey"),
        "{script}"
    );
}

#[test]
fn a_vllm_pod_with_open_invocations_or_docs_is_terminated() {
    for route in ["/invocations", "/docs"] {
        let td = TempDir::new();
        let project = vllm_pod_project(&td);
        let rig = rig();
        rig.runpod.world.state.lock().unwrap().open_routes = vec![route];
        let err = apply_with(&project, &rig).unwrap_err().to_string();
        assert!(
            err.contains(&format!("unauthenticated request to {route}"))
                && err.contains("open to anyone"),
            "{err}"
        );
        assert_eq!(pod_ids(&rig).len(), 0, "{route}");
    }
}

#[test]
fn a_failed_terminate_on_replace_creates_nothing_and_keeps_the_record() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), ACCOUNT, &[])));
    let rig = rig();
    add_pod(&rig, "pod7", "NVIDIA A40", Some(owner()));
    rig.runpod.world.state.lock().unwrap().delete_fails = true;
    apply_with(&project, &rig).unwrap_err();
    assert_eq!(pod_writes(&rig), ["action pod7 terminate"]);
    assert_eq!(pod_ids(&rig), ["pod7"]);
    assert_eq!(stops_for(&project, DEP)[0].app_id.as_deref(), Some("pod7"));
}

#[test]
fn a_replacement_whose_new_spec_is_refused_leaves_the_old_pod_running() {
    struct CommaKey;
    #[async_trait::async_trait]
    impl SecretResolver for CommaKey {
        async fn resolve(&self, _: &SecretRef) -> Result<SecretValue, AdapterError> {
            Ok(SecretValue::new("a,b".to_owned()))
        }
    }
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), ACCOUNT, &[])));
    let mut rig = rig();
    add_pod(&rig, "pod7", "NVIDIA A40", Some(owner()));
    rig.resolver = Box::new(CommaKey);
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("comma"), "{err}");
    assert_eq!(pod_writes(&rig), Vec::<String>::new());
    assert_eq!(pod_ids(&rig), ["pod7"]);
}

#[test]
fn a_cloud_change_alone_replaces_the_pod() {
    let td = TempDir::new();
    let project = pod_project(&td, None);
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    write(
        &td.path,
        "amicu.hcl",
        &pod_config("    cloud = \"community\"\n"),
    );
    rig.runpod.world.state.lock().unwrap().log.clear();
    apply_with(&project, &rig).unwrap();
    assert_eq!(pod_writes(&rig), ["action pod1 terminate", "Post /pods"]);
    let w = rig.runpod.world.state.lock().unwrap();
    assert_eq!(w.pods.len(), 1);
    assert_eq!(w.pods[0].body["cloud"], "COMMUNITY");
}

#[test]
fn the_management_key_is_refused_as_a_pod_api_key() {
    struct ManagementKey;
    #[async_trait::async_trait]
    impl SecretResolver for ManagementKey {
        async fn resolve(&self, _: &SecretRef) -> Result<SecretValue, AdapterError> {
            Ok(SecretValue::new(MANAGEMENT_KEY.to_owned()))
        }
    }
    let td = TempDir::new();
    let project = pod_project(&td, None);
    let mut rig = rig();
    rig.resolver = Box::new(ManagementKey);
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("management key"), "{err}");
    assert!(err.contains("separate random key"), "{err}");
    assert!(!err.contains(MANAGEMENT_KEY), "{err}");
    assert_eq!(rig.runpod.world.log().len(), 0);
}

#[test]
fn a_pod_api_key_need_not_be_a_runpod_key() {
    struct RuntimeKey;
    #[async_trait::async_trait]
    impl SecretResolver for RuntimeKey {
        async fn resolve(&self, _: &SecretRef) -> Result<SecretValue, AdapterError> {
            Ok(SecretValue::new("9f2c4e7a1b3d5f60".repeat(4)))
        }
    }
    let td = TempDir::new();
    let project = pod_project(&td, None);
    let mut rig = rig();
    rig.resolver = Box::new(RuntimeKey);
    apply_with(&project, &rig).unwrap();
    assert_eq!(pod_ids(&rig), ["pod1"]);
}

#[test]
fn destroy_terminates_the_pod_and_deletes_its_secrets() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), ACCOUNT, &["amicu-x"])));
    let rig = rig();
    add_pod(&rig, "pod7", "NVIDIA L4", Some(owner()));
    add_secret(&rig, "s5", "amicu-x");
    destroy(&project, &rig).unwrap();
    assert_eq!(
        pod_writes(&rig),
        ["action pod7 terminate"],
        "never just stopped"
    );
    assert_eq!(pod_ids(&rig).len(), 0);
    assert_eq!(
        rig.runpod.world.secret_names(),
        [amicu_runpod::ACCOUNT_MARKER_SECRET]
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn a_failed_terminate_keeps_the_pod_record_and_its_secrets() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), ACCOUNT, &["amicu-x"])));
    let rig = rig();
    add_pod(&rig, "pod7", "NVIDIA L4", Some(owner()));
    add_secret(&rig, "s5", "amicu-x");
    rig.runpod.world.state.lock().unwrap().delete_fails = true;
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("kept in state"), "{err}");
    assert_eq!(stops_for(&project, DEP)[0].secrets, ["amicu-x"]);
    assert_eq!(pod_ids(&rig), ["pod7"]);
}

#[test]
fn destroy_without_a_pod_id_terminates_the_owned_pod_found_by_name() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(None, ACCOUNT, &[])));
    let rig = rig();
    add_pod(&rig, "pod9", "NVIDIA L4", Some(owner()));
    destroy(&project, &rig).unwrap();
    assert_eq!(pod_ids(&rig).len(), 0);
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn destroy_without_a_pod_id_refuses_a_foreign_pod_unless_forgetting() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(None, ACCOUNT, &["amicu-x"])));
    let mut rig = rig();
    add_pod(&rig, "pod9", "NVIDIA L4", Some("someone-else".to_owned()));
    add_secret(&rig, "s5", "amicu-x");
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(
        err.contains("AMICU_OWNER") && err.contains("--forget-unconfirmed"),
        "{err}"
    );
    assert_eq!(pod_ids(&rig), ["pod9"]);
    assert_eq!(stops_for(&project, DEP).len(), 1);

    rig.forget = true;
    destroy(&project, &rig).unwrap();
    assert_eq!(pod_ids(&rig), ["pod9"]);
    assert_eq!(
        rig.runpod.world.secret_names(),
        [amicu_runpod::ACCOUNT_MARKER_SECRET]
    );
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn destroy_of_a_pod_with_a_key_of_another_account_keeps_the_record() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), "personal:s99", &[])));
    let rig = rig();
    add_pod(&rig, "pod7", "NVIDIA L4", Some(owner()));
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("personal:s99"), "{err}");
    assert_eq!(pod_ids(&rig), ["pod7"]);
    assert_eq!(stops_for(&project, DEP).len(), 1);
}

#[test]
fn plan_shows_a_neutral_billing_line_per_mode() {
    let state = state_with(amicu_core::graph::Graph::new());
    let (text, _) = plan_text_for(&pod_config(""), &state);
    assert!(
        text.contains("      billing:  always on \u{b7} ~$0.59/hr\n"),
        "{text}"
    );
    assert!(
        text.contains("      workers:  1 pod (always on, never scales)\n"),
        "{text}"
    );
    let (text, _) = plan_text_for(&runpod_config(), &state);
    assert!(
        text.contains("      billing:  scale to zero \u{b7} ~$0.69/hr while active\n"),
        "{text}"
    );
    assert!(
        text.contains("      workers:  min 0 \u{b7} max 1 \u{b7} scale down after 15m idle\n"),
        "{text}"
    );
    let (text, _) = plan_text_for(&pod_config("    cloud = \"community\"\n"), &state);
    assert!(
        text.contains("      billing:  always on \u{b7} ~$0.44/hr\n"),
        "{text}"
    );
    for word in ["warn", "WARNING", "expensive"] {
        assert!(!text.contains(word), "{text}");
    }
}

#[test]
fn plan_shows_no_switch_step_for_a_recorded_pod() {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), ACCOUNT, &[])));
    let mut state = project.load_state().unwrap().unwrap();
    state.pending.clear();
    state.graph = amicu_core::graph::Graph::new();
    let (text, _) = plan_text_for(&pod_config(""), &state);
    assert!(!text.contains("destroy old"), "{text}");
    let (text, _) = plan_text_for(&runpod_config(), &state);
    assert!(text.contains("destroy old (runpod-pod "), "{text}");
}

// --- amicu runpod status / logs / metrics / stop -----------------------------

fn running_endpoint(rig: &Rig) -> (TempDir, Project) {
    let td = TempDir::new();
    let project = project_with(&td, Some(record(Some("ep7"), ACCOUNT, &[])));
    add_endpoint(rig, "ep7", Some(owner()));
    rig.runpod.world.state.lock().unwrap().endpoints[0].max_workers = Some(1);
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
    let mut out = Vec::new();
    commands::runpod::status(&unrecorded, DEP, OutputFormat::Human, &mut out, &rig.env()).unwrap();
    assert!(utf8(out).contains("RunPod endpoint ep7"));

    let empty = self::rig();
    let err = commands::runpod::status(
        &unrecorded,
        DEP,
        OutputFormat::Human,
        &mut Vec::new(),
        &empty.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("no RunPod endpoint id recorded and no endpoint named"),
        "{err}"
    );
}

#[test]
fn an_unrecorded_id_never_resolves_to_a_same_named_foreign_resource() {
    let rig = rig();
    let td = TempDir::new();
    let project = project_with(&td, Some(record(None, ACCOUNT, &[])));
    add_endpoint(&rig, "ep9", None);
    add_endpoint(&rig, "ep8", Some("someone-else".into()));
    let err = commands::runpod::metrics(
        &project,
        DEP,
        commands::runpod::MetricsMode {
            watch: false,
            interval_secs: 1,
            wake: true,
        },
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("carries its owner marker (ep9, ep8"), "{err}");
    assert!(
        !rig.runpod
            .world
            .log()
            .iter()
            .any(|l| l.starts_with("metrics")),
        "no key sent to a foreign endpoint"
    );

    let td = TempDir::new();
    let pods = pod_project(&td, Some(pod_record(None, ACCOUNT, &[])));
    add_pod(&rig, "pod9", "NVIDIA L4", Some("someone-else".into()));
    let err =
        commands::runpod::status(&pods, DEP, OutputFormat::Human, &mut Vec::new(), &rig.env())
            .unwrap_err()
            .to_string();
    assert!(err.contains("carries its owner marker (pod9"), "{err}");
}

#[test]
fn among_same_named_resources_the_one_carrying_our_marker_is_used() {
    let rig = rig();
    let td = TempDir::new();
    let project = project_with(&td, Some(record(None, ACCOUNT, &[])));
    add_endpoint(&rig, "ep8", Some("someone-else".into()));
    add_endpoint(&rig, "ep9", Some(owner()));
    let mut out = Vec::new();
    commands::runpod::status(&project, DEP, OutputFormat::Human, &mut out, &rig.env()).unwrap();
    assert!(utf8(out).contains("RunPod endpoint ep9"));

    add_endpoint(&rig, "ep10", Some(owner()));
    let err = commands::runpod::status(
        &project,
        DEP,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("carry its owner marker (ep9, ep10)"), "{err}");
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
    commands::runpod::logs(
        &project,
        DEP,
        Some(20),
        OutputFormat::Human,
        &mut out,
        &rig.env(),
    )
    .unwrap();
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
    let code = commands::runpod::logs(
        &project,
        DEP,
        None,
        OutputFormat::Human,
        &mut out,
        &rig.env(),
    )
    .unwrap();
    assert_eq!(code, 0);
    assert!(utf8(out).contains("no RunPod workers are running"));
}

const ONE_SHOT: commands::runpod::MetricsMode = commands::runpod::MetricsMode {
    watch: false,
    interval_secs: 5,
    wake: false,
};

#[test]
fn metrics_go_through_the_gateway_with_the_deployment_key() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    {
        let mut w = rig.runpod.world.state.lock().unwrap();
        w.metrics = "vllm:num_requests_running 3\nvllm:generation_tokens_total 10\n".to_owned();
        w.workers = vec![("w1".into(), "RUNNING".into())];
    }
    let mut out = Vec::new();
    commands::runpod::metrics(
        &project,
        DEP,
        ONE_SHOT,
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
        ONE_SHOT,
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

#[test]
fn metrics_refuse_to_wake_a_scaled_to_zero_endpoint_without_wake() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    rig.runpod.world.state.lock().unwrap().metrics = "vllm:num_requests_running 0\n".to_owned();
    let err = commands::runpod::metrics(
        &project,
        DEP,
        ONE_SHOT,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("--wake"), "{err}");
    let scraped = |rig: &Rig| {
        rig.runpod
            .world
            .log()
            .iter()
            .any(|l| l.starts_with("metrics "))
    };
    assert!(!scraped(&rig));

    let wake = commands::runpod::MetricsMode {
        wake: true,
        ..ONE_SHOT
    };
    commands::runpod::metrics(
        &project,
        DEP,
        wake,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap();
    assert!(scraped(&rig));
}

#[test]
fn logs_as_json_list_each_worker_with_its_lines() {
    let rig = rig();
    let (_td, project) = running_endpoint(&rig);
    rig.runpod.world.state.lock().unwrap().workers = vec![("w1".into(), "RUNNING".into())];
    let mut out = Vec::new();
    commands::runpod::logs(
        &project,
        DEP,
        None,
        OutputFormat::Json,
        &mut out,
        &rig.env(),
    )
    .unwrap();
    let doc: Value = serde_json::from_str(&utf8(out)).unwrap();
    assert_eq!(doc["endpoint_id"], "ep7");
    assert_eq!(doc["workers"][0]["id"], "w1");
    assert!(doc["workers"][0]["error"].is_null());
    assert_eq!(doc["workers"][0]["lines"][0]["line"], "hello from w1");
    assert_eq!(doc["workers"][0]["lines"][1]["source"], "system");
}

/// Applies the RunPod config with `region` on the hardware profile and
/// `extra` HCL appended; returns apply's result and the endpoint create bodies.
fn apply_in_region(region: &str, extra: &str) -> (Result<u8, crate::error::CliError>, Vec<Value>) {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let config = runpod_config().replacen(
        "provider = \"runpod\"",
        &format!("provider = \"runpod\"\n  region   = \"{region}\""),
        1,
    );
    write(&td.path, "amicu.hcl", &format!("{config}{extra}"));
    let rig = rig();
    let result = apply_with(&project, &rig);
    let created = rig.runpod.world.state.lock().unwrap().created.clone();
    (result, created)
}

fn created_in(region: &str) -> Value {
    let (result, created) = apply_in_region(region, "");
    assert_eq!(result.unwrap(), 0);
    assert_eq!(created.len(), 1);
    created[0].clone()
}

fn data_centers(body: &Value) -> Vec<String> {
    body["dataCenterIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_region_group_places_the_endpoint_in_its_data_centers() {
    let body = created_in("eu");
    let dcs = data_centers(&body);
    assert!(dcs.len() > 1, "{dcs:?}");
    assert!(dcs.iter().all(|d| d.starts_with("EU")), "{dcs:?}");
    // llama.cpp serves on the image's port and asks for its CUDA floor.
    assert_eq!(body["ports"], json!(["8080/http"]));
    assert_eq!(body["env"]["PORT_HEALTH"], json!("8080"));
    assert_eq!(body["gpu"]["minCudaVersion"], json!("12.8"));
}

#[test]
fn an_exact_data_center_places_the_endpoint_there() {
    let body = created_in("EU-RO-1");
    assert_eq!(data_centers(&body), ["EU-RO-1"]);
}

#[test]
fn a_region_group_outside_the_data_class_residency_is_refused_before_any_create() {
    let extra = r#"
resource "data_class" "customer" {
  sensitivity = "confidential"
  residency   = ["us"]
}
"#;
    // The plan-time check cannot resolve the `eu` group; apply still refuses it.
    let (result, created) = apply_in_region("eu", extra);
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("not within the required residency (us)"),
        "{err}"
    );
    assert_eq!(created.len(), 0);
}

// --- pods in refresh, orphans and `amicu runpod` ------------------------------

fn applied_pod(td: &TempDir, record: StopRecord) -> Project {
    let project = pod_project(td, Some(record));
    let mut state = project.load_state().unwrap().unwrap();
    state.pending.clear();
    project.save_state(&state).unwrap();
    project
}

fn add_named_pod(rig: &Rig, id: &str, name: &str, owner: Option<&str>, status: &'static str) {
    let mut env = serde_json::Map::new();
    if let Some(o) = owner {
        env.insert(amicu_runpod::OWNER_ENV_VAR.to_owned(), json!(o));
    }
    rig.runpod.world.state.lock().unwrap().pods.push(PodRec {
        id: id.to_owned(),
        body: json!({"name": name, "gpu": {"id": "NVIDIA L4", "count": 1},
            "cloud": "SECURE", "dataCenterId": "EU-RO-1", "cost": 0.44,
            "ports": ["8080/http"], "env": env}),
        status,
        started: 0,
    });
}

#[test]
fn refresh_keeps_a_live_pod_without_notes() {
    let td = TempDir::new();
    let project = applied_pod(&td, pod_record(Some("pod7"), ACCOUNT, &[]));
    let rig = rig();
    add_pod(&rig, "pod7", "NVIDIA L4", Some(owner()));
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r, crate::refresh::Refresh::default());
    assert_eq!(state.pending.len(), 0);
    assert!(
        rig.runpod
            .world
            .log()
            .contains(&"Get /pods/pod7".to_owned())
    );
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

#[test]
fn refresh_marks_a_pod_gone_or_terminated_in_the_recorded_account_pending() {
    for terminated in [false, true] {
        let td = TempDir::new();
        let project = applied_pod(&td, pod_record(Some("pod7"), ACCOUNT, &["amicu-x"]));
        let rig = rig();
        if terminated {
            add_named_pod(&rig, "pod7", &endpoint_name(), None, "TERMINATED");
        }
        let (r, state) = refresh(&project, &rig);
        assert_eq!(r.notes.len(), 1, "{r:?}");
        let note = &r.notes[0];
        assert!(
            note.contains("RunPod pod") && note.contains("pod id pod7") && note.contains(ACCOUNT),
            "{note}"
        );
        assert!(note.contains("kept in state as pending"), "{note}");
        assert!(state.pending.iter().any(|d| d.as_str() == DEP));
        assert_eq!(
            state.stops,
            [pod_record(Some("pod7"), ACCOUNT, &["amicu-x"])]
        );
        assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
    }
}

#[test]
fn refresh_never_calls_a_pod_gone_from_another_account() {
    let td = TempDir::new();
    let project = applied_pod(&td, pod_record(Some("pod7"), "personal:s99", &[]));
    let rig = rig();
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 0, "{r:?}");
    let warning = r.warning.unwrap();
    assert!(
        warning.contains("RunPod pod") && warning.contains("cannot tell whether it is gone"),
        "{warning}"
    );
    assert_eq!(state.pending.len(), 0);
}

#[test]
fn refresh_finds_an_id_less_pod_by_name_and_owner_and_never_a_foreign_one() {
    let td = TempDir::new();
    let project = applied_pod(&td, pod_record(None, ACCOUNT, &[]));
    let rig = rig();
    add_pod(&rig, "pod9", "NVIDIA L4", Some(owner()));
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r, crate::refresh::Refresh::default());
    assert_eq!(state.pending.len(), 0);

    let rig = self::rig();
    add_pod(&rig, "pod9", "NVIDIA L4", Some("someone-else".to_owned()));
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 0, "{r:?}");
    let warning = r.warning.unwrap();
    assert!(
        warning.contains("no pod id recorded")
            && warning.contains("pod9")
            && warning.contains("AMICU_OWNER"),
        "{warning}"
    );
    assert_eq!(state.pending.len(), 0);
}

#[test]
fn refresh_reports_gpu_and_cloud_drift_of_a_live_pod_as_a_note_only() {
    let td = TempDir::new();
    let project = applied_pod(&td, pod_record(Some("pod7"), ACCOUNT, &[]));
    let rig = rig();
    add_pod(&rig, "pod7", "NVIDIA A40", Some(owner()));
    rig.runpod.world.state.lock().unwrap().pods[0].body["cloud"] = json!("COMMUNITY");
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 1, "{r:?}");
    let note = &r.notes[0];
    assert!(
        note.contains("RunPod pod") && note.contains("pod7"),
        "{note}"
    );
    assert!(
        note.contains("GPU type NVIDIA A40 (applied NVIDIA L4)"),
        "{note}"
    );
    assert!(note.contains("cloud COMMUNITY (applied SECURE)"), "{note}");
    assert!(note.contains("revert it in the RunPod console"), "{note}");
    assert_eq!(state.pending.len(), 0);
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

#[test]
fn orphans_lists_untracked_owned_pods_with_a_terminate_command() {
    use crate::commands::orphans::{Kind, Ownership};
    let td = TempDir::new();
    let project = applied_pod(&td, pod_record(Some("pod7"), ACCOUNT, &[]));
    let rig = rig();
    add_named_pod(&rig, "pod7", &endpoint_name(), Some(&owner()), "RUNNING");
    let stale = amicu_runpod::project_endpoint_name(PROJECT, "old");
    add_named_pod(&rig, "pod8", &stale, Some("marker"), "EXITED");
    add_named_pod(&rig, "pod9", "not-amicu", None, "RUNNING");
    add_named_pod(&rig, "pod10", &stale, Some("marker"), "TERMINATED");
    let report = orphans(&project, &rig);
    let pods: Vec<(&str, bool, Ownership)> = report
        .resources
        .iter()
        .filter(|r| r.kind == Kind::RunpodPod)
        .map(|r| (r.identity.as_str(), r.tracked, r.ownership))
        .collect();
    let stale_id = format!("{stale} (pod8)");
    let ours = format!("{} (pod7)", endpoint_name());
    assert_eq!(pods.len(), 2, "{pods:?}");
    assert!(pods.contains(&(stale_id.as_str(), false, Ownership::ThisProject)));
    assert!(pods.iter().any(|p| p.0 == ours && p.1), "{pods:?}");
    let orphan = report
        .resources
        .iter()
        .find(|r| r.identity == stale_id)
        .unwrap();
    assert!(
        orphan.stop_command.contains("curl -X DELETE")
            && orphan.stop_command.contains("$RUNPOD_API_KEY")
            && orphan.stop_command.contains("/v2/pods/pod8"),
        "{}",
        orphan.stop_command
    );
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

fn running_pod(rig: &Rig) -> (TempDir, Project) {
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), ACCOUNT, &[])));
    add_named_pod(rig, "pod7", &endpoint_name(), Some(&owner()), "RUNNING");
    (td, project)
}

#[test]
fn status_shows_the_pod_gpu_cloud_cost_and_proxy_url() {
    let rig = rig();
    let (_td, project) = running_pod(&rig);
    let mut out = Vec::new();
    commands::runpod::status(&project, DEP, OutputFormat::Human, &mut out, &rig.env()).unwrap();
    let text = utf8(out);
    assert!(text.contains("RunPod pod pod7"), "{text}");
    assert!(text.contains("status:      RUNNING"), "{text}");
    assert!(
        text.contains("url:         https://pod7-8080.proxy.runpod.net"),
        "{text}"
    );
    assert!(text.contains("gpu:         NVIDIA L4 x1"), "{text}");
    assert!(text.contains("cloud:       SECURE"), "{text}");
    assert!(text.contains("data center: EU-RO-1"), "{text}");
    assert!(text.contains("cost:        $0.44/hr"), "{text}");

    let mut out = Vec::new();
    commands::runpod::status(&project, DEP, OutputFormat::Json, &mut out, &rig.env()).unwrap();
    let doc: Value = serde_json::from_str(&utf8(out)).unwrap();
    assert_eq!(doc["pod_id"], "pod7");
    assert_eq!(doc["url"], "https://pod7-8080.proxy.runpod.net");
    assert_eq!(doc["cost_per_hour_usd"], 0.44);
    assert_eq!(rig.runpod.world.writes(), Vec::<String>::new());
}

#[test]
fn status_of_a_pod_gone_remotely_or_unrecorded_says_so() {
    let rig = rig();
    let td = TempDir::new();
    let project = pod_project(&td, Some(pod_record(Some("pod7"), ACCOUNT, &[])));
    let err = commands::runpod::status(
        &project,
        DEP,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("RunPod pod pod7") && err.contains("not found"),
        "{err}"
    );

    let td = TempDir::new();
    let unrecorded = pod_project(&td, Some(pod_record(None, ACCOUNT, &[])));
    let err = commands::runpod::status(
        &unrecorded,
        DEP,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no RunPod pod id recorded"), "{err}");
}

#[test]
fn pod_logs_are_read_from_the_pod_logs_route() {
    let rig = rig();
    let (_td, project) = running_pod(&rig);
    let mut out = Vec::new();
    commands::runpod::logs(
        &project,
        DEP,
        Some(20),
        OutputFormat::Human,
        &mut out,
        &rig.env(),
    )
    .unwrap();
    let text = utf8(out);
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        ["[pod7] (system) pulled", "[pod7] hello from pod7"],
        "{text}"
    );
    assert!(
        rig.runpod
            .world
            .log()
            .contains(&"Get /pods/pod7/logs?tail=20".to_owned())
    );

    let mut out = Vec::new();
    commands::runpod::logs(
        &project,
        DEP,
        None,
        OutputFormat::Json,
        &mut out,
        &rig.env(),
    )
    .unwrap();
    let doc: Value = serde_json::from_str(&utf8(out)).unwrap();
    assert_eq!(doc["pod_id"], "pod7");
    assert_eq!(doc["lines"][1]["line"], "hello from pod7");
}

#[test]
fn pod_metrics_go_through_the_proxy_url_with_the_runtime_key() {
    let rig = rig();
    let (_td, project) = running_pod(&rig);
    rig.runpod.world.state.lock().unwrap().metrics = "llamacpp:requests_processing 2\n".to_owned();
    let mut out = Vec::new();
    commands::runpod::metrics(
        &project,
        DEP,
        ONE_SHOT,
        OutputFormat::Json,
        &mut out,
        &rig.env(),
    )
    .unwrap();
    let doc: Value = serde_json::from_str(&utf8(out)).unwrap();
    assert_eq!(doc["pod_id"], "pod7");
    assert_eq!(doc["runtime"]["requests_running"], 2);
    let log = rig.runpod.world.log();
    assert!(
        log.contains(&"proxy metrics pod7-8080 bearer=rpa_ENDPOINTKEY".to_owned()),
        "{log:?}"
    );
    assert!(!log.iter().any(|l| l.contains("workers")), "{log:?}");
}

#[test]
fn stop_terminates_the_pod_then_drops_the_record() {
    let rig = rig();
    let (_td, project) = running_pod(&rig);
    commands::runpod::stop(
        &project,
        DEP,
        None,
        OutputFormat::Human,
        &mut Vec::new(),
        &rig.env(),
    )
    .unwrap();
    assert_eq!(pod_writes(&rig), ["action pod7 terminate"]);
    assert_eq!(pod_ids(&rig).len(), 0);
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn a_pod_region_outside_the_data_class_residency_is_refused_before_any_create() {
    let td = TempDir::new();
    let project = project_with(&td, None);
    let config = pod_config("").replacen(
        "provider = \"runpod\"",
        "provider = \"runpod\"\n  region   = \"eu\"",
        1,
    );
    let extra = r#"
resource "data_class" "customer" {
  sensitivity = "confidential"
  residency   = ["us"]
}
"#;
    write(&td.path, "amicu.hcl", &format!("{config}{extra}"));
    let rig = rig();
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(
        err.contains("not within the required residency (us)"),
        "{err}"
    );
    assert_eq!(pod_ids(&rig).len(), 0);
}
