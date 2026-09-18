//! Live integration tests for the Modal deploy path.
//!
//! These are the only way to settle **H1** (a module baked into the image is
//! importable with `mount_ids = []`) and **H2** (the `Function` field set for
//! a class-based server is accepted by `FunctionCreate` from a non-Python
//! client) — see the crate-level docs. Everything else about the deploy path
//! is covered by the offline unit tests in `src/`.
//!
//! Both are `#[ignore]`d: they need network access, a real Modal token, and
//! real (small) money. They **skip** rather than fail when
//! `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` are absent, so a developer without a
//! Modal account can still run `cargo test -- --ignored` without a red build.
//!
//! ```sh
//! # (a) the cheap one — CPU only, settles H1 and H2 for cents
//! MODAL_TOKEN_ID=ak-... MODAL_TOKEN_SECRET=as-... \
//!     cargo test -p amicu-modal --test live_deploy -- --ignored --nocapture \
//!     deploy_cpu_http_server
//!
//! # (b) the GPU smoke test — one L4, a 0.5B model
//! MODAL_TOKEN_ID=ak-... MODAL_TOKEN_SECRET=as-... \
//!     cargo test -p amicu-modal --test live_deploy -- --ignored --nocapture \
//!     deploy_vllm_smoke
//! ```
//!
//! # Cost
//!
//! * `deploy_cpu_http_server`: a `python:3.12-slim` image build (tens of
//!   seconds of builder CPU) plus well under a minute of a CPU-only container.
//!   Single-digit cents; comfortably inside Modal's $30/month free credit.
//! * `deploy_vllm_smoke`: a first-run image pull of `vllm/vllm-openai`
//!   (several GiB) plus roughly 3-6 minutes of one L4 while vLLM loads
//!   Qwen2.5-0.5B into the HF cache Volume. At Modal's L4 rate that is well
//!   under $0.10 per run; re-runs are faster because both the image and the
//!   weights are cached. `--enforce-eager` is deliberate: it trades inference
//!   throughput for a much shorter (and therefore cheaper) startup.
//!
//! Both tests call `stop_app` in a cleanup path even when the assertion fails,
//! so a failing run does not leave a GPU replica scaling up behind it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use amicu_modal::{Credentials, Deployment, ImageSpec, ModalClient, ServerSpec, render_shim};

/// Returns `None` (and prints why) when the token env vars are not set.
fn credentials_or_skip(test: &str) -> Option<Credentials> {
    match Credentials::from_env() {
        Ok(credentials) => Some(credentials),
        Err(missing) => {
            println!("skipping {test}: {missing}");
            None
        }
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// Polls `url` until it answers 200 or `deadline` passes.
async fn wait_for_http_200(url: &str, deadline: Duration) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("building the HTTP client: {e}"))?;
    let started = Instant::now();
    let mut last = "no attempt made".to_owned();

    while started.elapsed() < deadline {
        match client.get(url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => last = format!("HTTP {}", response.status()),
            Err(error) => last = format!("{error}"),
        }
        println!("  {:>4}s {url}: {last}", started.elapsed().as_secs());
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    Err(format!("gave up after {deadline:?}; last was {last}"))
}

/// Runs `check` against the deployment, then stops the app either way.
async fn with_cleanup(
    client: &mut ModalClient,
    deployment: &Deployment,
    check: impl std::future::Future<Output = Result<(), String>>,
) {
    let outcome = check.await;
    if let Err(error) = client.stop_app(&deployment.app_id).await {
        println!("warning: AppStop for {} failed: {error}", deployment.app_id);
    } else {
        println!("stopped {}", deployment.app_id);
    }
    if let Err(error) = outcome {
        panic!("{error}");
    }
}

async fn deploy(spec: &ServerSpec) -> (ModalClient, Deployment) {
    let credentials = Credentials::from_env().expect("checked by credentials_or_skip");
    let mut client = ModalClient::connect(credentials)
        .await
        .expect("connect to Modal");
    // `Box::pin`: `deploy_server` awaits a long chain of tonic calls, so its
    // future is a few tens of KiB — too big for the test thread's stack budget
    // (clippy::large_futures).
    let deployment = Box::pin(client.deploy_server(spec, &mut |line| print!("{line}")))
        .await
        .expect("deploy the server");
    println!(
        "deployed app={} function={} url={}",
        deployment.app_id, deployment.function_id, deployment.url
    );
    (client, deployment)
}

/// The cheap experiment: no GPU, no model, a stdlib HTTP server. If this
/// returns 200 then H1 and H2 both hold — the baked module was imported, the
/// lifecycle hooks ran, and Modal's proxy reached the process.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MODAL_TOKEN_ID/MODAL_TOKEN_SECRET, network access, and a few cents; see module docs"]
async fn deploy_cpu_http_server() {
    if credentials_or_skip("deploy_cpu_http_server").is_none() {
        return;
    }

    let argv = argv(&["python", "-m", "http.server", "8000"]);
    let spec = ServerSpec {
        app_name: "amicu-live-cpu-http".to_owned(),
        gpu: None,
        region: None,
        port: 8000,
        image: ImageSpec {
            base_registry_image: "python:3.12-slim".to_owned(),
            pip_packages: Vec::new(),
            env: BTreeMap::new(),
            shim_source: render_shim(&argv, 8000),
        },
        argv,
        volumes: Vec::new(),
        secret_env: BTreeMap::new(),
        scaledown_window_secs: 60,
        startup_timeout_secs: 120,
        timeout_secs: 600,
        target_concurrency: 10,
        min_containers: 1,
        max_containers: Some(1),
    };

    let (mut client, deployment) = Box::pin(deploy(&spec)).await;
    let url = deployment.url.clone();
    with_cleanup(&mut client, &deployment, async move {
        wait_for_http_200(&url, Duration::from_secs(spec.startup_timeout_secs.into())).await
    })
    .await;
}

/// The GPU smoke test: the smallest useful vLLM server, asserting that the
/// OpenAI-compatible `/v1/models` endpoint answers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MODAL_TOKEN_ID/MODAL_TOKEN_SECRET, network access, and an L4 for a few minutes; see module docs"]
async fn deploy_vllm_smoke() {
    if credentials_or_skip("deploy_vllm_smoke").is_none() {
        return;
    }

    let argv = argv(&[
        "vllm",
        "serve",
        "Qwen/Qwen2.5-0.5B-Instruct",
        "--host",
        "0.0.0.0",
        "--port",
        "8000",
        "--enforce-eager",
        "--max-model-len",
        "2048",
    ]);
    let spec = ServerSpec {
        app_name: "amicu-live-vllm-smoke".to_owned(),
        gpu: Some("L4".to_owned()),
        region: None,
        port: 8000,
        image: ImageSpec {
            // Pinned: the official vLLM server image already contains Python
            // 3.12, CUDA userspace, and vLLM, so the only thing amicu adds is
            // the `modal` package and the shim. `v0.11.0` is the tag used
            // here; bump it deliberately, and re-run this test when you do.
            base_registry_image: "vllm/vllm-openai:v0.11.0".to_owned(),
            pip_packages: Vec::new(),
            env: BTreeMap::from([("HF_XET_HIGH_PERFORMANCE".to_owned(), "1".to_owned())]),
            shim_source: render_shim(&argv, 8000),
        },
        argv,
        volumes: vec![(
            "amicu-live-hf-cache".to_owned(),
            "/root/.cache/huggingface".to_owned(),
        )],
        secret_env: BTreeMap::new(),
        scaledown_window_secs: 120,
        startup_timeout_secs: 600,
        timeout_secs: 1800,
        target_concurrency: 8,
        min_containers: 1,
        max_containers: Some(1),
    };

    let (mut client, deployment) = Box::pin(deploy(&spec)).await;
    let url = format!("{}/v1/models", deployment.url.trim_end_matches('/'));
    with_cleanup(&mut client, &deployment, async move {
        wait_for_http_200(&url, Duration::from_secs(spec.startup_timeout_secs.into())).await
    })
    .await;
}
