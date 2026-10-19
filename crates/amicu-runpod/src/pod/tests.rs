use std::collections::BTreeMap;

use amicu_core::ByteSize;
use amicu_core::resource::{AcceleratorKind, Residency, Runtime, VllmSettings};
use serde_json::{Value, json};

use super::*;
use crate::client::ApiKey;
use crate::supervisor::secret_name;
use crate::supervisor::tests::{
    BASE, Canned, DEP, FAST, FOUND_SECRET, HF, KEY, MGMT, Mock, NO_SECRETS, PROJECT, SECRET,
    llama_plan, plan, position, posted, secrets_for, vllm_plan, vllm_secrets,
};

const TYPE: &str = "NVIDIA H100 80GB HBM3";
const CATALOG: &str = r#"{"gpus":[
    {"id":"NVIDIA H100 80GB HBM3","name":"H100 SXM","pool":"ADA_80_PRO","memory":80},
    {"id":"NVIDIA H200 NVL","name":"H200 NVL","memory":143}]}"#;
const NO_PODS: &str = r#"{"pods":[],"pagination":{"hasNextPage":false}}"#;
const PODS: &str = "/pods";
const POD: &str = "/pods/pod1";
const ACTION: &str = "/pods/pod1/action";
const HEALTH: &str = "https://pod1-8000.proxy.runpod.net/health";
const MODELS: &str = "https://pod1-8000.proxy.runpod.net/v1/models";
const CHAT: &str = "https://pod1-8000.proxy.runpod.net/v1/chat/completions";
const INVOCATIONS: &str = "https://pod1-8000.proxy.runpod.net/invocations";
const DOCS: &str = "https://pod1-8000.proxy.runpod.net/docs";

fn owner() -> String {
    owner_marker(None, PROJECT, DEP)
}

fn name() -> String {
    project_endpoint_name(PROJECT, DEP)
}

fn sname(var: &str, value: &str) -> String {
    secret_name(PROJECT, DEP, &owner(), var, value)
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// `GET /pods/pod1` in `status`, owned by this deployment, started at `t0`.
fn pod(status: &str) -> &'static str {
    pod_at(status, "t0")
}

fn pod_at(status: &str, started: &str) -> &'static str {
    pod_full(status, TYPE, "EU-RO-1", &owner(), started)
}

fn pod_with(status: &str, gpu: &str, dc: &str, owner: &str) -> &'static str {
    pod_full(status, gpu, dc, owner, "t0")
}

fn pod_full(status: &str, gpu: &str, dc: &str, owner: &str, started: &str) -> &'static str {
    leak(format!(
        r#"{{"id":"pod1","name":"{}","status":"{status}","gpu":{{"id":"{gpu}","count":2}},
        "cloud":"SECURE","dataCenterId":"{dc}","cost":5.38,"startedAt":"{started}",
        "env":{{"{OWNER_ENV_VAR}":"{owner}","HF_TOKEN":"x"}}}}"#,
        name()
    ))
}

/// The pod exactly as `start` would configure it for the vLLM plan.
fn matching_pod(sup: &PodSupervisor, status: &str, started: &str) -> &'static str {
    let image = runtime_image(RuntimeKind::Vllm);
    let prepared = sup
        .prepare(&vllm_plan(), &vllm_secrets(), &image, None)
        .unwrap();
    let mut body = prepared.pod.to_body();
    let m = body.as_object_mut().unwrap();
    m.insert("id".into(), json!("pod1"));
    m.insert("status".into(), json!(status));
    m.insert("startedAt".into(), json!(started));
    m.insert("dataCenterId".into(), json!("EU-RO-1"));
    leak(body.to_string())
}

fn one_pod(status: &str) -> &'static str {
    leak(format!(
        r#"{{"pods":[{{"id":"pod1","name":"{}","status":"{status}"}}],
        "pagination":{{"hasNextPage":false}}}}"#,
        name()
    ))
}

fn h100() -> Accelerator {
    Accelerator {
        kind: AcceleratorKind::Gpu,
        model: "h100-sxm-80gb".to_owned(),
        count: 2,
        memory: ByteSize::from_bytes(80 * (1 << 30)),
        memory_bandwidth_gbps: None,
    }
}

fn supervisor(mock: Mock) -> (PodSupervisor, Arc<Mock>) {
    let mock = Arc::new(mock.ok(HttpMethod::Get, "/catalog/gpus", 200, CATALOG));
    let client = RunPodClient::with_transport(ApiKey::new(MGMT), mock.clone(), BASE);
    let sup = PodSupervisor::new(client, PROJECT, DEP)
        .with_probe_transport(mock.clone())
        .with_accelerator(Some(&h100()))
        .unwrap()
        .with_readiness_timing(FAST);
    (sup, mock)
}

/// Secrets, an empty pod list, a create, then RUNNING / health / anonymous
/// and keyed exposure answers as given.
fn create_mock(status: &[Canned], health: &[Canned], anon: &[Canned], keyed: &[Canned]) -> Mock {
    // Two "does it exist?" lookups, then every lookup finds it.
    Mock::default()
        .on(
            HttpMethod::Get,
            "/account/secrets?name=",
            &[
                Canned::Status(200, NO_SECRETS),
                Canned::Status(200, NO_SECRETS),
                Canned::Status(200, FOUND_SECRET),
            ],
        )
        .ok(HttpMethod::Post, "/account/secrets", 201, SECRET)
        .ok(HttpMethod::Delete, "/account/secrets/s1", 204, "")
        .ok(HttpMethod::Get, PODS, 200, NO_PODS)
        .ok(HttpMethod::Post, PODS, 201, pod("PROVISIONING"))
        .on(HttpMethod::Get, POD, status)
        .on(HttpMethod::Get, HEALTH, health)
        .on(HttpMethod::Post, CHAT, anon)
        .ok(HttpMethod::Post, INVOCATIONS, 401, "")
        .ok(HttpMethod::Get, DOCS, 401, "")
        .on(HttpMethod::Get, MODELS, keyed)
        .ok(HttpMethod::Post, ACTION, 204, "")
}

fn happy_mock() -> Mock {
    create_mock(
        &[Canned::Status(200, pod("RUNNING"))],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(401, "")],
        &[Canned::Status(200, "{}")],
    )
}

fn body_of(mock: &Mock, method: HttpMethod, suffix: &str) -> Value {
    mock.requests()
        .into_iter()
        .find(|s| s.method == method && s.url.ends_with(suffix))
        .and_then(|s| s.body)
        .unwrap_or_else(|| panic!("no {method:?} {suffix}"))
}

fn count(mock: &Mock, method: HttpMethod, suffix: &str) -> usize {
    mock.requests()
        .iter()
        .filter(|s| s.method == method && s.url.ends_with(suffix))
        .count()
}

fn err_text(e: &LlamaCppError) -> String {
    format!("{e} {e:?}")
}

// --- create ------------------------------------------------------------------

#[tokio::test]
async fn vllm_start_creates_secrets_then_the_pod_and_checks_exposure() {
    let (sup, mock) = supervisor(happy_mock());
    let handle = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(handle.id, "pod1");
    assert_eq!(handle.endpoint, "https://pod1-8000.proxy.runpod.net");
    assert_eq!(
        handle.exposure,
        EndpointExposure::Public {
            gateway: GatewayAuth::None
        }
    );

    let key_secret = sname("VLLM_API_KEY", KEY);
    let hf_secret = sname("HF_TOKEN", HF);
    assert_eq!(
        body_of(&mock, HttpMethod::Post, PODS),
        json!({
            "name": name(),
            "image": amicu_llamacpp::remote::DEFAULT_VLLM_IMAGE,
            "entrypoint": ["/bin/sh", "-c"],
            "cmd": [vllm_pod_command(&[
                "vllm".into(), "serve".into(), "meta-llama/Llama-3.1-8B-Instruct".into(),
                "--port".into(), "8000".into(), "--host".into(), "0.0.0.0".into(),
            ]).1[0].clone()],
            "env": {
                "AMICU_OWNER": owner(),
                "HF_TOKEN": format!("{{{{ RUNPOD_SECRET_{hf_secret} }}}}"),
                "VLLM_API_KEY": format!("{{{{ RUNPOD_SECRET_{key_secret} }}}}"),
            },
            "ports": ["8000/http"],
            "disk": 100,
            "gpu": {"id": TYPE, "count": 2,
                    "minCudaVersion": amicu_llamacpp::remote::DEFAULT_VLLM_IMAGE_MIN_CUDA},
            "cloud": "SECURE",
            "startSsh": false,
            "startJupyter": false
        })
    );

    // The runtime key is a RunPod secret here (the proxy adds no auth).
    let secret_bodies = posted(&mock, "/account/secrets");
    let values: Vec<&str> = secret_bodies
        .iter()
        .map(|b| b["value"].as_str().unwrap())
        .collect();
    assert!(values.contains(&KEY) && values.contains(&HF));
    assert!(
        position(&mock, HttpMethod::Post, "/account/secrets")
            < position(&mock, HttpMethod::Post, PODS)
    );
    assert!(
        position(&mock, HttpMethod::Get, "/catalog/gpus")
            < position(&mock, HttpMethod::Post, "/account/secrets")
    );

    // Anonymous first, then with the key; neither probe carries the
    // management key, and nothing but the key ever does.
    let probes: Vec<(HttpMethod, String, Option<String>)> = mock
        .requests()
        .into_iter()
        .filter(|s| [MODELS, CHAT, INVOCATIONS, DOCS].contains(&s.url.as_str()))
        .map(|s| (s.method, s.url, s.bearer))
        .collect();
    assert_eq!(
        probes,
        [
            (HttpMethod::Post, CHAT.to_owned(), None),
            (HttpMethod::Post, INVOCATIONS.to_owned(), None),
            (HttpMethod::Get, DOCS.to_owned(), None),
            (HttpMethod::Get, MODELS.to_owned(), Some(KEY.to_owned()))
        ]
    );
    assert!(
        mock.requests()
            .iter()
            .filter(|s| s.url == HEALTH)
            .all(|s| s.bearer.is_none())
    );
    assert_eq!(sup.deployed_pod().lock().unwrap().as_deref(), Some("pod1"));
    let mut created = sup.created_secrets().lock().unwrap().clone();
    created.sort();
    let mut want = vec![key_secret, hf_secret];
    want.sort();
    assert_eq!(created, want);
}

#[tokio::test]
async fn planned_secret_names_match_what_start_writes() {
    let (sup, mock) = supervisor(happy_mock());
    let planned = sup
        .planned_secret_names(&vllm_plan(), &vllm_secrets())
        .unwrap();
    assert!(mock.requests().is_empty(), "planning makes no calls");
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    let written: Vec<String> = posted(&mock, "/account/secrets")
        .iter()
        .map(|b| b["name"].as_str().unwrap().to_owned())
        .collect();
    let mut written = written;
    written.sort();
    assert_eq!(planned, written);
}

#[tokio::test]
async fn llama_start_uses_the_image_server_and_its_key_var() {
    // llama.cpp serves on the image's port 8080 even when the deployment says 8000.
    let (sup, mock) = supervisor(
        create_mock(&[Canned::Status(200, pod("RUNNING"))], &[], &[], &[])
            .first(
                HttpMethod::Get,
                "https://pod1-8080.proxy.runpod.net/health",
                &[Canned::Status(200, "{}")],
            )
            .first(
                HttpMethod::Post,
                "https://pod1-8080.proxy.runpod.net/v1/chat/completions",
                &[Canned::Status(401, "")],
            )
            .first(
                HttpMethod::Get,
                "https://pod1-8080.proxy.runpod.net/v1/models",
                &[Canned::Status(200, "{}")],
            ),
    );
    let mut p = llama_plan();
    if let Runtime::LlamaCpp(s) = &mut p.settings {
        s.port = 8000;
    }
    let mut sup = sup;
    sup.gguf = Some(GgufSource {
        repo: "org/g-GGUF".into(),
        file: "g.gguf".into(),
    });
    let handle = sup.start(&p, &secrets_for("LLAMA_API_KEY")).await.unwrap();
    assert_eq!(handle.endpoint, "https://pod1-8080.proxy.runpod.net");
    let body = body_of(&mock, HttpMethod::Post, PODS);
    assert_eq!(body["entrypoint"], json!(["/app/llama-server"]));
    assert_eq!(count(&mock, HttpMethod::Post, "/invocations"), 0);
    assert_eq!(count(&mock, HttpMethod::Get, "/docs"), 0);
    assert_eq!(body["ports"], json!(["8080/http"]));
    assert_eq!(
        body["gpu"]["minCudaVersion"],
        json!(amicu_llamacpp::remote::DEFAULT_LLAMACPP_IMAGE_MIN_CUDA)
    );
    assert!(
        body["env"]["LLAMA_API_KEY"]
            .as_str()
            .unwrap()
            .starts_with("{{ RUNPOD_SECRET_")
    );
}

#[tokio::test]
async fn prepare_refuses_what_a_pod_cannot_run_safely() {
    let (sup, mock) = supervisor(Mock::default());
    // No api_key: the proxy would be open.
    let only_hf = vec![vllm_secrets().remove(1)];
    let e = sup.start(&vllm_plan(), &only_hf).await.unwrap_err();
    assert!(err_text(&e).contains("need an `api_key`"), "{e}");

    // The key var in the plain env block (plain text in the pod config).
    let mut p = plan(
        &["vllm", "serve", "m", "--port", "8000"],
        Runtime::Vllm(VllmSettings {
            port: 8000,
            env: BTreeMap::from([("VLLM_API_KEY".to_owned(), "plain".to_owned())]),
            ..Default::default()
        }),
    );
    let e = sup.start(&p, &[]).await.unwrap_err();
    assert!(err_text(&e).contains("plain text"), "{e}");

    // Variables that would turn on SSH / Jupyter.
    for reserved in ["PUBLIC_KEY", "JUPYTER_PASSWORD", OWNER_ENV_VAR] {
        if let Runtime::Vllm(s) = &mut p.settings {
            s.env = BTreeMap::from([(reserved.to_owned(), "x".to_owned())]);
        }
        let e = sup.start(&p, &vllm_secrets()).await.unwrap_err();
        assert!(err_text(&e).contains("reserved"), "{reserved}: {e}");
    }

    // A key the runtime would read as a list.
    let mut bad = vllm_secrets();
    bad[0].value = SecretValue::new("a,b".to_owned());
    assert!(sup.start(&vllm_plan(), &bad).await.is_err());

    assert!(mock.requests().is_empty(), "nothing is sent");
}

#[tokio::test]
async fn no_gpu_or_an_unlisted_type_stops_before_any_write() {
    let client = RunPodClient::with_transport(ApiKey::new(MGMT), Arc::new(Mock::default()), BASE);
    let sup = PodSupervisor::new(client, PROJECT, DEP);
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&e).contains("GPU accelerator"), "{e}");

    let mock = Mock::default().ok(HttpMethod::Get, "/catalog/gpus", 200, r#"{"gpus":[]}"#);
    let (sup, mock) = supervisor(mock);
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&e).contains("does not list"), "{e}");
    assert!(mock.requests().iter().all(|s| s.method == HttpMethod::Get));
}

#[test]
fn pods_only_cards_resolve_for_pods() {
    let client = RunPodClient::with_transport(ApiKey::new(MGMT), Arc::new(Mock::default()), BASE);
    let nvl = Accelerator {
        model: "h200-nvl".into(),
        count: 1,
        memory: ByteSize::from_bytes(141 * (1 << 30)),
        ..h100()
    };
    let sup = PodSupervisor::new(client, PROJECT, DEP)
        .with_accelerator(Some(&nvl))
        .unwrap();
    assert_eq!(sup.gpu_type_id.as_deref(), Some("NVIDIA H200 NVL"));
}

#[tokio::test]
async fn community_cloud_and_region_reach_the_body() {
    let (sup, mock) = supervisor(happy_mock());
    let region = Region {
        code: "EU-RO-1".into(),
        residency: Residency::Eu,
        display_name: None,
        country: None,
        default_zone: None,
    };
    let sup = sup
        .with_cloud(Cloud::Community)
        .with_region(Some(&region))
        .unwrap();
    // The pod reports SECURE, so this only checks the create body.
    let _ = sup.start(&vllm_plan(), &vllm_secrets()).await;
    let body = body_of(&mock, HttpMethod::Post, PODS);
    assert_eq!(body["cloud"], json!("COMMUNITY"));
    assert_eq!(body["dataCenterIds"], json!(["EU-RO-1"]));
}

#[test]
fn the_data_class_residency_allowlist_bounds_placement() {
    let (sup, _) = supervisor(Mock::default());
    let err = sup
        .with_region(Some(&Region::from_code("eu-ro-1")))
        .unwrap()
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

    let (sup, _) = supervisor(Mock::default());
    let sup = sup.with_allowed_residencies(&[]).unwrap();
    assert_eq!(sup.data_center_ids, Vec::<String>::new());
}

// --- ambiguity, adoption, update ---------------------------------------------

#[tokio::test]
async fn an_ambiguous_create_is_resolved_by_name_and_never_resent() {
    let mock = happy_mock()
        .first(HttpMethod::Post, PODS, &[Canned::Timeout])
        .first(
            HttpMethod::Get,
            PODS,
            &[
                Canned::Status(200, NO_PODS),
                Canned::Status(200, one_pod("STARTING")),
            ],
        );
    let (sup, mock) = supervisor(mock);
    let handle = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(handle.id, "pod1");
    assert_eq!(count(&mock, HttpMethod::Post, PODS), 1);
}

#[tokio::test]
async fn an_ambiguous_create_with_nothing_found_fails_without_retrying() {
    let mock = happy_mock().first(HttpMethod::Post, PODS, &[Canned::Timeout]);
    let (sup, mock) = supervisor(mock);
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&e).contains("not retried"), "{e}");
    assert_eq!(count(&mock, HttpMethod::Post, PODS), 1);
}

#[tokio::test]
async fn duplicate_pods_by_name_are_refused() {
    let two = leak(format!(
        r#"{{"pods":[{{"id":"a","name":"{n}","status":"RUNNING"}},
        {{"id":"b","name":"{n}","status":"EXITED"}}],"pagination":{{"hasNextPage":false}}}}"#,
        n = name()
    ));
    let (sup, mock) =
        supervisor(happy_mock().first(HttpMethod::Get, PODS, &[Canned::Status(200, two)]));
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&e).contains("several RunPod pods"), "{e}");
    assert_eq!(count(&mock, HttpMethod::Post, PODS), 0);
}

#[tokio::test]
async fn an_owned_pod_found_by_name_is_patched_and_restarted_if_stopped() {
    let mock = happy_mock()
        .first(
            HttpMethod::Get,
            PODS,
            &[Canned::Status(200, one_pod("EXITED"))],
        )
        .first(
            HttpMethod::Get,
            POD,
            &[
                Canned::Status(200, pod("EXITED")),
                // The previous container's status, right after `start`.
                Canned::Status(200, pod("EXITED")),
                Canned::Status(200, pod_at("RUNNING", "t1")),
            ],
        )
        .ok(HttpMethod::Patch, POD, 200, pod("EXITED"))
        .first(
            HttpMethod::Post,
            ACTION,
            &[Canned::Status(200, pod("STARTING"))],
        );
    let (sup, mock) = supervisor(mock);
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(count(&mock, HttpMethod::Post, PODS), 0, "no create");
    let patch = body_of(&mock, HttpMethod::Patch, POD);
    assert!(patch.get("gpu").is_none() && patch.get("cloud").is_none());
    assert_eq!(patch["ports"], json!(["8000/http"]));
    assert_eq!(patch["env"]["AMICU_OWNER"], json!(owner()));
    assert_eq!(
        body_of(&mock, HttpMethod::Post, ACTION),
        json!({"action": "start"})
    );
}

#[tokio::test]
async fn a_pod_found_by_name_without_our_marker_is_refused() {
    let foreign = pod_with("RUNNING", TYPE, "EU-RO-1", "someone-else");
    let mock = happy_mock()
        .first(
            HttpMethod::Get,
            PODS,
            &[Canned::Status(200, one_pod("RUNNING"))],
        )
        .first(HttpMethod::Get, POD, &[Canned::Status(200, foreign)]);
    let (sup, mock) = supervisor(mock);
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&e).contains("refusing to adopt"), "{e}");
    assert_eq!(count(&mock, HttpMethod::Patch, POD), 0);
}

#[tokio::test]
async fn a_recorded_pod_on_other_hardware_must_be_replaced() {
    let other = pod_with("RUNNING", "NVIDIA A40", "EU-RO-1", &owner());
    let mock = happy_mock().first(HttpMethod::Get, POD, &[Canned::Status(200, other)]);
    let (sup, mock) = supervisor(mock);
    let sup = sup.with_recorded_pod("pod1");
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&e).contains("must be replaced"), "{e}");
    assert_eq!(count(&mock, HttpMethod::Patch, POD), 0);
    assert_eq!(count(&mock, HttpMethod::Get, PODS), 0, "no name lookup");
    // The secrets written for the refused configuration are dropped again.
    assert_eq!(
        count(&mock, HttpMethod::Post, "/account/secrets"),
        count(&mock, HttpMethod::Delete, "/account/secrets/s1")
    );
    assert_eq!(sup.deployed_pod().lock().unwrap().as_deref(), Some("pod1"));
}

#[tokio::test]
async fn a_refused_create_removes_the_secrets_it_wrote() {
    let mock = happy_mock().first(
        HttpMethod::Post,
        PODS,
        &[Canned::Status(400, r#"{"detail":"no capacity"}"#)],
    );
    let (sup, mock) = supervisor(mock);
    assert!(sup.start(&vllm_plan(), &vllm_secrets()).await.is_err());
    assert_eq!(count(&mock, HttpMethod::Delete, "/account/secrets/s1"), 2);
    assert!(sup.created_secrets().lock().unwrap().is_empty());
    assert!(sup.deployed_pod().lock().unwrap().is_none());
}

#[tokio::test]
async fn a_patched_running_pod_is_restarted_and_its_old_container_not_trusted() {
    let mock = happy_mock()
        .first(
            HttpMethod::Get,
            POD,
            &[
                Canned::Status(200, pod("RUNNING")),
                // Still the old container (its /health answers 200).
                Canned::Status(200, pod("RUNNING")),
                Canned::Status(200, pod("STARTING")),
                Canned::Status(200, pod_at("RUNNING", "t1")),
            ],
        )
        // The PATCH's own reset is in progress: STARTING, same startedAt.
        .ok(HttpMethod::Patch, POD, 200, pod("STARTING"))
        .first(
            HttpMethod::Post,
            ACTION,
            &[
                Canned::Status(409, r#"{"detail":"not running"}"#),
                Canned::Status(200, pod("RUNNING")),
            ],
        );
    let (sup, mock) = supervisor(mock);
    let sup = sup.with_recorded_pod("pod1");
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(count(&mock, HttpMethod::Patch, POD), 1);
    assert_eq!(count(&mock, HttpMethod::Post, ACTION), 2, "409 retried");
    assert_eq!(
        body_of(&mock, HttpMethod::Post, ACTION),
        json!({"action": "restart"})
    );
    assert!(position(&mock, HttpMethod::Patch, POD) < position(&mock, HttpMethod::Post, ACTION));
    assert_eq!(count(&mock, HttpMethod::Get, POD), 4, "waited for t1");
    assert_eq!(count(&mock, HttpMethod::Get, HEALTH), 1, "only the new one");
}

#[tokio::test(start_paused = true)]
async fn the_restart_retries_and_the_readiness_wait_share_one_deadline() {
    let timing = ReadinessTiming {
        timeout: Duration::from_millis(300),
        ..FAST
    };
    let mut action = vec![Canned::Status(409, r#"{"detail":"not running"}"#); 50];
    action.push(Canned::Status(200, pod("STARTING")));
    let mock = happy_mock()
        .first(
            HttpMethod::Get,
            POD,
            &[
                Canned::Status(200, pod("RUNNING")),
                Canned::Status(200, pod("STARTING")),
            ],
        )
        .ok(HttpMethod::Patch, POD, 200, pod("STARTING"))
        .first(HttpMethod::Post, ACTION, &action);
    let (sup, mock) = supervisor(mock);
    let sup = sup.with_recorded_pod("pod1").with_readiness_timing(timing);
    let started = tokio::time::Instant::now();
    let err = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&err).contains("did not become ready"), "{err:?}");
    assert_eq!(count(&mock, HttpMethod::Post, ACTION), 51);
    assert!(started.elapsed() <= timing.timeout + timing.max_backoff);
}

#[tokio::test]
async fn an_unchanged_pod_is_neither_patched_nor_restarted() {
    let (probe, _) = supervisor(Mock::default());
    let same = matching_pod(&probe, "RUNNING", "t0");
    let mock = happy_mock().first(HttpMethod::Get, POD, &[Canned::Status(200, same)]);
    let (sup, mock) = supervisor(mock);
    let sup = sup.with_recorded_pod("pod1");
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(count(&mock, HttpMethod::Patch, POD), 0);
    assert_eq!(count(&mock, HttpMethod::Post, ACTION), 0);
}

#[tokio::test]
async fn a_started_pod_that_stays_exited_fails_after_the_grace_window() {
    let mock = happy_mock()
        .first(HttpMethod::Get, POD, &[Canned::Status(200, pod("EXITED"))])
        .ok(HttpMethod::Patch, POD, 200, pod("EXITED"))
        .first(
            HttpMethod::Post,
            ACTION,
            &[Canned::Status(200, pod("STARTING"))],
        );
    let (mut sup, mock) = supervisor(mock);
    sup.start_grace = Duration::from_millis(20);
    let sup = sup.with_recorded_pod("pod1");
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    let text = err_text(&e);
    assert!(
        text.contains("EXITED") && text.contains("left in place"),
        "{text}"
    );
    assert!(
        count(&mock, HttpMethod::Get, POD) > 2,
        "not the first EXITED"
    );
}

// --- readiness and exposure ----------------------------------------------------

#[tokio::test]
async fn a_terminated_pod_is_not_reported_as_left_in_place() {
    let (sup, _) = supervisor(create_mock(
        &[Canned::Status(200, pod("TERMINATED"))],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(401, "")],
        &[Canned::Status(200, "{}")],
    ));
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    let text = err_text(&e);
    assert!(
        text.contains("terminated") && !text.contains("left in place"),
        "{text}"
    );
}

#[tokio::test]
async fn a_validation_error_to_an_anonymous_request_means_open() {
    let (sup, mock) = supervisor(create_mock(
        &[Canned::Status(200, pod("RUNNING"))],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(400, "{}")],
        &[Canned::Status(200, "{}")],
    ));
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    let text = err_text(&e);
    assert!(
        text.contains("unauthenticated") && text.contains("was terminated"),
        "{text}"
    );
    assert_eq!(
        body_of(&mock, HttpMethod::Post, CHAT),
        json!({}),
        "anonymous probe is a guarded POST"
    );
}

#[tokio::test]
async fn a_403_is_not_taken_as_the_runtime_refusing() {
    let (sup, mock) = supervisor(create_mock(
        &[Canned::Status(200, pod("RUNNING"))],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(403, "")],
        &[Canned::Status(200, "{}")],
    ));
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    let text = err_text(&e);
    assert!(
        text.contains("no 401 answer") && text.contains("was terminated"),
        "{text}"
    );
    assert_eq!(count(&mock, HttpMethod::Get, MODELS), 0);
}

async fn open_route_is_terminated(method: HttpMethod, url: &'static str, path: &str) {
    let (sup, mock) = supervisor(happy_mock().first(method, url, &[Canned::Status(200, "{}")]));
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    let text = err_text(&e);
    assert!(
        text.contains(&format!("unauthenticated request to {path} (HTTP 200)"))
            && text.contains("was terminated"),
        "{text}"
    );
    assert_eq!(count(&mock, HttpMethod::Get, MODELS), 0);
}

#[tokio::test]
async fn an_open_vllm_invocations_route_tears_the_pod_down() {
    open_route_is_terminated(HttpMethod::Post, INVOCATIONS, "/invocations").await;
}

#[tokio::test]
async fn an_open_vllm_docs_route_tears_the_pod_down() {
    open_route_is_terminated(HttpMethod::Get, DOCS, "/docs").await;
}

#[test]
fn the_vllm_pod_command_loads_the_auth_middleware_without_the_key() {
    let sup = PodSupervisor::new(
        RunPodClient::with_transport(ApiKey::new(MGMT), Arc::new(Mock::default()), BASE),
        PROJECT,
        DEP,
    )
    .with_accelerator(Some(&h100()))
    .unwrap();
    let prepared = sup
        .prepare(&vllm_plan(), &vllm_secrets(), "img", None)
        .unwrap();
    assert_eq!(prepared.pod.entrypoint, ["/bin/sh", "-c"]);
    let [script] = prepared.pod.cmd.as_slice() else {
        panic!("{:?}", prepared.pod.cmd)
    };
    assert!(
        script.contains("cat > /tmp/amicu/amicu_auth.py <<'AMICU_AUTH_EOF'\n"),
        "{script}"
    );
    assert!(script.contains(VLLM_AUTH_MODULE));
    assert!(
        script.ends_with(
            "export PYTHONPATH=/tmp/amicu${PYTHONPATH:+:$PYTHONPATH}\nexec 'vllm' 'serve' \
             'meta-llama/Llama-3.1-8B-Instruct' '--port' '8000' '--host' '0.0.0.0' \
             --middleware amicu_auth.RequireKey\n"
        ),
        "{script}"
    );
    assert!(!script.contains(KEY));
    assert!(!VLLM_AUTH_MODULE.contains(HEREDOC_END));
    assert!(VLLM_AUTH_MODULE.contains(&format!(
        "os.environ[\"{}\"]",
        RuntimeKind::Vllm.api_key_env_var()
    )));
    assert_eq!(prepared.anon_probes, VLLM_ANON_PROBES);
}

#[test]
fn vllm_pod_command_quotes_every_argument() {
    let (_, cmd) = vllm_pod_command(&["vllm".into(), "it's $HOME".into()]);
    assert!(
        cmd[0].contains("exec 'vllm' 'it'\\''s $HOME' --middleware"),
        "{}",
        cmd[0]
    );
}

/// Runs the rendered script with a stand-in `vllm` that loads the middleware
/// the way vLLM does and drives it with ASGI requests (needs `python3`).
#[test]
fn the_rendered_script_guards_every_route_but_health() {
    const HARNESS: &str = r#"
import asyncio, importlib, sys
args = sys.argv[1:]
assert args[-2:] == ["--middleware", "amicu_auth.RequireKey"], args
assert args[:2] == ["serve", "it's"], args
mod, name = args[-1].rsplit(".", 1)
cls = getattr(importlib.import_module(mod), name)

async def inner(scope, receive, send):
    if scope["type"] == "http":
        await send({"type": "http.response.start", "status": 200, "headers": []})
        await send({"type": "http.response.body", "body": b"ok"})
    else:
        await send({"type": "lifespan.startup.complete"})

async def call(scope):
    sent = []
    async def send(m):
        sent.append(m)
    async def receive():
        return {"type": "lifespan.startup"}
    await cls(inner)(scope, receive, send)
    return sent

def http(method, path, auth=None):
    headers = [(b"authorization", auth)] if auth else []
    sent = asyncio.run(call({"type": "http", "method": method, "path": path, "headers": headers}))
    return sent[0]["status"]

key = b"Bearer k3y"
out = [
    http("GET", "/health"),
    http("POST", "/health"),
    http("POST", "/invocations"),
    http("GET", "/docs"),
    http("GET", "/metrics", b"Bearer wrong"),
    http("GET", "/metrics", b"bearer k3y"),
    http("POST", "/tokenize", key),
    http("POST", "/v1/chat/completions", key),
    asyncio.run(call({"type": "lifespan"}))[0]["type"],
    asyncio.run(call({"type": "websocket", "path": "/v1/realtime", "headers": []}))[0]["code"],
]
print(out)
"#;
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: no python3");
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "amicu-auth-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(dir.join("harness.py"), HARNESS).unwrap();
    let vllm = bin.join("vllm");
    std::fs::write(
        &vllm,
        format!(
            "#!/bin/sh\nexec python3 {} \"$@\"\n",
            dir.join("harness.py").display()
        ),
    )
    .unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&vllm)
        .status()
        .unwrap();
    let (entrypoint, cmd) = vllm_pod_command(&["vllm".into(), "serve".into(), "it's".into()]);
    let script = cmd[0].replace(VLLM_AUTH_DIR, &dir.join("auth").display().to_string());
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = std::process::Command::new(&entrypoint[0])
        .arg(&entrypoint[1])
        .arg(&script)
        .env("PATH", path)
        .env("VLLM_API_KEY", "k3y")
        .env_remove("PYTHONPATH")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "[200, 401, 401, 401, 401, 401, 200, 200, 'lifespan.startup.complete', 1008]"
    );
}

#[tokio::test]
async fn readiness_waits_for_running_then_health() {
    let (sup, mock) = supervisor(create_mock(
        &[
            Canned::Status(200, pod("PROVISIONING")),
            Canned::Status(200, pod("STARTING")),
            Canned::Status(200, pod("RUNNING")),
        ],
        &[
            Canned::Status(502, ""),
            Canned::Timeout,
            Canned::Status(200, "{}"),
        ],
        &[Canned::Status(401, "")],
        &[Canned::Status(200, "{}")],
    ));
    sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap();
    assert_eq!(count(&mock, HttpMethod::Get, HEALTH), 3);
}

#[tokio::test]
async fn an_error_or_exited_pod_fails_at_once_and_stays_recorded() {
    for status in ["ERROR", "EXITED"] {
        let (sup, mock) = supervisor(create_mock(
            &[Canned::Status(200, pod(status))],
            &[Canned::Status(200, "{}")],
            &[Canned::Status(401, "")],
            &[Canned::Status(200, "{}")],
        ));
        let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
        assert!(err_text(&e).contains(status), "{e}");
        assert_eq!(count(&mock, HttpMethod::Get, POD), 1, "no polling");
        assert_eq!(count(&mock, HttpMethod::Post, ACTION), 0, "left in place");
        assert_eq!(sup.deployed_pod().lock().unwrap().as_deref(), Some("pod1"));
    }
}

#[tokio::test]
async fn readiness_times_out_and_keeps_the_pod() {
    let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = lines.clone();
    let (sup, mock) = supervisor(
        create_mock(
            &[Canned::Status(200, pod("STARTING"))],
            &[Canned::Status(200, "{}")],
            &[Canned::Status(401, "")],
            &[Canned::Status(200, "{}")],
        )
        .ok(
            HttpMethod::Get,
            "/pods/pod1/logs?tail=8",
            200,
            "data: {\"ts\":\"t\",\"line\":\"pulling image\"}\n\n",
        ),
    );
    let sup = sup.with_progress(Duration::from_millis(20), move |l| {
        seen.lock().unwrap().push(l.to_owned());
    });
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    let text = e.to_string();
    assert!(text.contains("STARTING"), "{e}");
    assert!(text.contains("`amicu runpod logs chat`"), "{text}");
    assert!(
        text.ends_with("\nlast log lines:\n  [pod1] pulling image"),
        "{text}"
    );
    let lines = lines.lock().unwrap();
    assert!(
        lines
            .first()
            .is_some_and(|l| l.starts_with("waiting for RunPod pod pod1: ")
                && l.ends_with("elapsed, STARTING")),
        "{lines:?}"
    );
    assert_eq!(count(&mock, HttpMethod::Post, ACTION), 0);
    assert_eq!(sup.deployed_pod().lock().unwrap().as_deref(), Some("pod1"));
}

#[tokio::test]
async fn an_open_pod_is_terminated_before_its_secrets_are_removed() {
    let (sup, mock) = supervisor(create_mock(
        &[Canned::Status(200, pod("RUNNING"))],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(200, "{}")],
    ));
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    let text = err_text(&e);
    assert!(
        text.contains("unauthenticated") && text.contains("was terminated"),
        "{text}"
    );
    assert_eq!(
        body_of(&mock, HttpMethod::Post, ACTION),
        json!({"action": "terminate"})
    );
    assert!(
        position(&mock, HttpMethod::Post, ACTION)
            < position(&mock, HttpMethod::Delete, "/account/secrets/s1")
    );
    assert!(sup.deployed_pod().lock().unwrap().is_none());
    assert!(sup.created_secrets().lock().unwrap().is_empty());
}

#[tokio::test]
async fn no_definite_refusal_fails_closed() {
    let (sup, mock) = supervisor(create_mock(
        &[Canned::Status(200, pod("RUNNING"))],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(502, "")],
        &[Canned::Status(200, "{}")],
    ));
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&e).contains("could not verify"), "{e}");
    assert_eq!(count(&mock, HttpMethod::Post, ACTION), 1);
}

#[tokio::test]
async fn a_failed_teardown_says_so() {
    let mock = create_mock(
        &[Canned::Status(200, pod("RUNNING"))],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(200, "{}")],
    )
    .first(HttpMethod::Post, ACTION, &[Canned::Status(500, "{}")]);
    let (sup, mock) = supervisor(mock);
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&e).contains("FAILED"), "{e}");
    assert_eq!(count(&mock, HttpMethod::Delete, "/account/secrets/s1"), 0);
    assert_eq!(sup.deployed_pod().lock().unwrap().as_deref(), Some("pod1"));
}

#[tokio::test]
async fn a_pod_outside_the_region_is_terminated() {
    let outside = pod_with("STARTING", TYPE, "US-TX-3", &owner());
    let (sup, mock) = supervisor(create_mock(
        &[Canned::Status(200, outside)],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(401, "")],
        &[Canned::Status(200, "{}")],
    ));
    let region = Region {
        code: "eu".into(),
        residency: Residency::Eu,
        display_name: None,
        country: None,
        default_zone: None,
    };
    let sup = sup.with_region(Some(&region)).unwrap();
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(err_text(&e).contains("US-TX-3"), "{e}");
    assert_eq!(
        body_of(&mock, HttpMethod::Post, ACTION),
        json!({"action": "terminate"})
    );
}

#[tokio::test]
async fn a_rejected_key_is_reported_and_the_pod_kept() {
    let (sup, mock) = supervisor(create_mock(
        &[Canned::Status(200, pod("RUNNING"))],
        &[Canned::Status(200, "{}")],
        &[Canned::Status(401, "")],
        &[Canned::Status(401, "")],
    ));
    let e = sup.start(&vllm_plan(), &vllm_secrets()).await.unwrap_err();
    assert!(
        err_text(&e).contains("rejected the deployment's api_key"),
        "{e}"
    );
    assert_eq!(count(&mock, HttpMethod::Post, ACTION), 0);
    assert!(!err_text(&e).contains(KEY));
}

// --- status / stop ---------------------------------------------------------------

fn handle() -> RuntimeHandle {
    RuntimeHandle {
        id: "pod1".into(),
        endpoint: "https://pod1-8000.proxy.runpod.net".into(),
        deployment: amicu_core::id::DeploymentId::from(DEP),
        exposure: EndpointExposure::Public {
            gateway: GatewayAuth::None,
        },
    }
}

#[tokio::test]
async fn status_maps_pod_states() {
    for (body, want_running, needle) in [
        (pod("RUNNING"), true, "$5.38/hr"),
        (pod("STARTING"), true, "STARTING"),
        (pod("EXITED"), false, "EXITED"),
        (pod("ERROR"), false, "ERROR"),
    ] {
        let (sup, _) = supervisor(Mock::default().ok(HttpMethod::Get, POD, 200, body));
        let s = sup.status(&handle()).await.unwrap();
        assert_eq!(s.is_running(), want_running, "{s:?}");
        assert!(format!("{s:?}").contains(needle), "{s:?}");
    }
    let (sup, _) = supervisor(Mock::default().ok(HttpMethod::Get, POD, 404, "{}"));
    assert!(matches!(
        sup.status(&handle()).await.unwrap(),
        RuntimeStatus::Stopped { .. }
    ));
}

fn stop_mock(terminate: u16) -> Mock {
    Mock::default()
        .ok(HttpMethod::Post, ACTION, terminate, "")
        .ok(HttpMethod::Get, "/account/secrets?name=", 200, FOUND_SECRET)
        .ok(HttpMethod::Delete, "/account/secrets/s1", 204, "")
}

#[tokio::test]
async fn stop_terminates_then_deletes_secrets() {
    let (sup, mock) = supervisor(stop_mock(204));
    let sup = sup.with_recorded_secrets(vec!["recorded".into()]);
    assert_eq!(sup.stop_pod(&handle()).await.unwrap(), StopOutcome::Deleted);
    let seen = mock.requests();
    assert_eq!(seen[0].method, HttpMethod::Post);
    assert!(seen[0].url.ends_with(ACTION));
    assert_eq!(seen[0].body, Some(json!({"action": "terminate"})));
    assert!(
        seen.iter().all(|s| s.method != HttpMethod::Patch),
        "never a stop"
    );
    assert_eq!(count(&mock, HttpMethod::Delete, "/account/secrets/s1"), 1);
    assert!(sup.created_secrets().lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unconfirmed_404_keeps_the_secrets() {
    let (sup, mock) = supervisor(stop_mock(404));
    let sup = sup.with_recorded_secrets(vec!["recorded".into()]);
    assert_eq!(
        sup.stop_pod(&handle()).await.unwrap(),
        StopOutcome::AlreadyGone {
            kept_secrets: vec!["recorded".into()]
        }
    );
    assert_eq!(count(&mock, HttpMethod::Delete, "/account/secrets/s1"), 0);
    let e = sup.stop(&handle()).await.unwrap_err();
    assert!(err_text(&e).contains("`recorded`"), "{e}");

    let (sup, mock) = supervisor(stop_mock(404));
    let sup = sup
        .with_recorded_secrets(vec!["recorded".into()])
        .with_confirmed_account(true);
    sup.stop(&handle()).await.unwrap();
    assert_eq!(count(&mock, HttpMethod::Delete, "/account/secrets/s1"), 1);
}

#[tokio::test]
async fn a_conflicting_terminate_is_resolved_by_reading_the_pod() {
    let (sup, mock) = supervisor(stop_mock(409).ok(HttpMethod::Get, POD, 200, pod("TERMINATED")));
    let sup = sup.with_recorded_secrets(vec!["recorded".into()]);
    assert_eq!(sup.stop_pod(&handle()).await.unwrap(), StopOutcome::Deleted);
    assert_eq!(count(&mock, HttpMethod::Delete, "/account/secrets/s1"), 1);

    let (sup, mock) = supervisor(stop_mock(409).ok(HttpMethod::Get, POD, 200, pod("RUNNING")));
    let sup = sup.with_recorded_secrets(vec!["recorded".into()]);
    assert!(sup.stop_pod(&handle()).await.is_err());
    assert_eq!(count(&mock, HttpMethod::Delete, "/account/secrets/s1"), 0);
}

#[tokio::test]
async fn a_failed_terminate_touches_no_secret() {
    let (sup, mock) = supervisor(stop_mock(500));
    let sup = sup.with_recorded_secrets(vec!["recorded".into()]);
    assert!(sup.stop(&handle()).await.is_err());
    assert_eq!(count(&mock, HttpMethod::Get, "/account/secrets?name="), 0);
    assert_eq!(*sup.created_secrets().lock().unwrap(), Vec::<String>::new());
}
