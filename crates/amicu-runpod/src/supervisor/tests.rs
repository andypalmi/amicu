use std::collections::VecDeque;
use std::sync::Mutex;

use amicu_core::ByteSize;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{AcceleratorKind, LlamaCppSettings, Residency, VllmSettings};
use amicu_providers::{HttpResponse, TransportError};
use amicu_runtime::{RenderedLaunch, ResourceEstimate};
use serde_json::{Value, json};

use super::*;
use crate::client::ApiKey;

/// The end-user endpoint key (the deployment's `api_key`).
const KEY: &str = "rpa_ENDUSERKEY0123456789";
/// The operator's RunPod management key.
const MGMT: &str = "rpa_MANAGEMENTKEY";
const BASE: &str = "https://api.runpod.io/v2";
const PROJECT: &str = "aaaa1111";
const DEP: &str = "chat";

#[derive(Clone)]
enum Canned {
    Status(u16, &'static str),
    Timeout,
}

type Route = (HttpMethod, &'static str, VecDeque<Canned>);

#[derive(Clone)]
struct Seen {
    method: HttpMethod,
    url: String,
    bearer: Option<String>,
    body: Option<Value>,
}

/// Canned answers by `(method, url suffix)`; a suffix ending in `=` matches
/// any URL containing it (query lookups). Each route plays its answers in
/// order and repeats the last one. Every request is recorded.
#[derive(Default)]
struct Mock {
    routes: Mutex<Vec<Route>>,
    seen: Mutex<Vec<Seen>>,
}

impl Mock {
    fn on(self, method: HttpMethod, suffix: &'static str, answers: &[Canned]) -> Self {
        self.routes
            .lock()
            .unwrap()
            .push((method, suffix, answers.iter().cloned().collect()));
        self
    }

    fn ok(self, method: HttpMethod, suffix: &'static str, status: u16, body: &'static str) -> Self {
        self.on(method, suffix, &[Canned::Status(status, body)])
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn count(&self, method: HttpMethod, suffix: &str) -> usize {
        self.requests()
            .iter()
            .filter(|s| s.method == method && s.url.ends_with(suffix))
            .count()
    }
}

#[async_trait]
impl HttpTransport for Mock {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.seen.lock().unwrap().push(Seen {
            method: request.method,
            url: request.url.clone(),
            bearer: request.bearer.clone(),
            body: request.body.clone(),
        });
        let mut routes = self.routes.lock().unwrap();
        let hit = routes.iter_mut().find(|(m, s, _)| {
            *m == request.method
                && (request.url.ends_with(*s) || (s.ends_with('=') && request.url.contains(*s)))
        });
        let Some((_, _, answers)) = hit else {
            return Ok(HttpResponse::new(500, b"unrouted".to_vec()));
        };
        let answer = if answers.len() > 1 {
            answers.pop_front().unwrap()
        } else {
            answers.front().cloned().unwrap()
        };
        match answer {
            Canned::Status(status, body) => {
                // A secret lookup answers with whatever name was asked for.
                let asked = request.url.split_once("?name=").map_or("", |(_, n)| n);
                let body = body.replace("{NAME}", asked);
                Ok(HttpResponse::new(status, body.into_bytes()))
            }
            Canned::Timeout => Err(TransportError::Timeout),
        }
    }
}

const NO_ENDPOINTS: &str = r#"{"endpoints":[],"pagination":{"hasNextPage":false}}"#;
const ONE_ENDPOINT: &str =
    r#"{"endpoints":[{"id":"ep1","name":"x"}],"pagination":{"hasNextPage":false}}"#;
const TWO_ENDPOINTS: &str = r#"{"endpoints":[{"id":"ep1","name":"x"},{"id":"ep2","name":"x"}],"pagination":{"hasNextPage":false}}"#;
const ENDPOINT: &str = r#"{"id":"ep1","name":"x","type":"LOAD_BALANCER"}"#;
const SECRET: &str = r#"{"id":"s1","name":"x"}"#;
const FOUND_SECRET: &str = r#"{"secrets":[{"id":"s1","name":"{NAME}"}]}"#;
const NO_SECRETS: &str = r#"{"secrets":[]}"#;
const HEALTH: &str = "https://ep1.api.runpod.ai/health";

const FAST: ReadinessTiming = ReadinessTiming {
    timeout: Duration::from_millis(200),
    initial_backoff: Duration::from_millis(1),
    max_backoff: Duration::from_millis(5),
};

fn gpu(model: &str, count: u32) -> Accelerator {
    Accelerator {
        kind: AcceleratorKind::Gpu,
        model: model.to_owned(),
        count,
        memory: ByteSize::from_bytes(0),
        memory_bandwidth_gbps: None,
    }
}

/// The names the mock's "found" rows carry do not matter: the client filters
/// by exact name, so make the rows match the supervisor's endpoint name.
fn named(body: &'static str) -> &'static str {
    let name = project_endpoint_name(PROJECT, DEP);
    Box::leak(
        body.replace("\"name\":\"x\"", &format!("\"name\":\"{name}\""))
            .into_boxed_str(),
    )
}

/// Live-catalog shape: the H100 pool holds a card the static table lacks.
const CATALOG: &str = r#"{"gpus":[
    {"id":"NVIDIA H100 80GB HBM3","name":"H100 SXM","pool":"ADA_80_PRO","memory":80},
    {"id":"NVIDIA H100 PCIe","name":"H100 PCIe","pool":"ADA_80_PRO","memory":80},
    {"id":"NVIDIA H100 NVL","name":"H100 NVL","pool":"ADA_80_PRO","memory":94},
    {"id":"NVIDIA H100 Future","name":"H100 X","pool":"ADA_80_PRO","memory":80},
    {"id":"NVIDIA L40S","name":"L40S","pool":"ADA_48_PRO","memory":48}]}"#;

/// Every supervisor's mock also answers the GPU catalog (routes added by a
/// test come first, so a test can override it).
fn supervisor(mock: Mock) -> (RunPodSupervisor, Arc<Mock>) {
    let mock = mock.ok(HttpMethod::Get, "/catalog/gpus", 200, CATALOG);
    let mock = Arc::new(mock);
    let client = RunPodClient::with_transport(ApiKey::new(MGMT), mock.clone(), BASE);
    let sup = RunPodSupervisor::new(client, PROJECT, DEP)
        .with_probe_transport(mock.clone())
        .with_accelerator(Some(&gpu("H100", 2)))
        .unwrap()
        .with_readiness_timing(FAST);
    (sup, mock)
}

fn plan(argv: &[&str], settings: Runtime) -> LaunchPlan {
    let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
    LaunchPlan {
        deployment: DeploymentId::from(DEP),
        runtime: RuntimeKind::of(&settings),
        rendered: RenderedLaunch::Command {
            text: argv.join(" "),
            argv,
        },
        settings,
        secret_refs: Vec::new(),
        api_key: None,
        service_label: None,
        estimate: ResourceEstimate {
            estimated_memory: ByteSize::from_bytes(0),
            gpu_count: 2,
            kv_cache: None,
        },
    }
}

fn vllm_plan() -> LaunchPlan {
    plan(
        &[
            "vllm",
            "serve",
            "meta-llama/Llama-3.1-8B-Instruct",
            "--port",
            "8000",
            "--host",
            "127.0.0.1",
        ],
        Runtime::Vllm(VllmSettings {
            port: 8000,
            ..Default::default()
        }),
    )
}

fn llama_plan() -> LaunchPlan {
    plan(
        &[
            "llama-server",
            "-m",
            "models/gemma/gemma-q4_k_m.gguf",
            "--port",
            "8080",
            "--host",
            "127.0.0.1",
        ],
        Runtime::LlamaCpp(LlamaCppSettings {
            port: 8080,
            ..Default::default()
        }),
    )
}

fn key_secret(var: &str) -> Vec<ResolvedSecret> {
    vec![ResolvedSecret {
        name: var.to_owned(),
        value: SecretValue::new(KEY.to_owned()),
    }]
}

fn happy_mock() -> Mock {
    Mock::default()
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .ok(HttpMethod::Get, "/serverless", 200, NO_ENDPOINTS)
        .ok(HttpMethod::Post, "/serverless", 201, ENDPOINT)
        .ok(HttpMethod::Get, HEALTH, 200, "{}")
}

fn posted(mock: &Mock, suffix: &str) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|s| s.method == HttpMethod::Post && s.url.ends_with(suffix))
        .filter_map(|s| s.body)
        .collect()
}

fn key_name(var: &str) -> String {
    secret_name(PROJECT, DEP, var)
}

// --- naming / templating ---------------------------------------------------

#[test]
fn secret_names_are_deterministic_scoped_and_runpod_safe() {
    let a = secret_name(PROJECT, DEP, "VLLM_API_KEY");
    assert_eq!(a, secret_name(PROJECT, DEP, "VLLM_API_KEY"), "stable");
    assert!(a.starts_with("amicu-aaaa1111-chat-vllm-api-key-"), "{a}");
    assert!(a.len() <= 63);
    assert!(
        a.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    );
    assert_ne!(a, secret_name(PROJECT, DEP, "HF_TOKEN"));
    assert_ne!(a, secret_name(PROJECT, "other", "VLLM_API_KEY"));
    assert_ne!(a, secret_name("bbbb2222", DEP, "VLLM_API_KEY"));
    assert_eq!(secret_reference("n-1"), "{{ RUNPOD_SECRET_n-1 }}");
}

#[test]
fn planned_secret_names_cover_every_secret_and_the_standard_ones() {
    let (sup, _) = supervisor(Mock::default());
    let mut secrets = key_secret("VLLM_API_KEY");
    secrets.push(ResolvedSecret {
        name: "my_token".into(),
        value: SecretValue::new("v".to_owned()),
    });
    let names = sup.planned_secret_names(&secrets);
    for var in ["VLLM_API_KEY", "LLAMA_API_KEY", "HF_TOKEN", "my_token"] {
        assert!(names.contains(&key_name(var)), "{var}");
    }
    assert_eq!(names.len(), 4, "deduplicated");
}

// --- create bodies -----------------------------------------------------------

#[tokio::test]
async fn vllm_start_creates_the_secret_then_the_endpoint_and_waits_for_health() {
    let (sup, mock) = supervisor(happy_mock());
    let sup = sup
        .with_region(Some(&Region {
            code: "eu".into(),
            residency: Residency::Eu,
            display_name: None,
            country: None,
            default_zone: None,
        }))
        .unwrap();
    let handle = sup
        .start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap();
    assert_eq!(handle.id, "ep1");
    assert_eq!(handle.endpoint, "https://ep1.api.runpod.ai");
    assert_eq!(
        handle.exposure,
        EndpointExposure::Public {
            gateway: GatewayAuth::RunPodBearer
        }
    );
    assert_eq!(
        sup.deployed_endpoint().lock().unwrap().as_deref(),
        Some("ep1")
    );

    // The secret holds the key; it is written before the endpoint.
    let order: Vec<(HttpMethod, String)> = mock
        .requests()
        .into_iter()
        .map(|s| (s.method, s.url))
        .collect();
    let secret_at = order
        .iter()
        .position(|(m, u)| *m == HttpMethod::Post && u.ends_with("/account/secrets"))
        .unwrap();
    let create_at = order
        .iter()
        .position(|(m, u)| *m == HttpMethod::Post && u.ends_with("/serverless"))
        .unwrap();
    assert!(secret_at < create_at);
    let secret = &posted(&mock, "/account/secrets")[0];
    assert_eq!(secret["name"], json!(key_name("VLLM_API_KEY")));
    assert_eq!(secret["value"], json!(KEY));
    assert_eq!(
        *sup.created_secrets().lock().unwrap(),
        vec![key_name("VLLM_API_KEY")]
    );

    let body = &posted(&mock, "/serverless")[0];
    let name = project_endpoint_name(PROJECT, DEP);
    assert_eq!(body["name"], json!(name));
    assert_eq!(body["type"], json!("LOAD_BALANCER"));
    assert_eq!(body["image"], json!(remote::DEFAULT_VLLM_IMAGE));
    assert_eq!(body["entrypoint"], json!(["vllm"]));
    assert_eq!(
        body["cmd"],
        json!([
            "serve",
            "meta-llama/Llama-3.1-8B-Instruct",
            "--port",
            "8000",
            "--host",
            "0.0.0.0"
        ])
    );
    assert_eq!(
        body["env"],
        json!({
            "HEALTH_CHECK_PATH": "/health",
            "PORT": "8000",
            "PORT_HEALTH": "8000",
            "VLLM_API_KEY": format!("{{{{ RUNPOD_SECRET_{} }}}}", key_name("VLLM_API_KEY")),
        })
    );
    assert_eq!(body["ports"], json!(["8000/http"]));
    assert_eq!(body["disk"], json!(DEFAULT_CONTAINER_DISK_GB));
    assert_eq!(body["gpu"]["pools"], json!(["ADA_80_PRO"]));
    assert_eq!(body["gpu"]["count"], json!(2));
    // Exclusions come from the live catalog, including the card the static
    // table does not know.
    assert_eq!(
        body["gpu"]["excludedTypes"],
        json!(["NVIDIA H100 PCIe", "NVIDIA H100 NVL", "NVIDIA H100 Future"])
    );
    assert_eq!(
        body["workers"],
        json!({"min": 0, "max": DEFAULT_MAX_WORKERS, "idleTimeout": DEFAULT_IDLE_TIMEOUT_SECS})
    );
    assert_eq!(
        body["scaling"],
        json!({"type": "REQUEST_COUNT", "requestCount": DEFAULT_REQUEST_COUNT})
    );
    let dcs = body["dataCenterIds"].as_array().unwrap();
    assert_ne!(dcs.len(), 0);
    assert!(
        dcs.iter().all(|d| d.as_str().unwrap().starts_with("EU")),
        "{dcs:?}"
    );
    // The key is never in the endpoint config, only its template.
    assert!(!body.to_string().contains(KEY));

    // Management calls carry the RunPod key; the health probe the end-user key.
    for s in mock.requests() {
        let want = if s.url == HEALTH { KEY } else { MGMT };
        assert_eq!(s.bearer.as_deref(), Some(want), "{}", s.url);
    }
}

#[tokio::test]
async fn llama_start_runs_the_image_server_with_an_hf_gguf() {
    let (sup, mock) = supervisor(happy_mock());
    let sup = sup.with_scaling(Some(&ScalingSettings {
        min_replicas: Some(1),
        max_replicas: Some(5),
        target_concurrency: Some(4),
        scaledown_window_secs: Some(99_999),
    }));
    let mut sup = sup;
    sup.gguf = Some(GgufSource {
        repo: "ggml-org/gemma-3-1b-it-GGUF".into(),
        file: "gemma-q4_k_m.gguf".into(),
    });
    sup.start(&llama_plan(), &key_secret("LLAMA_API_KEY"))
        .await
        .unwrap();
    let body = &posted(&mock, "/serverless")[0];
    assert_eq!(body["image"], json!(remote::DEFAULT_LLAMACPP_IMAGE));
    assert_eq!(body["entrypoint"], json!(["/app/llama-server"]));
    assert_eq!(
        body["cmd"],
        json!([
            "-hf",
            "ggml-org/gemma-3-1b-it-GGUF",
            "-hff",
            "gemma-q4_k_m.gguf",
            "--port",
            "8080",
            "--host",
            "0.0.0.0",
            "--no-mmproj"
        ])
    );
    assert_eq!(body["env"]["PORT"], json!("8080"));
    assert_eq!(
        body["env"]["LLAMA_API_KEY"],
        json!(secret_reference(&key_name("LLAMA_API_KEY")))
    );
    assert_eq!(
        body["workers"],
        json!({"min": 1, "max": 5, "idleTimeout": MAX_IDLE_TIMEOUT_SECS})
    );
    assert_eq!(body["scaling"]["requestCount"], json!(4));
    assert!(
        body.get("dataCenterIds").is_none(),
        "no region: RunPod picks"
    );
}

#[test]
fn every_secret_goes_through_a_runpod_secret_and_plain_env_stays_plain() {
    let (sup, _) = supervisor(Mock::default());
    let mut p = vllm_plan();
    if let Runtime::Vllm(s) = &mut p.settings {
        s.env = BTreeMap::from([("VLLM_LOGGING_LEVEL".to_owned(), "DEBUG".to_owned())]);
    }
    let prepared = sup
        .prepare(
            &p,
            &key_secret("VLLM_API_KEY"),
            "img",
            Some("hf_operator_token"),
        )
        .unwrap();
    let env = &prepared.endpoint.env;
    assert_eq!(env["VLLM_LOGGING_LEVEL"], "DEBUG");
    assert_eq!(
        env["HF_TOKEN"],
        secret_reference(&key_name("HF_TOKEN")),
        "the operator HF token fallback is a secret too"
    );
    let names: Vec<&str> = prepared.secrets.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&key_name("HF_TOKEN").as_str()));
    let dbg = format!("{prepared:?} {sup:?}");
    assert!(
        !dbg.contains(KEY) && !dbg.contains("hf_operator_token"),
        "{dbg}"
    );
    assert!(!dbg.contains(MGMT));
}

#[test]
fn prepare_refuses_what_runpod_cannot_run_safely() {
    let (sup, _) = supervisor(Mock::default());
    // No api_key: the endpoint would only be guarded by the gateway.
    let err = sup
        .prepare(&vllm_plan(), &[], "img", None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("need an `api_key`"), "{err}");

    // A reserved load-balancer variable in the env block.
    let mut p = vllm_plan();
    if let Runtime::Vllm(s) = &mut p.settings {
        s.env = BTreeMap::from([("PORT".to_owned(), "1".to_owned())]);
    }
    let err = sup
        .prepare(&p, &key_secret("VLLM_API_KEY"), "img", None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("'PORT' is reserved"), "{err}");

    // A local model path, reported with the RunPod name.
    let mut p = vllm_plan();
    if let RenderedLaunch::Command { argv, .. } = &mut p.rendered {
        argv[2] = "/models/llama".into();
    }
    let err = sup
        .prepare(&p, &key_secret("VLLM_API_KEY"), "img", None)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("RunPod deployments need a Hugging Face"),
        "{err}"
    );

    // No GPU.
    let mock = Arc::new(Mock::default());
    let client = RunPodClient::with_transport(ApiKey::new(MGMT), mock, BASE);
    let err = RunPodSupervisor::new(client, PROJECT, DEP)
        .prepare(&vllm_plan(), &key_secret("VLLM_API_KEY"), "img", None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs a GPU"), "{err}");
}

#[test]
fn a_tpu_or_unknown_gpu_is_rejected_before_any_call() {
    let mock = Arc::new(Mock::default());
    let client = RunPodClient::with_transport(ApiKey::new(MGMT), mock.clone(), BASE);
    let tpu = Accelerator {
        kind: AcceleratorKind::Tpu,
        ..gpu("v5e", 1)
    };
    assert!(
        RunPodSupervisor::new(client, PROJECT, DEP)
            .with_accelerator(Some(&tpu))
            .is_err()
    );
    assert!(mock.requests().is_empty());
}

// --- create / ambiguity --------------------------------------------------------

#[tokio::test]
async fn an_ambiguous_create_is_resolved_by_name_and_never_resent() {
    let mock = ambiguous_then(named(ONE_ENDPOINT)).ok(HttpMethod::Get, HEALTH, 200, "{}");
    let (sup, mock) = supervisor(mock);
    let handle = sup
        .start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap();
    assert_eq!(handle.id, "ep1");
    assert_eq!(
        mock.count(HttpMethod::Post, "/serverless"),
        1,
        "never retried"
    );
    assert_eq!(mock.count(HttpMethod::Get, "/serverless"), 2);
    assert_eq!(
        sup.deployed_endpoint().lock().unwrap().as_deref(),
        Some("ep1")
    );
}

fn ambiguous_then(after: &'static str) -> Mock {
    Mock::default()
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .on(
            HttpMethod::Get,
            "/serverless",
            &[
                Canned::Status(200, NO_ENDPOINTS),
                Canned::Status(200, after),
            ],
        )
        .on(HttpMethod::Post, "/serverless", &[Canned::Timeout])
}

#[tokio::test]
async fn an_ambiguous_create_with_nothing_found_fails_without_retrying() {
    let (sup, mock) = supervisor(ambiguous_then(NO_ENDPOINTS));
    let err = sup
        .start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("outcome unknown"), "{err}");
    assert!(err.contains("not retried"), "{err}");
    assert_eq!(mock.count(HttpMethod::Post, "/serverless"), 1);
    assert!(sup.deployed_endpoint().lock().unwrap().is_none());
    // The secret is kept for the caller: the endpoint may still appear.
    assert_eq!(sup.created_secrets().lock().unwrap().len(), 1);
    assert!(!err.contains(KEY));
}

#[tokio::test]
async fn an_ambiguous_create_that_finds_duplicates_refuses_to_pick() {
    let (sup, mock) = supervisor(ambiguous_then(named(TWO_ENDPOINTS)));
    let err = sup
        .start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("ep1, ep2"), "{err}");
    assert_eq!(mock.count(HttpMethod::Post, "/serverless"), 1);
}

#[tokio::test]
async fn an_existing_endpoint_is_updated_in_place_not_duplicated() {
    let mock = Mock::default()
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .ok(HttpMethod::Get, "/serverless", 200, named(ONE_ENDPOINT))
        .ok(HttpMethod::Patch, "/serverless/ep1", 200, ENDPOINT)
        .ok(HttpMethod::Get, HEALTH, 200, "{}");
    let (sup, mock) = supervisor(mock);
    let handle = sup
        .start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap();
    assert_eq!(handle.id, "ep1");
    assert_eq!(mock.count(HttpMethod::Post, "/serverless"), 0);
    let patch = mock
        .requests()
        .into_iter()
        .find(|s| s.method == HttpMethod::Patch)
        .unwrap()
        .body
        .unwrap();
    assert_eq!(patch["entrypoint"], json!(["vllm"]));
    assert!(patch.get("name").is_none());
    assert!(!patch.to_string().contains(KEY));
}

#[tokio::test]
async fn a_catalog_that_does_not_list_the_card_stops_before_any_write() {
    let mock = happy_mock().ok(
        HttpMethod::Get,
        "/catalog/gpus",
        200,
        r#"{"gpus":[{"id":"NVIDIA H100 PCIe","name":"x","pool":"ADA_80_PRO"}]}"#,
    );
    let (sup, mock) = supervisor(mock);
    let err = sup
        .start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not list"), "{err}");
    assert!(
        mock.requests().iter().all(|s| s.method == HttpMethod::Get),
        "nothing created"
    );

    // An unreadable catalog also stops the deploy (never guess the exclusions).
    let (sup, mock) = supervisor(happy_mock().ok(HttpMethod::Get, "/catalog/gpus", 500, "{}"));
    assert!(
        sup.start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
            .await
            .is_err()
    );
    assert_eq!(mock.count(HttpMethod::Post, "/account/secrets"), 0);
}

#[tokio::test]
async fn a_refused_create_removes_the_secrets_it_wrote() {
    let mock = Mock::default()
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .ok(HttpMethod::Get, "/serverless", 200, NO_ENDPOINTS)
        .ok(
            HttpMethod::Post,
            "/serverless",
            400,
            r#"{"detail":"bad gpu"}"#,
        )
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, FOUND_SECRET)
        .ok(HttpMethod::Delete, "/account/secrets/s1", 204, "");
    let (sup, mock) = supervisor(mock);
    let err = sup
        .start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP 400"), "{err}");
    assert!(!err.contains(KEY));
    assert_eq!(mock.count(HttpMethod::Delete, "/account/secrets/s1"), 1);
    assert!(sup.created_secrets().lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_existing_secret_is_replaced_with_the_current_value() {
    let mock = Mock::default()
        .on(
            HttpMethod::Post,
            "/account/secrets",
            &[
                Canned::Status(409, r#"{"detail":"exists"}"#),
                Canned::Status(201, SECRET),
            ],
        )
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, FOUND_SECRET)
        .ok(HttpMethod::Delete, "/account/secrets/s1", 204, "")
        .ok(HttpMethod::Get, "/serverless", 200, NO_ENDPOINTS)
        .ok(HttpMethod::Post, "/serverless", 201, ENDPOINT)
        .ok(HttpMethod::Get, HEALTH, 200, "{}");
    let (sup, mock) = supervisor(mock);
    sup.start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap();
    assert_eq!(mock.count(HttpMethod::Post, "/account/secrets"), 2);
    assert_eq!(mock.count(HttpMethod::Delete, "/account/secrets/s1"), 1);
    assert!(
        posted(&mock, "/account/secrets")
            .iter()
            .all(|b| b["value"] == json!(KEY))
    );
}

// --- readiness -------------------------------------------------------------------

fn ready_mock(health: &[Canned]) -> Mock {
    Mock::default()
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .ok(HttpMethod::Get, "/serverless", 200, NO_ENDPOINTS)
        .ok(HttpMethod::Post, "/serverless", 201, ENDPOINT)
        .on(HttpMethod::Get, HEALTH, health)
}

#[tokio::test]
async fn readiness_waits_through_initializing_and_cold_start_answers() {
    let (sup, mock) = supervisor(ready_mock(&[
        Canned::Status(204, ""),
        Canned::Timeout,
        Canned::Status(503, "no workers available"),
        Canned::Status(200, "{}"),
    ]));
    sup.start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap();
    assert_eq!(mock.count(HttpMethod::Get, HEALTH), 4);
}

#[tokio::test]
async fn readiness_times_out_naming_the_last_answer_and_keeps_the_id() {
    let (sup, _) = supervisor(ready_mock(&[Canned::Status(204, "")]));
    let err = sup
        .start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("did not become ready"), "{err}");
    assert!(err.contains("initializing (HTTP 204)"), "{err}");
    assert!(!err.contains(KEY));
    assert_eq!(
        sup.deployed_endpoint().lock().unwrap().as_deref(),
        Some("ep1")
    );
}

#[tokio::test]
async fn a_rejected_key_fails_readiness_at_once() {
    let (sup, mock) = supervisor(ready_mock(&[Canned::Status(401, "")]));
    let err = sup
        .start(&vllm_plan(), &key_secret("VLLM_API_KEY"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("rejected the deployment's api_key"), "{err}");
    assert!(!err.contains(KEY));
    assert_eq!(mock.count(HttpMethod::Get, HEALTH), 1);
}

// --- status ------------------------------------------------------------------------

fn handle() -> RuntimeHandle {
    RuntimeHandle {
        id: "ep1".into(),
        deployment: DeploymentId::from(DEP),
        endpoint: endpoint_url("ep1"),
        exposure: EndpointExposure::Public {
            gateway: GatewayAuth::RunPodBearer,
        },
    }
}

async fn status_with(workers: &'static str) -> RuntimeStatus {
    let mock = Mock::default()
        .ok(HttpMethod::Get, "/serverless/ep1", 200, ENDPOINT)
        .ok(HttpMethod::Get, "/serverless/ep1/workers", 200, workers);
    supervisor(mock).0.status(&handle()).await.unwrap()
}

#[tokio::test]
async fn status_reads_the_worker_summary() {
    let ready = status_with(r#"{"workers":[],"summary":{"running":1,"idle":1}}"#).await;
    assert!(ready.is_running(), "{ready:?}");
    let zero = status_with(r#"{"workers":[],"summary":{}}"#).await;
    assert!(
        matches!(&zero, RuntimeStatus::Running { detail } if detail.contains("scaled to zero"))
    );
    let starting = status_with(r#"{"workers":[],"summary":{"initializing":1}}"#).await;
    assert!(matches!(&starting, RuntimeStatus::Failed { detail } if detail.contains("not ready")));
    let sick = status_with(r#"{"workers":[],"summary":{"unhealthy":2}}"#).await;
    assert!(matches!(&sick, RuntimeStatus::Failed { detail } if detail.contains("unhealthy")));

    let gone = supervisor(Mock::default().ok(HttpMethod::Get, "/serverless/ep1", 404, "{}"))
        .0
        .status(&handle())
        .await
        .unwrap();
    assert!(matches!(gone, RuntimeStatus::Stopped { .. }));
}

#[tokio::test]
async fn metrics_and_logs_say_they_are_not_supported_yet() {
    let (sup, mock) = supervisor(Mock::default());
    let m = sup.metrics(&handle()).await.unwrap_err().to_string();
    assert!(m.contains("not yet supported for RunPod"), "{m}");
    let l = sup
        .logs(&handle(), LogOptions::default())
        .await
        .unwrap_err()
        .to_string();
    assert!(l.contains("not yet supported for RunPod"), "{l}");
    assert!(mock.requests().is_empty());
}

// --- stop --------------------------------------------------------------------------

fn stop_mock(endpoint_status: u16) -> Mock {
    Mock::default()
        .ok(HttpMethod::Delete, "/serverless/ep1", endpoint_status, "")
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, FOUND_SECRET)
        .ok(HttpMethod::Delete, "/account/secrets/s1", 204, "")
}

#[tokio::test]
async fn stop_deletes_the_endpoint_then_its_secrets() {
    for status in [204, 404] {
        let (sup, mock) = supervisor(stop_mock(status));
        let sup = sup.with_recorded_secrets(vec!["recorded-secret".into()]);
        sup.stop(&handle()).await.unwrap();
        let seen = mock.requests();
        assert_eq!(seen[0].method, HttpMethod::Delete);
        assert!(seen[0].url.ends_with("/serverless/ep1"), "endpoint first");
        let looked_up: Vec<String> = seen
            .iter()
            .filter(|s| s.url.contains("/account/secrets?name="))
            .map(|s| s.url.clone())
            .collect();
        assert!(
            looked_up
                .iter()
                .any(|u| u.ends_with("name=recorded-secret"))
        );
        let api_key = key_name("VLLM_API_KEY");
        assert!(
            looked_up
                .iter()
                .any(|u| u.ends_with(&format!("name={api_key}")))
        );
        assert_eq!(
            mock.count(HttpMethod::Delete, "/account/secrets/s1"),
            looked_up.len(),
            "status {status}"
        );
    }
}

#[tokio::test]
async fn secrets_stay_when_the_endpoint_delete_is_not_confirmed() {
    let (sup, mock) = supervisor(stop_mock(500));
    let err = sup.stop(&handle()).await.unwrap_err().to_string();
    assert!(err.contains("500"), "{err}");
    assert_eq!(
        mock.requests().len(),
        1,
        "no secret call after a failed endpoint delete"
    );
}

#[tokio::test]
async fn a_secret_that_cannot_be_deleted_is_reported_and_kept() {
    let mock = Mock::default()
        .ok(HttpMethod::Delete, "/serverless/ep1", 204, "")
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, NO_SECRETS);
    let (sup, _) = supervisor(mock);
    sup.stop(&handle()).await.unwrap();

    let mock = Mock::default()
        .ok(HttpMethod::Delete, "/serverless/ep1", 204, "")
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, FOUND_SECRET)
        .ok(HttpMethod::Delete, "/account/secrets/s1", 500, "");
    let (sup, _) = supervisor(mock);
    sup.created_secrets()
        .lock()
        .unwrap()
        .push("made-here".into());
    let err = sup.stop(&handle()).await.unwrap_err().to_string();
    assert!(
        err.contains("was deleted, but these secrets could not be"),
        "{err}"
    );
    assert!(err.contains("`made-here`"), "{err}");
    assert_eq!(
        *sup.created_secrets().lock().unwrap(),
        vec!["made-here".to_owned()]
    );
}
