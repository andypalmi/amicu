//! Live end-to-end test: deploys a tiny llama.cpp model as a RunPod
//! serverless endpoint, sends one chat request, destroys it, and checks that
//! nothing is left. Billed: a few cents of L4 time. Never run in CI.
//!
//! `#[ignore]`d, and a no-op unless `AMICU_RUNPOD_LIVE=1`:
//!
//! ```sh
//! AMICU_RUNPOD_LIVE=1 RUNPOD_API_KEY=... AMICU_RUNPOD_INVOKE_KEY=... \
//!     cargo test -p amicu-runpod --test live_serverless -- --ignored --nocapture
//! ```
//!
//! - `RUNPOD_API_KEY`: management key (creates and deletes the endpoint).
//! - `AMICU_RUNPOD_INVOKE_KEY`: a RunPod key with api.runpod.ai access, used
//!   as the deployment's `api_key` (the gateway bearer).
//! - `AMICU_RUNPOD_LIVE_GPU`: accelerator model (default `l4-24gb`).
//! - `AMICU_RUNPOD_LIVE_MODEL`: `-hf` reference (default a 135M GGUF).
//! - `AMICU_RUNPOD_LIVE_VOLUME_DC`: when set, a temporary 10 GB network volume
//!   `amicu-live-test-volume` is created in that data center, mounted as the
//!   model cache and deleted at the end. After the first answer the workers
//!   are drained and the endpoint restarts with `LLAMA_ARG_OFFLINE=1`
//!   (llama.cpp may then only read its cache): a second answer proves the
//!   model came from the volume.
//!
//! Teardown runs whatever the outcome: the scenario runs in a spawned task,
//! so a panic is caught before the endpoint, its secrets and the volume are
//! removed and their absence confirmed.

use std::sync::Arc;
use std::time::Duration;

use amicu_core::ByteSize;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{Accelerator, AcceleratorKind, LlamaCppSettings, Runtime};
use amicu_core::secret::SecretValue;
use amicu_llamacpp::{ProcessSupervisor, ResolvedSecret};
use amicu_providers::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport};
use amicu_runpod::{
    ApiKey, BASE_URL, NetworkVolume, ReadinessTiming, RunPodClient, RunPodSupervisor,
    UpdateEndpoint, WorkersConfig, endpoint_url, project_endpoint_name,
};
use amicu_runtime::{
    EndpointExposure, GatewayAuth, LaunchPlan, RenderedLaunch, ResourceEstimate, RuntimeHandle,
    RuntimeKind,
};
use serde_json::{Value, json};

const DEPLOYMENT: &str = "live";
const VOLUME_NAME: &str = "amicu-live-test-volume";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn client(key: &str) -> RunPodClient {
    RunPodClient::new(ApiKey::new(key))
}

fn launch_plan(model: &str, offline: bool) -> LaunchPlan {
    let argv: Vec<String> = ["llama-server", "-hf", model, "--port", "8080", "-c", "2048"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let mut s = LlamaCppSettings {
        port: 8080,
        context_length: 2048,
        ..Default::default()
    };
    if offline {
        s.env.insert("LLAMA_ARG_OFFLINE".to_owned(), "1".to_owned());
    }
    let settings = Runtime::LlamaCpp(s);
    LaunchPlan {
        deployment: DeploymentId::from(DEPLOYMENT),
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
            gpu_count: 1,
            kv_cache: None,
        },
    }
}

async fn raw(key: &str, method: HttpMethod, path: &str, body: Option<Value>) -> (u16, Value) {
    let mut req = HttpRequest::new(method, format!("{BASE_URL}{path}")).with_bearer(key);
    if let Some(b) = body {
        req = req.with_json(b);
    }
    match ReqwestTransport::new().execute(req).await {
        Ok(r) => (
            r.status,
            serde_json::from_slice(&r.body).unwrap_or(Value::Null),
        ),
        Err(e) => (0, json!(e.to_string())),
    }
}

/// The created volume, or the id RunPod returned (if any) and why it failed.
async fn create_volume(key: &str, dc: &str) -> Result<NetworkVolume, (Option<String>, String)> {
    let (status, body) = raw(
        key,
        HttpMethod::Post,
        "/network-volumes",
        Some(json!({"name": VOLUME_NAME, "size": 10, "dataCenter": dc})),
    )
    .await;
    let id = body["id"].as_str().map(str::to_owned);
    if !(200..300).contains(&status) {
        return Err((id, format!("volume create: {status} {body}")));
    }
    serde_json::from_value(body).map_err(|e| (id, format!("volume body: {e}")))
}

/// Deletes the test volume (retrying while the endpoint still holds it) and
/// returns whether a `GET` then answers `404`.
async fn delete_volume(key: &str, id: &str) -> bool {
    let path = format!("/network-volumes/{id}");
    for _ in 0..12 {
        let (status, body) = raw(key, HttpMethod::Delete, &path, None).await;
        if (200..300).contains(&status) || status == 404 {
            break;
        }
        eprintln!("volume delete answered {status} {body}; retrying");
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    raw(key, HttpMethod::Get, &path, None).await.0 == 404
}

async fn chat(id: &str, invoke: &str) -> Result<String, String> {
    let url = format!("{}/v1/chat/completions", endpoint_url(id));
    let body = json!({"model": "live", "max_tokens": 8,
        "messages": [{"role": "user", "content": "Say hello."}]});
    let mut last = String::new();
    for _ in 0..10 {
        let req = HttpRequest::new(HttpMethod::Post, url.clone())
            .with_bearer(invoke)
            .with_json(body.clone());
        match ReqwestTransport::new().execute(req).await {
            Ok(r) if r.status == 200 => {
                let v: Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
                return v["choices"][0]["message"]["content"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("no content in {v}"));
            }
            Ok(r) => last = format!("HTTP {}", r.status),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
    Err(format!("chat never answered 200 (last: {last})"))
}

/// Scales the endpoint to zero workers and waits until none is left, so
/// the next start runs on a fresh worker.
async fn drain(client: &RunPodClient, id: &str) -> Result<(), String> {
    let zero = UpdateEndpoint {
        workers: Some(WorkersConfig {
            min: 0,
            max: 0,
            idle_timeout: None,
        }),
        ..UpdateEndpoint::default()
    };
    client
        .update_endpoint(id, &zero)
        .await
        .map_err(|e| format!("drain: {e}"))?;
    for _ in 0..36 {
        if let Ok(w) = client.list_workers(id).await
            && w.workers.is_empty()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    Err("workers did not drain within 3 minutes".to_owned())
}

async fn scenario(
    sup: Arc<RunPodSupervisor>,
    invoke: String,
    model: String,
    volume: Option<String>,
) -> Result<(), String> {
    let secrets = vec![ResolvedSecret {
        name: RuntimeKind::LlamaCpp.api_key_env_var().to_owned(),
        value: SecretValue::new(invoke.clone()),
    }];
    let handle = sup
        .start(&launch_plan(&model, false), &secrets)
        .await
        .map_err(|e| format!("start: {e}"))?;
    eprintln!("endpoint {} ready", handle.id);
    if let Some(v) = &volume {
        let ep = sup
            .client()
            .get_endpoint(&handle.id)
            .await
            .map_err(|e| format!("read endpoint: {e}"))?;
        if ep.network_volumes.as_deref() != Some(std::slice::from_ref(v)) {
            return Err(format!(
                "endpoint mounts {:?}, expected [{v}]",
                ep.network_volumes
            ));
        }
        eprintln!("endpoint mounts volume {v}");
    }
    let reply = chat(&handle.id, &invoke).await?;
    eprintln!("chat reply: {reply:?}");
    if volume.is_some() {
        drain(sup.client(), &handle.id).await?;
        let again = sup
            .start(&launch_plan(&model, true), &secrets)
            .await
            .map_err(|e| format!("offline restart (model not in the volume cache?): {e}"))?;
        let reply = chat(&again.id, &invoke).await?;
        eprintln!("offline chat reply from the volume cache: {reply:?}");
    }
    Ok(())
}

async fn teardown(key: &str, sup: &RunPodSupervisor, project: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let created: Vec<String> = sup.created_secrets().lock().unwrap().clone();
    let name = project_endpoint_name(project, DEPLOYMENT);
    let mut ids: Vec<String> = sup
        .deployed_endpoint()
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    if let Ok(found) = sup.client().find_endpoints_by_name(&name).await {
        for e in found {
            if !ids.contains(&e.id) {
                ids.push(e.id);
            }
        }
    }
    for id in ids {
        let handle = RuntimeHandle {
            endpoint: endpoint_url(&id),
            id: id.clone(),
            deployment: DeploymentId::from(DEPLOYMENT),
            exposure: EndpointExposure::Public {
                gateway: GatewayAuth::RunPodBearer,
            },
        };
        if let Err(e) = sup.stop_endpoint(&handle).await {
            problems.push(format!("stop {id}: {e}"));
        }
    }
    let c = client(key);
    match c.list_endpoints().await {
        Ok(eps) if eps.iter().any(|e| e.name == name) => {
            problems.push(format!("endpoint `{name}` still listed"));
        }
        Ok(_) => {}
        Err(e) => problems.push(format!("list endpoints: {e}")),
    }
    match c.list_pods().await {
        Ok(pods) if pods.iter().any(|p| p.name == name) => {
            problems.push(format!("pod `{name}` still listed"));
        }
        Ok(_) => {}
        Err(e) => problems.push(format!("list pods: {e}")),
    }
    match c.list_secrets().await {
        Ok(secrets) => {
            for s in secrets.iter().filter(|s| created.contains(&s.name)) {
                problems.push(format!("secret `{}` still listed", s.name));
                let _ = c.delete_secret(&s.id).await;
            }
        }
        Err(e) => problems.push(format!("list secrets: {e}")),
    }
    problems
}

#[tokio::test]
#[ignore = "live: needs AMICU_RUNPOD_LIVE=1 and RunPod keys, bills a few cents; see module docs"]
async fn serverless_llamacpp_deploys_answers_and_leaves_nothing_behind() {
    if env("AMICU_RUNPOD_LIVE").as_deref() != Some("1") {
        eprintln!("AMICU_RUNPOD_LIVE is not 1: skipped");
        return;
    }
    let key = env("RUNPOD_API_KEY").expect("RUNPOD_API_KEY");
    let invoke = env("AMICU_RUNPOD_INVOKE_KEY").expect("AMICU_RUNPOD_INVOKE_KEY");
    let gpu = env("AMICU_RUNPOD_LIVE_GPU").unwrap_or_else(|| "l4-24gb".to_owned());
    let model = env("AMICU_RUNPOD_LIVE_MODEL")
        .unwrap_or_else(|| "bartowski/SmolLM2-135M-Instruct-GGUF:Q4_K_M".to_owned());
    let project = format!(
        "live{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            & 0xff_ffff
    );

    let accelerator = Accelerator {
        kind: AcceleratorKind::Gpu,
        model: gpu,
        count: 1,
        memory: ByteSize::from_bytes(24 * (1 << 30)),
        memory_bandwidth_gbps: None,
    };
    let mut sup = RunPodSupervisor::new(client(&key), &project, DEPLOYMENT)
        .with_workspace("amicu-live-test")
        .with_accelerator(Some(&accelerator))
        .expect("accelerator")
        .with_container_disk_gb(20)
        .with_readiness_timing(ReadinessTiming {
            timeout: Duration::from_mins(12),
            ..ReadinessTiming::DEFAULT
        })
        .with_progress(Duration::from_secs(30), |l| eprintln!("{l}"));
    let volume = match env("AMICU_RUNPOD_LIVE_VOLUME_DC") {
        Some(dc) => match create_volume(&key, &dc).await {
            Ok(v) => Some(v),
            Err((id, why)) => {
                let gone = match id {
                    Some(id) => delete_volume(&key, &id).await,
                    None => true,
                };
                panic!("{why} (volume deleted or never created: {gone})");
            }
        },
        None => None,
    };
    if let Some(v) = &volume {
        eprintln!("volume {} in {}", v.id, v.data_center);
        sup = match sup.with_network_volume(v) {
            Ok(s) => s,
            Err(e) => {
                let gone = delete_volume(&key, &v.id).await;
                panic!("volume placement: {e} (volume deleted: {gone})");
            }
        };
    }
    let sup = Arc::new(sup);

    let outcome = tokio::spawn(scenario(
        Arc::clone(&sup),
        invoke,
        model,
        volume.as_ref().map(|v| v.id.clone()),
    ))
    .await;
    let mut problems = teardown(&key, &sup, &project).await;
    if let Some(v) = &volume
        && !delete_volume(&key, &v.id).await
    {
        problems.push(format!("network volume {} still exists", v.id));
    }
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => problems.insert(0, e),
        Err(panic) => problems.insert(0, format!("scenario panicked: {panic}")),
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
