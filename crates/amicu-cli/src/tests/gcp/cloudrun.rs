//! `backend = "cloud_run"` through plan, apply, destroy, refresh and orphans,
//! against an in-memory Cloud Run and Secret Manager API. No Google calls.

use super::*;

const KEY: &str = "sk-gce-test-key";
const REGION: &str = "europe-west4";

struct Service {
    region: String,
    name: String,
    labels: Value,
}

#[derive(Default)]
pub(super) struct RunWorld {
    services: Vec<Service>,
    /// Body of every service create.
    bodies: Vec<Value>,
    /// `METHOD path` of every Cloud Run API call, and `app ...` for every
    /// request to a service URL.
    log: Vec<String>,
    next_op: u32,
    /// Fired once, just before the first service create.
    on_create: Option<Hook>,
    /// The service answers unauthenticated requests (vLLM without its key).
    open_endpoint: bool,
}

fn uri(name: &str) -> String {
    format!("https://{name}-abc123-ez.a.run.app")
}

fn service_json(project: &str, s: &Service) -> Value {
    json!({
        "name": format!("projects/{project}/locations/{}/services/{}", s.region, s.name),
        "labels": s.labels,
        "terminalCondition": {"type": "Ready", "state": "CONDITION_SUCCEEDED"},
        "latestReadyRevision": format!("{}-00001", s.name),
        "uri": uri(&s.name),
    })
}

impl World {
    /// The Cloud Run Admin API and the services' `*.run.app` URLs; `None` for
    /// any other URL.
    pub(super) fn cloud_run(
        &self,
        req: &HttpRequest,
    ) -> Option<Result<HttpResponse, TransportError>> {
        if let Some(rest) = req.url.strip_prefix(amicu_gcp::RUN_BASE_URL) {
            return Some(self.run_api(req, rest));
        }
        req.url
            .contains(".a.run.app")
            .then(|| Ok(self.run_app(req)))
    }

    fn run_api(&self, req: &HttpRequest, rest: &str) -> Result<HttpResponse, TransportError> {
        let method = req.method;
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        self.run
            .lock()
            .unwrap()
            .log
            .push(format!("{method:?} {path}"));
        match (method, parts.as_slice()) {
            (HttpMethod::Post, ["projects", p, "locations", r, "services"]) => {
                let hook = self.run.lock().unwrap().on_create.take();
                if let Some(hook) = hook {
                    hook();
                }
                let name = query.strip_prefix("serviceId=").unwrap_or_default();
                let body = req.body.clone().unwrap_or_default();
                let mut w = self.run.lock().unwrap();
                if w.services.iter().any(|s| s.name == name && s.region == *r) {
                    return json_reply(
                        409,
                        &json!({"error": {"code": 409, "status": "ALREADY_EXISTS", "message": "exists"}}),
                    );
                }
                w.bodies.push(body.clone());
                w.services.push(Service {
                    region: (*r).to_owned(),
                    name: name.to_owned(),
                    labels: body["labels"].clone(),
                });
                w.next_op += 1;
                let op = format!("projects/{p}/locations/{r}/operations/op-{}", w.next_op);
                json_reply(200, &json!({"name": op, "done": false}))
            }
            (HttpMethod::Get, ["projects", p, "locations", r, "services"]) => {
                let w = self.run.lock().unwrap();
                let items: Vec<Value> = w
                    .services
                    .iter()
                    .filter(|s| s.region == *r)
                    .map(|s| service_json(p, s))
                    .collect();
                json_reply(200, &json!({"services": items}))
            }
            (HttpMethod::Get, ["projects", p, "locations", r, "services", name]) => {
                let w = self.run.lock().unwrap();
                match w
                    .services
                    .iter()
                    .find(|s| s.name == *name && s.region == *r)
                {
                    Some(s) => json_reply(200, &service_json(p, s)),
                    None => not_found(),
                }
            }
            (HttpMethod::Delete, ["projects", p, "locations", r, "services", name]) => {
                let mut w = self.run.lock().unwrap();
                w.services.retain(|s| !(s.name == *name && s.region == *r));
                w.next_op += 1;
                let op = format!("projects/{p}/locations/{r}/operations/op-{}", w.next_op);
                json_reply(200, &json!({"name": op, "done": false}))
            }
            (HttpMethod::Get, ["projects", _, "locations", _, "operations", _]) => {
                json_reply(200, &json!({"name": path, "done": true}))
            }
            _ => json_reply(
                400,
                &json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": format!("unexpected {method:?} {path}")}}),
            ),
        }
    }

    fn run_app(&self, req: &HttpRequest) -> HttpResponse {
        let path = req.url.split_once(".a.run.app").map_or("", |(_, p)| p);
        let mut w = self.run.lock().unwrap();
        let who = match req.bearer.as_deref() {
            None => "anon",
            Some(KEY) => "key",
            Some(_) => "wrong",
        };
        w.log.push(format!("app {path} {who}"));
        let status = match (path, who) {
            ("/health", _) | (_, "key") => 200,
            (_, "anon") if w.open_endpoint => 200,
            _ => 401,
        };
        HttpResponse::new(status, "{}")
    }

    fn run_log(&self) -> Vec<String> {
        self.run.lock().unwrap().log.clone()
    }

    fn service_names(&self) -> Vec<String> {
        let w = self.run.lock().unwrap();
        w.services.iter().map(|s| s.name.clone()).collect()
    }

    fn add_service(&self, name: &str, labels: Value) {
        self.run.lock().unwrap().services.push(Service {
            region: REGION.to_owned(),
            name: name.to_owned(),
            labels,
        });
    }

    fn remove_service(&self, name: &str) {
        self.run.lock().unwrap().services.retain(|s| s.name != name);
    }
}

fn config(extra_gcp: &str, scaling: &str) -> String {
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

resource "hardware_profile" "l4" {{
  provider = "gcp"
  region   = "{REGION}"
  accelerator {{
    kind   = "gpu"
    model  = "l4"
    count  = 1
    memory = "24GiB"
  }}
  memory = "16GiB"
  gcp {{
    project         = "{GOOGLE_PROJECT}"
    backend         = "cloud_run"
    service_account = "{SERVICE_ACCOUNT}"
    {extra_gcp}
  }}
}}

resource "deployment" "chat" {{
  model    = model.qwen.id
  hardware = hardware_profile.l4.id
  runtime  = "vllm"
  api_key  = secret("GCP_TEST_KEY")
  vllm {{
    context_length = 4096
    port           = 8000
  }}
  {scaling}
}}
"#
    )
}

const SCALING: &str = "scaling {\n    max_replicas = 2\n  }";

fn service() -> String {
    amicu_gcp::cloudrun::service_name(PROJECT, DEP)
}

fn secrets() -> Vec<String> {
    ["api-key", "hf-token"]
        .map(|k| format!("{}-{k}", service()))
        .to_vec()
}

fn run_record(project: &Project) -> Option<amicu_gcp::cloudrun::CloudRunRecord> {
    stops_for(project, DEP)
        .into_iter()
        .find_map(|r| r.cloud_run)
}

fn applied() -> (TempDir, Project, Rig) {
    let td = TempDir::new();
    let project = project_from(&td, &config("", SCALING));
    let rig = rig();
    apply_with(&project, &rig).unwrap();
    (td, project, rig)
}

#[test]
fn apply_records_the_service_ahead_stores_its_key_and_verifies_the_endpoint() {
    let td = TempDir::new();
    let project = project_from(&td, &config("", SCALING));
    let rig = rig();
    let ahead: Arc<Mutex<Option<StopRecord>>> = Arc::default();
    let secrets_before: Arc<Mutex<Vec<String>>> = Arc::default();
    {
        let take = record_taker(&td, &ahead);
        let world = Arc::clone(&rig.gcp.world);
        let seen = Arc::clone(&secrets_before);
        rig.gcp.world.run.lock().unwrap().on_create = Some(Box::new(move || {
            take();
            *seen.lock().unwrap() = world.secret_ids();
        }));
    }
    apply_with(&project, &rig).unwrap();

    let ahead = ahead
        .lock()
        .unwrap()
        .clone()
        .expect("recorded before the create");
    assert_eq!(ahead.backend, Backend::CloudRun);
    assert_eq!(ahead.identity, service());
    let c = ahead.cloud_run.expect("record");
    assert_eq!(c.project, GOOGLE_PROJECT);
    assert_eq!(c.region, REGION);
    assert_eq!(c.owner, amicu_gcp::owner_marker(None, PROJECT, DEP));
    assert_eq!(c.secrets, secrets());
    assert_eq!(
        *secrets_before.lock().unwrap(),
        [secrets()[0].clone()],
        "the key is in Secret Manager before the service exists"
    );
    {
        let w = rig.gcp.world.state.lock().unwrap();
        let (_, labels, replication) = &w.secrets[0];
        assert_eq!(labels["amicu-owner"], c.owner);
        assert_eq!(
            replication["userManaged"]["replicas"][0]["location"],
            REGION
        );
        assert_eq!(
            w.grants,
            [(
                secrets()[0].clone(),
                format!("serviceAccount:{SERVICE_ACCOUNT}")
            )]
        );
    }

    let body = rig.gcp.world.run.lock().unwrap().bodies[0].clone();
    let t = &body["template"];
    let ctr = &t["containers"][0];
    assert_eq!(body["invokerIamDisabled"], true);
    assert_eq!(body["labels"]["amicu-owner"], c.owner);
    assert_eq!(t["nodeSelector"]["accelerator"], "nvidia-l4");
    assert_eq!(t["gpuZonalRedundancyDisabled"], true);
    assert_eq!(t["scaling"]["minInstanceCount"], 0);
    assert_eq!(t["scaling"]["maxInstanceCount"], 2);
    assert_eq!(t["serviceAccount"], SERVICE_ACCOUNT);
    assert_eq!(ctr["resources"]["limits"]["cpu"], "4");
    assert_eq!(ctr["resources"]["limits"]["memory"], "16Gi");
    assert_eq!(ctr["startupProbe"]["httpGet"]["path"], "/health");
    assert!(ctr["startupProbe"]["failureThreshold"].as_u64().unwrap() >= 180);
    assert_eq!(
        ctr["env"][0],
        json!({"name": "VLLM_API_KEY", "valueSource": {"secretKeyRef": {"secret": secrets()[0], "version": "latest"}}})
    );
    let args: Vec<&str> = ctr["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(args[..2], ["--model", "Qwen/Qwen3-0.6B"]);
    assert_eq!(
        args.iter().filter(|a| **a == "--port").count(),
        1,
        "{args:?}"
    );
    assert_eq!(
        args.iter().filter(|a| **a == "--host").count(),
        1,
        "{args:?}"
    );
    assert!(
        !body.to_string().contains(KEY),
        "the key never reaches the spec"
    );

    let log = rig.gcp.world.run_log();
    let anon = log.iter().position(|l| l == "app /v1/models anon").unwrap();
    let keyed = log.iter().position(|l| l == "app /v1/models key").unwrap();
    assert!(anon < keyed, "{log:?}");

    let state = project.load_state().unwrap().unwrap();
    assert_eq!(state.pending.len(), 0);
    assert_eq!(state.endpoints[0].endpoint, uri(&service()));
    assert_eq!(run_record(&project).unwrap(), c);
}

#[test]
fn a_crash_between_the_record_and_the_create_is_cleaned_up_by_destroy() {
    for created in [false, true] {
        let td = TempDir::new();
        let project = project_from(&td, &config("", SCALING));
        let rig = rig();
        let at_create: Arc<Mutex<Option<LocalState>>> = Arc::default();
        {
            let slot = Arc::clone(&at_create);
            let p = Project::new(&td.path);
            rig.gcp.world.run.lock().unwrap().on_create = Some(Box::new(move || {
                *slot.lock().unwrap() = p.load_state().unwrap();
            }));
        }
        apply_with(&project, &rig).unwrap();
        // The process died at the create: state is what it was then, and the
        // create may or may not have reached Google.
        let crashed = at_create.lock().unwrap().clone().unwrap();
        project.save_state(&crashed).unwrap();
        if !created {
            rig.gcp.world.remove_service(&service());
        }
        assert!(crashed.pending.iter().any(|d| d.as_str() == DEP));
        assert_eq!(rig.gcp.world.secret_ids(), [secrets()[0].clone()]);

        destroy(&project, &rig).unwrap();
        assert_eq!(rig.gcp.world.service_names(), Vec::<String>::new());
        assert_eq!(rig.gcp.world.secret_ids(), Vec::<String>::new());
        assert_eq!(run_record(&project), None, "dropped once confirmed gone");
    }
}

#[test]
fn a_failed_exposure_check_deletes_the_service_and_secrets_this_run_created() {
    let td = TempDir::new();
    let project = project_from(&td, &config("", SCALING));
    let rig = rig();
    rig.gcp.world.run.lock().unwrap().open_endpoint = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("failed its exposure check"), "{err}");
    assert!(err.contains("unauthenticated"), "{err}");
    assert!(err.contains("were deleted"), "{err}");
    assert_eq!(rig.gcp.world.service_names(), Vec::<String>::new());
    assert_eq!(rig.gcp.world.secret_ids(), Vec::<String>::new());
    // The record stays until a later run confirms the stop.
    assert!(run_record(&project).is_some());
    destroy(&project, &rig).unwrap();
    assert_eq!(run_record(&project), None);
}

#[test]
fn destroy_deletes_the_service_confirms_it_is_gone_then_deletes_the_secrets() {
    let (_td, project, rig) = applied();
    destroy(&project, &rig).unwrap();
    assert_eq!(rig.gcp.world.service_names(), Vec::<String>::new());
    assert_eq!(rig.gcp.world.secret_ids(), Vec::<String>::new());
    let log = rig.gcp.world.run_log();
    let svc = format!(
        "/projects/{GOOGLE_PROJECT}/locations/{REGION}/services/{}",
        service()
    );
    let del = log
        .iter()
        .position(|l| *l == format!("Delete {svc}"))
        .unwrap();
    assert!(
        log[del..].contains(&format!("Get {svc}")),
        "the delete is confirmed by a read that 404s: {log:?}"
    );
    let gcp_log = rig.gcp.world.log();
    assert!(gcp_log.iter().any(|l| l.starts_with("secret-delete")));
    assert_eq!(run_record(&project), None);
    let state = project.load_state().unwrap().unwrap();
    assert_eq!(state.graph.deployments.len(), 0);
}

#[test]
fn destroy_refuses_a_service_that_lost_our_label_and_keeps_everything() {
    let (_td, project, rig) = applied();
    rig.gcp.world.remove_service(&service());
    rig.gcp
        .world
        .add_service(&service(), json!({"env": "prod"}));
    let err = destroy(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("owner label"), "{err}");
    assert_eq!(rig.gcp.world.service_names(), [service()], "left alone");
    assert_eq!(rig.gcp.world.secret_ids().len(), 1, "secrets kept too");
    assert!(run_record(&project).is_some(), "record kept");
}

#[test]
fn refresh_marks_a_missing_service_pending_and_destroy_still_cleans_up() {
    let (_td, project, rig) = applied();
    let (r, state) = refresh(&project, &rig);
    assert_eq!(
        r,
        crate::refresh::Refresh::default(),
        "a live service has no notes"
    );
    assert_eq!(state.pending.len(), 0);

    rig.gcp.world.remove_service(&service());
    let (r, state) = refresh(&project, &rig);
    assert_eq!(r.notes.len(), 1, "{r:?}");
    assert!(r.notes[0].contains("Cloud Run service"), "{}", r.notes[0]);
    assert!(r.notes[0].contains("gone"), "{}", r.notes[0]);
    assert!(
        r.notes[0].contains("apply will recreate it"),
        "{}",
        r.notes[0]
    );
    assert!(state.pending.iter().any(|d| d.as_str() == DEP));

    destroy(&project, &rig).unwrap();
    assert_eq!(rig.gcp.world.secret_ids(), Vec::<String>::new());
    assert_eq!(run_record(&project), None);
}

#[test]
fn reapplying_a_changed_service_replaces_it_and_keeps_one_record() {
    let (td, project, rig) = applied();
    write(
        &td.path,
        "amicu.hcl",
        &config("", "scaling {\n    max_replicas = 3\n  }"),
    );
    apply_with(&project, &rig).unwrap();
    assert_eq!(rig.gcp.world.service_names(), [service()]);
    let bodies = rig.gcp.world.run.lock().unwrap().bodies.clone();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["template"]["scaling"]["maxInstanceCount"], 3);
    assert_eq!(stops_for(&project, DEP).len(), 1);
}

#[test]
fn plan_shows_the_cloud_run_service_its_instance_scaling_and_price() {
    let td = TempDir::new();
    let project = project_from(&td, &config("", SCALING));
    let mut out = Vec::new();
    let code =
        crate::commands::plan::run_with(&project, OutputFormat::Human, &mut out, |_| true).unwrap();
    let text = utf8(out);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(&format!("service:  {} (Cloud Run, {REGION}", service())),
        "{text}"
    );
    assert!(
        text.contains("1x nvidia-l4 \u{b7} 4 vCPU \u{b7} 16 GiB"),
        "{text}"
    );
    assert!(text.contains("min 0 \u{b7} max 2"), "{text}");
    assert!(text.contains("scale to zero \u{b7} ~$0.67/hr"), "{text}");
}

#[test]
fn fields_cloud_run_cannot_honour_stop_apply_before_anything_is_written() {
    let td = TempDir::new();
    let project = project_from(
        &td,
        &config(
            "zone = \"europe-west4-a\"",
            "scaling {\n    min_replicas = 1\n  }",
        ),
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
    assert!(err.contains("AMICU-CAP-019"), "{err}");
    assert!(
        err.contains("`zone`") && err.contains("min_replicas"),
        "{err}"
    );
    assert_eq!(rig.gcp.world.run_log(), Vec::<String>::new());
    assert_eq!(stops_for(&project, DEP).len(), 0);
}

#[test]
fn orphans_lists_an_owned_service_state_does_not_track() {
    use crate::commands::orphans::{
        Env, GcpOpsListing, Kind, Ownership, Platform, Tracked, discover,
    };
    let (_td, project, rig) = applied();
    let old = amicu_gcp::cloudrun::service_name(PROJECT, "old");
    rig.gcp
        .world
        .add_service(&old, json!(amicu_gcp::owner_labels(None, PROJECT, "old")));
    rig.gcp
        .world
        .add_service("unrelated", json!({"env": "prod"}));
    let writes_before = rig.gcp.world.run_log().len();

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
    let services: Vec<_> = report
        .resources
        .iter()
        .filter(|r| r.kind == Kind::CloudRunService)
        .collect();
    assert_eq!(services.len(), 2, "{services:?}");
    assert!(
        services
            .iter()
            .any(|r| r.tracked && r.identity.starts_with(&service()))
    );
    let untracked = services.iter().find(|r| !r.tracked).unwrap();
    assert!(untracked.identity.starts_with(&old), "{untracked:?}");
    assert_eq!(untracked.ownership, Ownership::ThisProject);
    assert!(
        untracked
            .stop_command
            .starts_with("gcloud run services delete"),
        "{}",
        untracked.stop_command
    );
    // The recorded secret is tracked; listing writes nothing.
    assert!(
        report
            .resources
            .iter()
            .filter(|r| r.kind == Kind::GceSecret)
            .all(|r| r.tracked)
    );
    assert!(
        rig.gcp.world.run_log()[writes_before..]
            .iter()
            .all(|l| l.starts_with("Get")),
    );
}

#[test]
fn the_cloud_run_example_validates_and_plans() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/gcp-cloudrun-l4/amicu.hcl");
    let text = std::fs::read_to_string(&path).unwrap();
    let td = TempDir::new();
    write(&td.path, "amicu.hcl", &text);
    let project = Project::new(&td.path);
    let mut out = Vec::new();
    let code = commands::validate::run(&project, false, OutputFormat::Human, &mut out).unwrap();
    assert_eq!(code, 0, "{}", utf8(out));
    let mut out = Vec::new();
    let code = commands::plan::run_with(&project, OutputFormat::Human, &mut out, |_| true).unwrap();
    let text = utf8(out);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("Cloud Run, europe-west4"), "{text}");
    assert!(text.contains("min 0 \u{b7} max 1"), "{text}");
}

#[test]
fn the_preflight_reads_cloud_run_not_compute_engine_and_needs_credentials() {
    let (_td, _project, done) = applied();
    let log = done.gcp.world.run_log();
    assert_eq!(
        log[0],
        format!("Get /projects/{GOOGLE_PROJECT}/locations/{REGION}/services"),
        "{log:?}"
    );
    let compute = done.gcp.world.log();
    assert!(
        compute.iter().all(|l| l.starts_with("secret-")),
        "no Compute Engine call: {compute:?}"
    );

    let td = TempDir::new();
    let project = project_from(&td, &config("", SCALING));
    let mut rig = rig();
    rig.gcp.no_credentials = true;
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("no Google Cloud credentials found"), "{err}");
    assert_eq!(stops_for(&project, DEP).len(), 0);
    assert_eq!(rig.gcp.world.run_log(), Vec::<String>::new());
}
