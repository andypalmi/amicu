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
    let mut ep = json!({"id": e.id, "name": e.name, "type": "LOAD_BALANCER", "env": env});
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
    assert!(note.contains("workers 0-7 (applied 0-3)"), "{note}");
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
