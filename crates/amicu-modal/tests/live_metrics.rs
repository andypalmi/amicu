//! Live integration test for `amicu_modal::metrics::scrape` over a real Modal
//! URL with proxy auth. `#[ignore]`d, and **skips** (not fails) without
//! `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` or, because Modal Servers reject
//! unauthenticated requests, without `MODAL_PROXY_TOKEN_ID`/
//! `MODAL_PROXY_TOKEN_SECRET`.
//!
//! ```sh
//! MODAL_TOKEN_ID=ak-... MODAL_TOKEN_SECRET=as-... \
//! MODAL_PROXY_TOKEN_ID=wk-... MODAL_PROXY_TOKEN_SECRET=ws-... \
//!     cargo test -p amicu-modal --test live_metrics -- --ignored --nocapture
//! ```
//!
//! Cheap (CPU, cents): rather than a GPU runtime, the container is a plain
//! `python -m http.server` whose docroot holds a file named `metrics` with
//! captured vLLM exposition text, so the scrape, proxy-auth headers, and
//! parser are exercised end to end against the real tunnel. Real runtime
//! output is covered by the fixture unit tests. An [`common::AppGuard`] stops
//! the app however the test ends.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use amicu_modal::metrics::{MetricsRuntime, scrape};
use amicu_modal::proxy_auth::{TokenState, env_state};
use amicu_modal::{Credentials, HealthStatus, ImageSpec, ModalClient, ServerSpec};

mod common;
use common::AppGuard;

const PORT: u16 = 8000;

const FIXTURE: &str = "vllm:num_requests_running{model_name=\"m\"} 2.0\\n\
vllm:kv_cache_usage_perc{model_name=\"m\"} 0.5\\n\
vllm:generation_tokens_total{model_name=\"m\"} 42.0\\n\
vllm:request_success_total{finished_reason=\"stop\",model_name=\"m\"} 7.0\\n";

fn unique_app_name(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    let suffix = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or_default();
    format!("{prefix}-{suffix:x}")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MODAL_TOKEN_*, MODAL_PROXY_TOKEN_*, network access, and a few cents"]
async fn scrape_a_cpu_server_through_the_proxy() {
    let test = "scrape_a_cpu_server_through_the_proxy";
    if let Err(missing) = Credentials::from_env() {
        println!("skipping {test}: {missing}");
        return;
    }
    if env_state() != TokenState::Complete {
        println!("skipping {test}: MODAL_PROXY_TOKEN_ID/MODAL_PROXY_TOKEN_SECRET not both set");
        return;
    }

    let app_name = unique_app_name("amicu-live-metrics");
    let spec = ServerSpec {
        app_name: app_name.clone(),
        gpu: None,
        region: None,
        proxy_regions: vec!["us-east".to_owned()],
        port: PORT,
        ready_path: None,
        argv: [
            "python",
            "-m",
            "http.server",
            &PORT.to_string(),
            "--directory",
            "/srv",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        image: ImageSpec {
            base_registry_image: "python:3.12-slim".to_owned(),
            pip_packages: Vec::new(),
            setup_commands: vec![format!(
                "mkdir -p /srv && printf '{}' > /srv/metrics",
                FIXTURE.replace('\'', "")
            )],
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
        memory_mb: None,
        milli_cpu: None,
    };

    let _guard = AppGuard::install(app_name.clone()).await;
    let mut client = common::connect().await;
    let deployment = Box::pin(client.deploy_server(&spec, &mut |line| print!("{line}")))
        .await
        .expect("deploy the CPU server");
    println!("deployed {} at {}", deployment.app_id, deployment.url);

    let deadline = Instant::now() + Duration::from_secs(spec.startup_timeout_secs.into());
    loop {
        let probe = ModalClient::health(&deployment.url, "/metrics", Duration::from_secs(10)).await;
        if let Some(hint) = probe.auth_hint() {
            panic!("{}: {hint}", deployment.url);
        }
        if matches!(probe, HealthStatus::Ok(200)) {
            break;
        }
        assert!(Instant::now() < deadline, "server never came up");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let snapshot = scrape(&deployment.url).await.expect("scrape /metrics");
    assert_eq!(snapshot.runtime, MetricsRuntime::Vllm);
    assert_eq!(snapshot.requests_total, Some(7));
    assert_eq!(snapshot.requests_running, Some(2));
    assert_eq!(snapshot.generation_tokens_total, Some(42));
    assert_eq!(snapshot.kv_cache_usage, Some(0.5));
}
