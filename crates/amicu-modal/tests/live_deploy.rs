//! Live integration tests for the Modal deploy path.
//!
//! These are the only way to settle **H1** (the shim module baked into the
//! image is importable) and **H2** (the `Function` field set for a class-based
//! server is accepted by `FunctionCreate` from a non-Python client) — see the
//! crate-level docs. Everything else about the deploy path is covered by the
//! offline unit tests in `src/`.
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
//!   under $0.10 per run; re-runs are faster because the image is cached and
//!   the weights persist in the named Volume. `--enforce-eager` is
//!   deliberate: it trades inference throughput for a much shorter (and
//!   therefore cheaper) startup.
//!
//! # Cleanup
//!
//! Each test creates a **uniquely named** app and installs an [`AppGuard`]
//! *before* deploying. The guard's `Drop` runs on the success path, on an
//! assertion failure, and on a panic inside `deploy_server` itself — it looks
//! the app up by name and stops it. A test that dies before the app is created
//! leaves nothing behind; one that dies after leaves a stopped app.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

use amicu_modal::{Credentials, Deployment, ImageSpec, ModalClient, ServerSpec};

/// Both servers listen here; the same value reaches `ServerSpec::port` (and
/// so the shim's readiness probe and `http_config`) and the child's `--port`,
/// with no second literal to drift.
const PORT: u16 = 8000;

/// Returns `false` (and prints why) when the token env vars are not set.
fn have_credentials(test: &str) -> bool {
    match Credentials::from_env() {
        Ok(_) => true,
        Err(missing) => {
            println!("skipping {test}: {missing}");
            false
        }
    }
}

async fn connect() -> ModalClient {
    let credentials = Credentials::from_env().expect("checked by have_credentials");
    ModalClient::connect(credentials)
        .await
        .expect("connect to Modal")
}

/// A per-run app name, so a crashed run never collides with the next one and
/// each guard only ever stops its own app.
fn unique_app_name(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    let suffix = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or_default();
    format!("{prefix}-{suffix:x}")
}

/// Stops the named app on drop, however the test ends.
///
/// It holds its own `ModalClient` (a second channel to the same endpoint) so
/// cleanup does not depend on the client the test was using still being alive
/// or unborrowed, and it resolves the app by *name* so it also works when
/// `deploy_server` panicked before returning an id.
struct AppGuard {
    client: ModalClient,
    app_name: String,
}

impl Drop for AppGuard {
    fn drop(&mut self) {
        let app_name = self.app_name.clone();
        let client = &mut self.client;
        // `block_in_place` + the current handle is the only way to await from
        // `Drop`; it requires the multi-threaded runtime these tests use.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                match client.find_app(&app_name).await {
                    Ok(Some(app_id)) => match client.stop_app(&app_id).await {
                        Ok(()) => println!("cleanup: stopped {app_name} ({app_id})"),
                        Err(error) => println!("cleanup: AppStop for {app_name} failed: {error}"),
                    },
                    Ok(None) => println!("cleanup: no app named {app_name} to stop"),
                    Err(error) => println!("cleanup: lookup of {app_name} failed: {error}"),
                }
            });
        });
    }
}

/// Polls `url` until it answers 200 or `deadline` passes.
async fn wait_for_http_200(url: &str, deadline: Duration) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("building the HTTP client");
    let started = Instant::now();
    let mut last = "no attempt made".to_owned();

    while started.elapsed() < deadline {
        match client.get(url).send().await {
            Ok(response) if response.status().is_success() => {
                println!("  {:>4}s {url}: 200", started.elapsed().as_secs());
                return;
            }
            Ok(response) => last = format!("HTTP {}", response.status()),
            Err(error) => last = format!("{error}"),
        }
        println!("  {:>4}s {url}: {last}", started.elapsed().as_secs());
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    panic!("{url} never answered 200 within {deadline:?}; last was {last}");
}

async fn deploy(client: &mut ModalClient, spec: &ServerSpec) -> Deployment {
    // `Box::pin`: `deploy_server` awaits a long chain of tonic calls, so its
    // future is a few tens of KiB (clippy::large_futures).
    let deployment = Box::pin(client.deploy_server(spec, &mut |line| print!("{line}")))
        .await
        .expect("deploy the server");
    println!(
        "deployed app={} function={} url={}",
        deployment.app_id, deployment.function_id, deployment.url
    );
    deployment
}

/// The cheap experiment: no GPU, no model, a stdlib HTTP server. If this
/// returns 200 then H1 and H2 both hold — the baked module was imported, the
/// lifecycle hooks ran, and Modal's proxy reached the process.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MODAL_TOKEN_ID/MODAL_TOKEN_SECRET, network access, and a few cents; see module docs"]
async fn deploy_cpu_http_server() {
    if !have_credentials("deploy_cpu_http_server") {
        return;
    }

    let app_name = unique_app_name("amicu-live-cpu-http");
    let spec = ServerSpec {
        app_name: app_name.clone(),
        gpu: None,
        region: None,
        port: PORT,
        argv: ["python", "-m", "http.server", &PORT.to_string()]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        image: ImageSpec {
            base_registry_image: "python:3.12-slim".to_owned(),
            pip_packages: Vec::new(),
            env: BTreeMap::new(),
        },
        volumes: Vec::new(),
        secret_env: BTreeMap::new(),
        scaledown_window_secs: 60,
        startup_timeout_secs: 120,
        timeout_secs: 600,
        target_concurrency: 10,
        min_containers: 1,
        max_containers: Some(1),
    };

    // Installed before the deploy, so a panic inside `deploy_server` is
    // cleaned up too.
    let _guard = AppGuard {
        client: connect().await,
        app_name,
    };

    let mut client = connect().await;
    let deployment = deploy(&mut client, &spec).await;
    wait_for_http_200(
        &deployment.url,
        Duration::from_secs(spec.startup_timeout_secs.into()),
    )
    .await;
}

/// The GPU smoke test: the smallest useful vLLM server, asserting that the
/// OpenAI-compatible `/v1/models` endpoint answers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MODAL_TOKEN_ID/MODAL_TOKEN_SECRET, network access, and an L4 for a few minutes; see module docs"]
async fn deploy_vllm_smoke() {
    if !have_credentials("deploy_vllm_smoke") {
        return;
    }

    let port = PORT.to_string();
    let app_name = unique_app_name("amicu-live-vllm");
    let spec = ServerSpec {
        app_name: app_name.clone(),
        gpu: Some("L4".to_owned()),
        region: None,
        port: PORT,
        argv: [
            "vllm",
            "serve",
            "Qwen/Qwen2.5-0.5B-Instruct",
            "--host",
            "0.0.0.0",
            "--port",
            &port,
            "--enforce-eager",
            "--max-model-len",
            "2048",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        image: ImageSpec {
            // The official vLLM server image already contains Python 3.12, the
            // CUDA userspace, and vLLM, so amicu adds only the shim — Modal's
            // own client package arrives as a runtime mount, never as pip
            // packages that could conflict with vLLM's pins.
            base_registry_image: "vllm/vllm-openai:v0.29.0".to_owned(),
            pip_packages: Vec::new(),
            env: BTreeMap::from([("HF_XET_HIGH_PERFORMANCE".to_owned(), "1".to_owned())]),
        },
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

    let _guard = AppGuard {
        client: connect().await,
        app_name,
    };

    let mut client = connect().await;
    let deployment = deploy(&mut client, &spec).await;
    let url = format!("{}/v1/models", deployment.url.trim_end_matches('/'));
    wait_for_http_200(&url, Duration::from_secs(spec.startup_timeout_secs.into())).await;
}
