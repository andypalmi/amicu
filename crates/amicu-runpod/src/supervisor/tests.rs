use std::collections::{BTreeMap, VecDeque};
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
pub(crate) const KEY: &str = "rpa_ENDUSERKEY0123456789";
/// The operator's RunPod management key.
pub(crate) const MGMT: &str = "rpa_MANAGEMENTKEY";
pub(crate) const BASE: &str = "https://api.runpod.io/v2";
pub(crate) const PROJECT: &str = "aaaa1111";
pub(crate) const DEP: &str = "chat";

#[derive(Clone)]
pub(crate) enum Canned {
    Status(u16, &'static str),
    Timeout,
}

pub(crate) type Route = (HttpMethod, &'static str, VecDeque<Canned>);

#[derive(Clone)]
pub(crate) struct Seen {
    pub(crate) method: HttpMethod,
    pub(crate) url: String,
    pub(crate) bearer: Option<String>,
    pub(crate) body: Option<Value>,
}

/// Canned answers by `(method, url suffix)`; a suffix ending in `=` matches
/// any URL containing it (query lookups). Each route plays its answers in
/// order and repeats the last one. Every request is recorded.
#[derive(Default)]
pub(crate) struct Mock {
    routes: Mutex<Vec<Route>>,
    seen: Mutex<Vec<Seen>>,
}

impl Mock {
    pub(crate) fn on(self, method: HttpMethod, suffix: &'static str, answers: &[Canned]) -> Self {
        self.routes
            .lock()
            .unwrap()
            .push((method, suffix, answers.iter().cloned().collect()));
        self
    }

    /// Like [`on`](Self::on), but matched before every route added so far.
    pub(crate) fn first(
        self,
        method: HttpMethod,
        suffix: &'static str,
        answers: &[Canned],
    ) -> Self {
        self.routes
            .lock()
            .unwrap()
            .insert(0, (method, suffix, answers.iter().cloned().collect()));
        self
    }

    pub(crate) fn ok(
        self,
        method: HttpMethod,
        suffix: &'static str,
        status: u16,
        body: &'static str,
    ) -> Self {
        self.on(method, suffix, &[Canned::Status(status, body)])
    }

    pub(crate) fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub(crate) fn count(&self, method: HttpMethod, suffix: &str) -> usize {
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
pub(crate) const SECRET: &str = r#"{"id":"s1","name":"x"}"#;
pub(crate) const FOUND_SECRET: &str = r#"{"secrets":[{"id":"s1","name":"{NAME}"}]}"#;
pub(crate) const NO_SECRETS: &str = r#"{"secrets":[]}"#;
const HEALTH: &str = "https://ep1.api.runpod.ai/health";

pub(crate) const FAST: ReadinessTiming = ReadinessTiming {
    timeout: Duration::from_millis(200),
    initial_backoff: Duration::from_millis(1),
    max_backoff: Duration::from_millis(5),
};

fn gpu(model: &str, count: u32) -> Accelerator {
    Accelerator {
        kind: AcceleratorKind::Gpu,
        model: model.to_owned(),
        count,
        memory: ByteSize::from_bytes(80 * 1024 * 1024 * 1024),
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
        .with_accelerator(Some(&gpu("h100-sxm-80gb", 2)))
        .unwrap()
        .with_readiness_timing(FAST);
    (sup, mock)
}

pub(crate) fn plan(argv: &[&str], settings: Runtime) -> LaunchPlan {
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

pub(crate) fn vllm_plan() -> LaunchPlan {
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

pub(crate) fn llama_plan() -> LaunchPlan {
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

/// The operator's Hugging Face token (a RunPod secret, unlike the api_key).
pub(crate) const HF: &str = "hf_HUGGINGFACETOKEN0123";

/// The deployment's `api_key` under the runtime's variable, plus `HF_TOKEN`
/// (passed explicitly so a `HF_TOKEN` in the test environment never adds a
/// fallback).
pub(crate) fn secrets_for(var: &str) -> Vec<ResolvedSecret> {
    vec![
        ResolvedSecret {
            name: var.to_owned(),
            value: SecretValue::new(KEY.to_owned()),
        },
        ResolvedSecret {
            name: HF_TOKEN_ENV_VAR.to_owned(),
            value: SecretValue::new(HF.to_owned()),
        },
    ]
}

pub(crate) fn vllm_secrets() -> Vec<ResolvedSecret> {
    secrets_for("VLLM_API_KEY")
}

pub(crate) fn owner() -> String {
    owner_marker(None, PROJECT, DEP)
}

pub(crate) fn sname(var: &str, value: &str) -> String {
    secret_name(PROJECT, DEP, &owner(), var, value)
}

fn hf_name() -> String {
    sname(HF_TOKEN_ENV_VAR, HF)
}

/// `GET /serverless/ep1` for an endpoint carrying this deployment's marker.
fn owned_endpoint() -> &'static str {
    Box::leak(
        format!(
            r#"{{"id":"ep1","name":"{}","env":{{"{OWNER_ENV_VAR}":"{}","HF_TOKEN":"x"}}}}"#,
            project_endpoint_name(PROJECT, DEP),
            owner()
        )
        .into_boxed_str(),
    )
}

fn happy_mock() -> Mock {
    Mock::default()
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, NO_SECRETS)
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .ok(HttpMethod::Get, "/serverless", 200, NO_ENDPOINTS)
        .ok(HttpMethod::Post, "/serverless", 201, ENDPOINT)
        .ok(HttpMethod::Get, HEALTH, 200, "{}")
}

pub(crate) fn posted(mock: &Mock, suffix: &str) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|s| s.method == HttpMethod::Post && s.url.ends_with(suffix))
        .filter_map(|s| s.body)
        .collect()
}

pub(crate) fn position(mock: &Mock, method: HttpMethod, suffix: &str) -> usize {
    mock.requests()
        .iter()
        .position(|s| s.method == method && s.url.ends_with(suffix))
        .unwrap_or_else(|| panic!("no {method:?} {suffix}"))
}

// --- naming / templating ---------------------------------------------------

#[test]
fn secret_names_are_content_addressed_scoped_and_runpod_safe() {
    let o = owner();
    let a = secret_name(PROJECT, DEP, &o, "HF_TOKEN", "v1");
    assert_eq!(a, secret_name(PROJECT, DEP, &o, "HF_TOKEN", "v1"), "stable");
    assert!(a.starts_with("amicu-aaaa1111-chat-hf-token-"), "{a}");
    assert!(a.len() <= 63);
    assert!(
        a.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    );
    assert_ne!(a, secret_name(PROJECT, DEP, &o, "HF_TOKEN", "v2"), "value");
    assert_ne!(a, secret_name(PROJECT, DEP, &o, "OTHER", "v1"), "variable");
    assert_ne!(a, secret_name(PROJECT, "other", &o, "HF_TOKEN", "v1"));
    assert_ne!(a, secret_name("bbbb2222", DEP, &o, "HF_TOKEN", "v1"));
    let other_ws = owner_marker(Some("ws2"), PROJECT, DEP);
    assert_ne!(o, other_ws, "the workspace is part of the owner");
    assert_ne!(a, secret_name(PROJECT, DEP, &other_ws, "HF_TOKEN", "v1"));
    // The value never appears in the name.
    let long = "hf_abcdefghijklmnopqrstuvwxyz";
    assert!(!secret_name(PROJECT, DEP, &o, "HF_TOKEN", long).contains("abcdefgh"));
    assert_eq!(secret_reference("n-1"), "{{ RUNPOD_SECRET_n-1 }}");
}

#[test]
fn planned_secret_names_are_exactly_what_start_writes_without_the_api_key() {
    let (sup, _) = supervisor(Mock::default());
    let mut secrets = vllm_secrets();
    secrets.push(ResolvedSecret {
        name: "my_token".into(),
        value: SecretValue::new("v".to_owned()),
    });
    let names = sup.planned_secret_names(&vllm_plan(), &secrets).unwrap();
    let mut want = vec![hf_name(), sname("my_token", "v")];
    want.sort();
    assert_eq!(names, want);
    // Missing api_key: refused before any call, like start.
    assert!(sup.planned_secret_names(&vllm_plan(), &[]).is_err());
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
    let handle = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
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
    assert!(sup.created_endpoint().load(Ordering::SeqCst));

    // Only HF_TOKEN becomes a RunPod secret, written before the endpoint;
    // the api_key is a RunPod credential and never leaves this machine
    // except as the gateway bearer.
    assert!(
        position(&mock, HttpMethod::Post, "/account/secrets")
            < position(&mock, HttpMethod::Post, "/serverless")
    );
    let written = posted(&mock, "/account/secrets");
    assert_eq!(written.len(), 1);
    assert_eq!(written[0]["name"], json!(hf_name()));
    assert_eq!(written[0]["value"], json!(HF));
    assert_eq!(*sup.created_secrets().lock().unwrap(), vec![hf_name()]);

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
            "AMICU_OWNER": owner(),
            "HEALTH_CHECK_PATH": "/health",
            "HF_TOKEN": secret_reference(&hf_name()),
            "PORT": "8000",
            "PORT_HEALTH": "8000",
        }),
        "no runtime API key: the gateway strips Authorization"
    );
    assert_eq!(body["ports"], json!(["8000/http"]));
    assert_eq!(body["disk"], json!(DEFAULT_CONTAINER_DISK_GB));
    assert_eq!(body["gpu"]["pools"], json!(["ADA_80_PRO"]));
    assert_eq!(body["gpu"]["count"], json!(2));
    assert_eq!(body["gpu"]["minCudaVersion"], json!("13.0"));
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
    // No value is in the endpoint config, only templates.
    assert!(!body.to_string().contains(KEY));
    assert!(!body.to_string().contains(HF));

    // Management calls carry the RunPod key; the health probe the end-user
    // key, and no other request carries it at all.
    for s in mock.requests() {
        let want = if s.url == HEALTH { KEY } else { MGMT };
        assert_eq!(s.bearer.as_deref(), Some(want), "{}", s.url);
        if s.url != HEALTH {
            assert!(
                !s.body
                    .map(|b| b.to_string())
                    .unwrap_or_default()
                    .contains(KEY),
                "{}",
                s.url
            );
        }
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
    sup.start(&llama_plan(), &secrets_for("LLAMA_API_KEY"))
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
    assert_eq!(body["gpu"]["minCudaVersion"], json!("12.8"));
    assert!(body["env"].get("LLAMA_API_KEY").is_none());
    assert!(!body.to_string().contains(KEY));
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
fn llama_serves_on_the_stock_image_port_whatever_the_deployment_says() {
    let (mut sup, _) = supervisor(Mock::default());
    sup.gguf = Some(GgufSource {
        repo: "org/g-GGUF".into(),
        file: "g.gguf".into(),
    });
    let plan = plan(
        &["llama-server", "-m", "m.gguf", "--port", "8000"],
        Runtime::LlamaCpp(LlamaCppSettings {
            port: 8000,
            ..Default::default()
        }),
    );
    let argv = sup.launch_argv(&plan).unwrap();
    let i = argv.iter().position(|a| a == "--port").unwrap();
    assert_eq!(argv[i + 1], "8080");
    assert_eq!(argv.iter().filter(|a| *a == "--port").count(), 1);
    assert_eq!(runtime_port(&plan), remote::LLAMACPP_IMAGE_PORT);
    assert_eq!(runtime_port(&vllm_plan()), 8000);
}

#[test]
fn an_overridden_image_gets_no_cuda_floor() {
    assert_eq!(
        image_min_cuda(remote::DEFAULT_LLAMACPP_IMAGE, RuntimeKind::LlamaCpp).as_deref(),
        Some("12.8")
    );
    assert_eq!(
        image_min_cuda(remote::DEFAULT_VLLM_IMAGE, RuntimeKind::Vllm).as_deref(),
        Some("13.0")
    );
    assert_eq!(image_min_cuda("me/vllm:custom", RuntimeKind::Vllm), None);
}

#[test]
fn a_dsl_region_without_residency_places_in_its_data_centers() {
    let (sup, _) = supervisor(Mock::default());
    let sup = sup.with_region(Some(&Region::from_code("eu"))).unwrap();
    assert_ne!(sup.data_center_ids.len(), 0);
    assert!(sup.data_center_ids.iter().all(|d| d.starts_with("EU")));

    let (sup, _) = supervisor(Mock::default());
    let sup = sup
        .with_region(Some(&Region::from_code("eu-ro-1")))
        .unwrap();
    assert_eq!(sup.data_center_ids, ["EU-RO-1"]);
    let err = sup
        .with_allowed_residencies(&[Residency::Us])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not within the required residency (us)"),
        "{err}"
    );

    let (sup, _) = supervisor(Mock::default());
    let sup = sup
        .with_region(None)
        .unwrap()
        .with_allowed_residencies(&[Residency::Eu])
        .unwrap();
    assert_ne!(sup.data_center_ids.len(), 0);
    assert!(sup.data_center_ids.iter().all(|d| d.starts_with("EU")));
}

#[test]
fn every_other_secret_goes_through_a_runpod_secret_and_plain_env_stays_plain() {
    let (sup, _) = supervisor(Mock::default());
    let mut p = vllm_plan();
    if let Runtime::Vllm(s) = &mut p.settings {
        s.env = BTreeMap::from([("VLLM_LOGGING_LEVEL".to_owned(), "DEBUG".to_owned())]);
    }
    let key_only = vec![ResolvedSecret {
        name: "VLLM_API_KEY".to_owned(),
        value: SecretValue::new(KEY.to_owned()),
    }];
    let prepared = sup
        .prepare(&p, &key_only, "img", Some("hf_operator_token"))
        .unwrap();
    let env = &prepared.endpoint.env;
    assert_eq!(env["VLLM_LOGGING_LEVEL"], "DEBUG");
    assert_eq!(
        env["HF_TOKEN"],
        secret_reference(&sname("HF_TOKEN", "hf_operator_token")),
        "the operator HF token fallback is a secret too"
    );
    assert!(!env.contains_key("VLLM_API_KEY"));
    let names: Vec<&str> = prepared.secrets.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec![sname("HF_TOKEN", "hf_operator_token").as_str()]);
    assert_eq!(prepared.api_key.expose_secret_str().unwrap(), KEY);
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
    // No api_key: the endpoint would have no gateway key to check.
    let err = sup
        .prepare(&vllm_plan(), &[], "img", None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("need an `api_key`"), "{err}");

    // Reserved load-balancer / owner variables in the env block.
    for reserved in ["PORT", OWNER_ENV_VAR] {
        let mut p = vllm_plan();
        if let Runtime::Vllm(s) = &mut p.settings {
            s.env = BTreeMap::from([(reserved.to_owned(), "1".to_owned())]);
        }
        let err = sup
            .prepare(&p, &vllm_secrets(), "img", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&format!("'{reserved}' is reserved")), "{err}");
    }

    // A runtime key in the env block would reject every request.
    let mut p = vllm_plan();
    if let Runtime::Vllm(s) = &mut p.settings {
        s.env = BTreeMap::from([("LLAMA_API_KEY".to_owned(), "x".to_owned())]);
    }
    let err = sup
        .prepare(&p, &vllm_secrets(), "img", None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("strips the Authorization"), "{err}");

    // A local model path, reported with the RunPod name.
    let mut p = vllm_plan();
    if let RenderedLaunch::Command { argv, .. } = &mut p.rendered {
        argv[2] = "/models/llama".into();
    }
    let err = sup
        .prepare(&p, &vllm_secrets(), "img", None)
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
        .prepare(&vllm_plan(), &vllm_secrets(), "img", None)
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

// --- create / ambiguity / ownership ------------------------------------------

fn ambiguous_then(after: &'static str) -> Mock {
    Mock::default()
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, NO_SECRETS)
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
async fn an_ambiguous_create_is_resolved_by_name_and_never_resent() {
    let mock = ambiguous_then(named(ONE_ENDPOINT))
        .ok(HttpMethod::Get, "/serverless/ep1", 200, owned_endpoint())
        .ok(HttpMethod::Get, HEALTH, 200, "{}");
    let (sup, mock) = supervisor(mock);
    let handle = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
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
    assert!(sup.created_endpoint().load(Ordering::SeqCst));
}

#[tokio::test]
async fn an_ambiguous_create_finding_an_unmarked_endpoint_refuses_it() {
    let mock = ambiguous_then(named(ONE_ENDPOINT)).ok(
        HttpMethod::Get,
        "/serverless/ep1",
        200,
        named(ENDPOINT),
    );
    let (sup, _) = supervisor(mock);
    let err = sup
        .start(&vllm_plan(), &vllm_secrets())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("refusing to adopt"), "{err}");
    assert!(err.contains("ep1"), "the id is reported: {err}");
    assert!(sup.deployed_endpoint().lock().unwrap().is_none());
}

#[tokio::test]
async fn an_ambiguous_create_with_nothing_found_fails_without_retrying() {
    let (sup, mock) = supervisor(ambiguous_then(NO_ENDPOINTS));
    let err = sup
        .start(&vllm_plan(), &vllm_secrets())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("outcome unknown"), "{err}");
    assert!(err.contains("not retried"), "{err}");
    assert_eq!(mock.count(HttpMethod::Post, "/serverless"), 1);
    assert!(sup.deployed_endpoint().lock().unwrap().is_none());
    // The secret is kept for the caller: the endpoint may still appear.
    assert_eq!(*sup.created_secrets().lock().unwrap(), vec![hf_name()]);
    assert!(!err.contains(KEY));
}

#[tokio::test]
async fn an_ambiguous_create_that_finds_duplicates_refuses_to_pick() {
    let (sup, mock) = supervisor(ambiguous_then(named(TWO_ENDPOINTS)));
    let err = sup
        .start(&vllm_plan(), &vllm_secrets())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("ep1, ep2"), "{err}");
    assert_eq!(mock.count(HttpMethod::Post, "/serverless"), 1);
}

fn update_mock(endpoint: &'static str) -> Mock {
    Mock::default()
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, NO_SECRETS)
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .ok(HttpMethod::Get, "/serverless", 200, named(ONE_ENDPOINT))
        .ok(HttpMethod::Get, "/serverless/ep1", 200, endpoint)
        .ok(HttpMethod::Patch, "/serverless/ep1", 200, ENDPOINT)
        .ok(HttpMethod::Get, HEALTH, 200, "{}")
}

fn patch_body(mock: &Mock) -> Value {
    mock.requests()
        .into_iter()
        .find(|s| s.method == HttpMethod::Patch)
        .unwrap()
        .body
        .unwrap()
}

#[tokio::test]
async fn an_owned_endpoint_found_by_name_is_updated_in_place() {
    let (sup, mock) = supervisor(update_mock(owned_endpoint()));
    let handle = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(handle.id, "ep1");
    assert_eq!(mock.count(HttpMethod::Post, "/serverless"), 0);
    let patch = patch_body(&mock);
    assert_eq!(patch["entrypoint"], json!(["vllm"]));
    assert_eq!(patch["env"][OWNER_ENV_VAR], json!(owner()));
    assert!(patch.get("name").is_none());
    assert!(!patch.to_string().contains(KEY));
}

#[tokio::test]
async fn an_update_sends_the_scaling_block_as_workers() {
    let (sup, mock) = supervisor(update_mock(owned_endpoint()));
    let sup = sup.with_scaling(Some(&ScalingSettings {
        min_replicas: Some(1),
        max_replicas: Some(4),
        target_concurrency: None,
        scaledown_window_secs: Some(600),
    }));
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    let patch = patch_body(&mock);
    assert_eq!(
        patch["workers"],
        json!({"min": 1, "max": 4, "idleTimeout": 600})
    );
    assert_eq!(
        patch["scaling"]["requestCount"],
        json!(DEFAULT_REQUEST_COUNT)
    );
}

#[test]
fn defaults_to_one_worker_and_a_lone_min_raises_the_ceiling() {
    let (sup, _) = supervisor(Mock::default());
    let prepared = sup
        .prepare(&vllm_plan(), &vllm_secrets(), "img", None)
        .unwrap();
    assert_eq!(DEFAULT_MAX_WORKERS, 1);
    assert_eq!(prepared.endpoint.workers.min, 0);
    assert_eq!(prepared.endpoint.workers.max, 1);

    let sup = sup.with_scaling(Some(&ScalingSettings {
        min_replicas: Some(2),
        ..ScalingSettings::default()
    }));
    let prepared = sup
        .prepare(&vllm_plan(), &vllm_secrets(), "img", None)
        .unwrap();
    assert_eq!(prepared.endpoint.workers.min, 2);
    assert_eq!(prepared.endpoint.workers.max, 2);
}

#[tokio::test]
async fn an_endpoint_found_by_name_without_our_marker_is_refused() {
    let foreign: &'static str = Box::leak(
        format!(
            r#"{{"id":"ep1","name":"x","env":{{"{OWNER_ENV_VAR}":"{}"}}}}"#,
            owner_marker(Some("other-ws"), PROJECT, DEP)
        )
        .into_boxed_str(),
    );
    for (endpoint, why) in [
        (named(ENDPOINT), "carries no AMICU_OWNER marker"),
        (foreign, "belongs to another amicu deployment"),
    ] {
        let (sup, mock) = supervisor(update_mock(endpoint));
        let err = sup
            .start(&vllm_plan(), &vllm_secrets())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to adopt"), "{err}");
        assert!(err.contains(why), "{err}");
        assert_eq!(mock.count(HttpMethod::Patch, "/serverless/ep1"), 0);
        assert_eq!(mock.count(HttpMethod::Post, "/serverless"), 0);
        assert!(sup.deployed_endpoint().lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn a_recorded_endpoint_is_updated_without_a_name_lookup() {
    // Recorded in state: trusted even without a marker (an older endpoint).
    let (sup, mock) = supervisor(update_mock(named(ENDPOINT)));
    let sup = sup.with_recorded_endpoint("ep1");
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(mock.count(HttpMethod::Get, "/serverless"), 0);
    assert_eq!(mock.count(HttpMethod::Patch, "/serverless/ep1"), 1);
    assert!(!sup.created_endpoint().load(Ordering::SeqCst));

    // Gone from RunPod: fall back to the name lookup and create.
    let mock = happy_mock().ok(HttpMethod::Get, "/serverless/gone", 404, "{}");
    let (sup, mock) = supervisor(mock);
    let sup = sup.with_recorded_endpoint("gone");
    let handle = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(handle.id, "ep1");
    assert_eq!(mock.count(HttpMethod::Post, "/serverless"), 1);
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
        .start(&vllm_plan(), &vllm_secrets())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not list"), "{err}");
    assert!(
        mock.requests().iter().all(|s| s.method == HttpMethod::Get),
        "nothing created"
    );

    let mock = happy_mock().ok(
        HttpMethod::Get,
        "/catalog/gpus",
        200,
        r#"{"gpus":[{"id":"NVIDIA H100 80GB HBM3","name":"x","pool":"ADA_80_PRO","memory":94}]}"#,
    );
    let (sup, mock) = supervisor(mock);
    let err = sup
        .start(&vllm_plan(), &vllm_secrets())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("94 GB"), "{err}");
    assert_eq!(mock.count(HttpMethod::Post, "/account/secrets"), 0);

    // An unreadable catalog also stops the deploy (never guess the exclusions).
    let (sup, mock) = supervisor(happy_mock().ok(HttpMethod::Get, "/catalog/gpus", 500, "{}"));
    assert!(sup.start(&vllm_plan(), &vllm_secrets()).await.is_err());
    assert_eq!(mock.count(HttpMethod::Post, "/account/secrets"), 0);
}

#[tokio::test]
async fn a_refused_create_removes_the_secrets_it_wrote() {
    let mock = Mock::default()
        .on(
            HttpMethod::Get,
            "/account/secrets?name=",
            &[
                Canned::Status(200, NO_SECRETS),
                Canned::Status(200, FOUND_SECRET),
            ],
        )
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .ok(HttpMethod::Get, "/serverless", 200, NO_ENDPOINTS)
        .ok(
            HttpMethod::Post,
            "/serverless",
            400,
            r#"{"detail":"bad gpu"}"#,
        )
        .ok(HttpMethod::Delete, "/account/secrets/s1", 204, "");
    let (sup, mock) = supervisor(mock);
    let err = sup
        .start(&vllm_plan(), &vllm_secrets())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP 400"), "{err}");
    assert!(!err.contains(KEY));
    assert_eq!(mock.count(HttpMethod::Delete, "/account/secrets/s1"), 1);
    assert!(sup.created_secrets().lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_existing_secret_name_already_holds_the_value_and_is_left_alone() {
    let mock = Mock::default()
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, FOUND_SECRET)
        .ok(HttpMethod::Get, "/serverless", 200, NO_ENDPOINTS)
        .ok(HttpMethod::Post, "/serverless", 201, ENDPOINT)
        .ok(HttpMethod::Get, HEALTH, 200, "{}");
    let (sup, mock) = supervisor(mock);
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(mock.count(HttpMethod::Post, "/account/secrets"), 0);
    assert_eq!(mock.count(HttpMethod::Delete, "/account/secrets/s1"), 0);
    assert_eq!(*sup.created_secrets().lock().unwrap(), vec![hf_name()]);
}

#[tokio::test]
async fn a_redeploy_retires_the_previous_secrets_only_after_the_update() {
    let mock = update_mock(owned_endpoint())
        .first(
            HttpMethod::Get,
            "/account/secrets?name=",
            &[
                Canned::Status(200, NO_SECRETS),
                Canned::Status(200, FOUND_SECRET),
            ],
        )
        .ok(HttpMethod::Delete, "/account/secrets/s1", 204, "");
    let (sup, mock) = supervisor(mock);
    let sup = sup.with_recorded_secrets(vec!["old-gen".into()]);
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert!(
        position(&mock, HttpMethod::Patch, "/serverless/ep1")
            < position(&mock, HttpMethod::Delete, "/account/secrets/s1"),
        "old secrets go only once the endpoint no longer references them"
    );
    assert!(
        mock.requests()
            .iter()
            .any(|s| s.url.ends_with("?name=old-gen"))
    );
    assert_eq!(mock.count(HttpMethod::Delete, "/account/secrets/s1"), 1);
    assert_eq!(*sup.created_secrets().lock().unwrap(), vec![hf_name()]);
}

#[tokio::test]
async fn a_refused_update_keeps_the_live_secrets_and_drops_the_new_ones() {
    let mock = Mock::default()
        .on(
            HttpMethod::Get,
            "/account/secrets?name=",
            &[
                Canned::Status(200, NO_SECRETS),
                Canned::Status(200, FOUND_SECRET),
            ],
        )
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .ok(HttpMethod::Delete, "/account/secrets/s1", 204, "")
        .ok(HttpMethod::Get, "/serverless/ep1", 200, owned_endpoint())
        .ok(
            HttpMethod::Patch,
            "/serverless/ep1",
            400,
            r#"{"detail":"bad"}"#,
        );
    let (sup, mock) = supervisor(mock);
    let sup = sup
        .with_recorded_endpoint("ep1")
        .with_recorded_secrets(vec!["old-gen".into()]);
    assert!(sup.start(&vllm_plan(), &vllm_secrets()).await.is_err());
    let lookups: Vec<String> = mock
        .requests()
        .into_iter()
        .filter(|s| s.url.contains("?name="))
        .map(|s| s.url)
        .collect();
    assert!(
        lookups.iter().all(|u| !u.ends_with("=old-gen")),
        "the live endpoint's secret is untouched: {lookups:?}"
    );
    assert_eq!(mock.count(HttpMethod::Delete, "/account/secrets/s1"), 1);
    assert_eq!(
        *sup.created_secrets().lock().unwrap(),
        vec!["old-gen".to_owned()]
    );
}

#[tokio::test]
async fn a_secret_create_lost_in_transport_is_checked_by_name() {
    // Found afterwards: it was written.
    let mock = happy_mock()
        .first(
            HttpMethod::Get,
            "/account/secrets?name=",
            &[
                Canned::Status(200, NO_SECRETS),
                Canned::Status(200, FOUND_SECRET),
            ],
        )
        .first(HttpMethod::Post, "/account/secrets", &[Canned::Timeout]);
    let (sup, mock) = supervisor(mock);
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(mock.count(HttpMethod::Post, "/account/secrets"), 1);

    // Not found: fail before the endpoint, nothing to record.
    let mock = happy_mock().first(HttpMethod::Post, "/account/secrets", &[Canned::Timeout]);
    let (sup, mock) = supervisor(mock);
    let err = sup
        .start(&vllm_plan(), &vllm_secrets())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not exist"), "{err}");
    assert!(!err.contains(HF));
    assert_eq!(mock.count(HttpMethod::Post, "/serverless"), 0);
    assert!(sup.created_secrets().lock().unwrap().is_empty());
}

// --- readiness -------------------------------------------------------------------

fn ready_mock(health: &[Canned]) -> Mock {
    Mock::default()
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, NO_SECRETS)
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
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(mock.count(HttpMethod::Get, HEALTH), 4);
}

#[tokio::test]
async fn readiness_times_out_naming_the_last_answer_and_keeps_the_id() {
    let (sup, _) = supervisor(ready_mock(&[Canned::Status(204, "")]).ok(
        HttpMethod::Get,
        "/serverless/ep1/workers",
        200,
        r#"{"workers":[],"summary":{"running":1,"throttled":3,"total":4}}"#,
    ));
    let err = sup
        .start(&vllm_plan(), &vllm_secrets())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("did not become ready"), "{err}");
    assert!(err.contains("initializing (HTTP 204)"), "{err}");
    assert!(
        err.contains("running=1 idle=0 initializing=0 throttled=3 unhealthy=0"),
        "{err}"
    );
    assert!(err.contains("`amicu runpod logs chat`"), "{err}");
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
        .start(&vllm_plan(), &vllm_secrets())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("gateway rejected the deployment's api_key"),
        "{err}"
    );
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
    // A cold start or a capacity wait is not a failure.
    for summary in [
        r#"{"workers":[],"summary":{"initializing":1}}"#,
        r#"{"workers":[],"summary":{"throttled":1}}"#,
        r#"{"workers":[],"summary":{"initializing":1,"unhealthy":1}}"#,
    ] {
        let starting = status_with(summary).await;
        assert!(
            matches!(&starting, RuntimeStatus::Running { detail } if detail.contains("starting")),
            "{summary}: {starting:?}"
        );
    }
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
async fn metrics_scrape_through_the_gateway_with_the_deployment_key() {
    let (sup, mock) = supervisor(
        Mock::default()
            .ok(
                HttpMethod::Get,
                "https://ep1.api.runpod.ai/metrics",
                200,
                "llamacpp:predicted_tokens_seconds 12.5\n",
            )
            .ok(
                HttpMethod::Get,
                "/serverless/ep1/workers",
                200,
                r#"{"workers":[],"summary":{"idle":1}}"#,
            ),
    );
    let err = sup.metrics(&handle()).await.unwrap_err().to_string();
    assert!(err.contains("api_key"), "{err}");
    assert!(!mock.requests().iter().any(|s| s.url.ends_with("/metrics")));

    let sup = sup.with_gateway_key(SecretValue::new(KEY.to_owned()));
    let m = sup.metrics(&handle()).await.unwrap();
    assert!((m.tokens_per_second - 12.5).abs() < f64::EPSILON);
    let scrapes: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|s| s.url.ends_with("/metrics"))
        .collect();
    assert_eq!(scrapes.len(), 1);
    assert_eq!(scrapes[0].bearer.as_deref(), Some(KEY));
}

#[tokio::test]
async fn metrics_never_wake_a_scaled_to_zero_endpoint_unless_asked() {
    let (sup, mock) = supervisor(
        Mock::default()
            .ok(
                HttpMethod::Get,
                "https://ep1.api.runpod.ai/metrics",
                200,
                "vllm:num_requests_running 0\n",
            )
            .ok(
                HttpMethod::Get,
                "/serverless/ep1/workers",
                200,
                r#"{"workers":[],"summary":{"initializing":1}}"#,
            ),
    );
    let sup = sup.with_gateway_key(SecretValue::new(KEY.to_owned()));
    let err = sup
        .scrape_metrics(&handle(), false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("starting") && err.contains("retry shortly"),
        "{err}"
    );
    assert!(sup.metrics(&handle()).await.is_err());
    assert!(!mock.requests().iter().any(|s| s.url.ends_with("/metrics")));

    sup.scrape_metrics(&handle(), true).await.unwrap();
    assert_eq!(mock.count(HttpMethod::Get, "/metrics"), 1);
}

#[tokio::test]
async fn logs_merge_the_workers_and_say_when_none_is_running() {
    let none = Mock::default().ok(
        HttpMethod::Get,
        "/serverless/ep1/workers",
        200,
        r#"{"workers":[],"summary":{}}"#,
    );
    let (sup, _) = supervisor(none);
    let lines = sup.logs(&handle(), LogOptions::default()).await.unwrap();
    assert_eq!(lines.lines(), [crate::logs::NO_WORKERS.to_owned()]);

    let one = Mock::default()
        .ok(
            HttpMethod::Get,
            "/serverless/ep1/workers",
            200,
            r#"{"workers":[{"id":"w1","status":"RUNNING"}],"summary":{"running":1}}"#,
        )
        .ok(
            HttpMethod::Get,
            "/workers/w1/logs?tail=7",
            200,
            "data: {\"ts\":\"t\",\"line\":\"ready\"}\n\n",
        );
    let (sup, _) = supervisor(one);
    let opts = LogOptions {
        follow: false,
        tail: Some(7),
    };
    let lines = sup.logs(&handle(), opts).await.unwrap();
    assert_eq!(lines.lines(), ["[w1] ready".to_owned()]);

    let follow = LogOptions {
        follow: true,
        tail: None,
    };
    let err = sup.logs(&handle(), follow).await.unwrap_err().to_string();
    assert!(err.contains("not supported"), "{err}");
}

// --- stop --------------------------------------------------------------------------

const FAST_STOP: StopRetry = StopRetry {
    attempts: 3,
    delay: Duration::from_millis(1),
};

fn stop_mock(endpoint_status: u16) -> Mock {
    Mock::default()
        .ok(HttpMethod::Patch, "/serverless/ep1", 200, ENDPOINT)
        .ok(HttpMethod::Delete, "/serverless/ep1", endpoint_status, "")
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, FOUND_SECRET)
        .ok(HttpMethod::Delete, "/account/secrets/s1", 204, "")
}

fn stopper(mock: Mock) -> (RunPodSupervisor, Arc<Mock>) {
    let (sup, mock) = supervisor(mock);
    (sup.with_stop_retry(FAST_STOP), mock)
}

#[tokio::test]
async fn stop_drains_and_deletes_the_endpoint_then_its_secrets() {
    let (sup, mock) = stopper(stop_mock(204));
    let sup = sup.with_recorded_secrets(vec!["recorded-secret".into()]);
    sup.secrets.note("made-here");
    assert_eq!(
        sup.stop_endpoint(&handle()).await.unwrap(),
        StopOutcome::Deleted
    );
    let seen = mock.requests();
    assert_eq!(seen[0].method, HttpMethod::Patch, "drain first");
    assert_eq!(
        seen[0].body.as_ref().unwrap(),
        &json!({"workers": {"min": 0, "max": 0}})
    );
    assert_eq!(seen[1].method, HttpMethod::Delete);
    assert!(seen[1].url.ends_with("/serverless/ep1"), "endpoint next");
    for name in ["recorded-secret", "made-here"] {
        assert!(
            seen.iter()
                .any(|s| s.url.ends_with(&format!("?name={name}"))),
            "{name}"
        );
    }
    assert_eq!(mock.count(HttpMethod::Delete, "/account/secrets/s1"), 2);
    assert!(sup.created_secrets().lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_404_deletes_secrets_only_for_a_confirmed_account() {
    let (sup, mock) = stopper(stop_mock(404));
    let sup = sup.with_recorded_secrets(vec!["recorded-secret".into()]);
    assert_eq!(
        sup.stop_endpoint(&handle()).await.unwrap(),
        StopOutcome::AlreadyGone {
            kept_secrets: vec!["recorded-secret".into()]
        }
    );
    assert!(mock.requests().iter().all(|s| !s.url.contains("/account/")));
    assert_eq!(
        *sup.created_secrets().lock().unwrap(),
        vec!["recorded-secret".to_owned()],
        "kept for the caller to record"
    );

    let (sup, mock) = stopper(stop_mock(404));
    let sup = sup
        .with_recorded_secrets(vec!["recorded-secret".into()])
        .with_confirmed_account(true);
    assert_eq!(
        sup.stop_endpoint(&handle()).await.unwrap(),
        StopOutcome::AlreadyGone {
            kept_secrets: Vec::new()
        }
    );
    assert_eq!(mock.count(HttpMethod::Delete, "/account/secrets/s1"), 1);
}

#[tokio::test]
async fn a_delete_refused_for_live_workers_is_retried_a_bounded_number_of_times() {
    let busy = Canned::Status(400, r#"{"detail":"endpoint still has active workers"}"#);
    let mock = stop_mock(204).first(
        HttpMethod::Delete,
        "/serverless/ep1",
        &[busy.clone(), Canned::Status(204, "")],
    );
    let (sup, mock) = stopper(mock);
    sup.stop(&handle()).await.unwrap();
    assert_eq!(mock.count(HttpMethod::Delete, "/serverless/ep1"), 2);

    let (sup, mock) = stopper(Mock::default().on(HttpMethod::Delete, "/serverless/ep1", &[busy]));
    let err = sup.stop(&handle()).await.unwrap_err().to_string();
    assert!(err.contains("active workers"), "{err}");
    assert_eq!(
        mock.count(HttpMethod::Delete, "/serverless/ep1"),
        FAST_STOP.attempts as usize
    );
}

#[tokio::test]
async fn stop_errors_when_an_unconfirmed_404_keeps_secrets() {
    let (sup, _) = stopper(stop_mock(404));
    let sup = sup.with_recorded_secrets(vec!["recorded-secret".into()]);
    let err = sup.stop(&handle()).await.unwrap_err().to_string();
    assert!(err.contains("`recorded-secret`"), "{err}");
    assert!(err.contains("could not be confirmed"), "{err}");
    assert!(err.contains("kept"), "{err}");

    let (sup, _) = stopper(stop_mock(404));
    sup.stop(&handle()).await.unwrap();
}

#[tokio::test]
async fn a_delete_refused_with_409_or_a_detail_less_400_is_retried() {
    for refusal in [Canned::Status(409, ""), Canned::Status(400, "")] {
        let mock = stop_mock(204).first(
            HttpMethod::Delete,
            "/serverless/ep1",
            &[refusal, Canned::Status(204, "")],
        );
        let (sup, mock) = stopper(mock);
        sup.stop(&handle()).await.unwrap();
        assert_eq!(mock.count(HttpMethod::Delete, "/serverless/ep1"), 2);
    }
}

#[tokio::test]
async fn a_failed_delete_after_the_drain_says_it_is_scaled_to_zero_but_tracked() {
    let (sup, _) = stopper(stop_mock(500));
    let err = sup.stop(&handle()).await.unwrap_err().to_string();
    assert!(err.contains("scaled to zero"), "{err}");
    assert!(err.contains("still recorded"), "{err}");
}

#[tokio::test]
async fn secrets_stay_when_the_endpoint_delete_is_not_confirmed() {
    let (sup, mock) = stopper(stop_mock(500));
    let err = sup.stop(&handle()).await.unwrap_err().to_string();
    assert!(err.contains("500"), "{err}");
    assert!(
        mock.requests().iter().all(|s| !s.url.contains("/account/")),
        "no secret call after a failed endpoint delete"
    );
    assert_eq!(mock.count(HttpMethod::Delete, "/serverless/ep1"), 1);
}

#[tokio::test]
async fn a_secret_that_cannot_be_deleted_is_reported_and_kept() {
    let mock = Mock::default()
        .ok(HttpMethod::Delete, "/serverless/ep1", 204, "")
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, NO_SECRETS);
    let (sup, _) = stopper(mock);
    sup.stop(&handle()).await.unwrap();

    let mock = Mock::default()
        .ok(HttpMethod::Delete, "/serverless/ep1", 204, "")
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, FOUND_SECRET)
        .ok(HttpMethod::Delete, "/account/secrets/s1", 500, "");
    let (sup, _) = stopper(mock);
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
