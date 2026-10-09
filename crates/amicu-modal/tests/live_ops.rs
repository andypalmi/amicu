//! Live integration test for `src/ops.rs`: status, log streaming, health,
//! and stop, after a CPU deploy. Like `tests/live_deploy.rs`, `#[ignore]`d
//! and **skips** (not fails) without a real Modal token, because it needs
//! network access and a few cents.
//!
//! ```sh
//! MODAL_TOKEN_ID=ak-... MODAL_TOKEN_SECRET=as-... \
//!     cargo test -p amicu-modal --test live_ops -- --ignored --nocapture
//! ```
//!
//! Settles whether a deployed server's URL lands on
//! `FunctionHandleMetadata.web_url` or `experimental_flash_urls` (offline
//! unit tests can only cover both branches of `ops::function_status`, not
//! which one Modal actually sends). After `stop_app`, `app_status` reports
//! no deployment by that name — `AppList` still lists the app itself, just
//! in the `"stopped"` state, so this checks that `find_app` no longer
//! resolves the *name* to an id, not that Modal forgot the app.
//!
//! Like `tests/live_deploy.rs`, an [`common::AppGuard`] is installed before
//! `deploy_server` so a panic between deploying and the explicit
//! `stop_app` call below (e.g. an assertion failure in `app_status` or
//! `stream_logs`) still stops the billed container.

use std::collections::BTreeMap;
use std::time::Duration;

use amicu_modal::{Credentials, HealthStatus, ImageSpec, ModalClient, ServerSpec};

mod common;
use common::AppGuard;

const PORT: u16 = 8000;

fn have_credentials(test: &str) -> bool {
    match Credentials::from_env() {
        Ok(_) => true,
        Err(missing) => {
            println!("skipping {test}: {missing}");
            false
        }
    }
}

fn unique_app_name(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    let suffix = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or_default();
    format!("{prefix}-{suffix:x}")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MODAL_TOKEN_ID/MODAL_TOKEN_SECRET, network access, and a few cents"]
async fn status_logs_health_and_stop_after_a_cpu_deploy() {
    if !have_credentials("status_logs_health_and_stop_after_a_cpu_deploy") {
        return;
    }

    let app_name = unique_app_name("amicu-live-ops");
    let spec = ServerSpec {
        app_name: app_name.clone(),
        gpu: None,
        region: None,
        proxy_regions: vec!["us-east".to_owned()],
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

    // Installed before the deploy, so a panic anywhere below (including
    // inside `deploy_server` itself) still stops the billed container.
    let _guard = AppGuard::install(app_name.clone()).await;

    let credentials = Credentials::from_env().expect("checked above");
    let mut client = ModalClient::connect(credentials)
        .await
        .expect("connect to Modal");
    let deployment = Box::pin(client.deploy_server(&spec, &mut |line| print!("{line}")))
        .await
        .expect("deploy the CPU server");
    println!("deployed {} at {}", deployment.app_id, deployment.url);

    // Wait for the container so status/logs/health below have something to
    // observe rather than racing the start.
    let deadline =
        std::time::Instant::now() + Duration::from_secs(spec.startup_timeout_secs.into());
    loop {
        if matches!(
            ModalClient::health(&deployment.url, "/", Duration::from_secs(10)).await,
            HealthStatus::Ok(200)
        ) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "server never came up");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let status = client
        .app_status(&app_name)
        .await
        .expect("app_status RPCs")
        .expect("just deployed");
    assert_eq!(status.app_id, deployment.app_id);
    assert_eq!(status.state, "deployed");
    assert_eq!(status.functions.len(), 1, "{:?}", status.functions);
    assert!(
        status.functions[0].url.is_some(),
        "{:?}",
        status.functions[0]
    );

    let mut lines = Vec::new();
    client
        .stream_logs(&deployment.app_id, None, false, |line| {
            lines.push(line);
            std::ops::ControlFlow::Continue(())
        })
        .await
        .expect("stream_logs");
    assert!(!lines.is_empty(), "expected at least one log line");

    client.stop_app(&deployment.app_id).await.expect("stop_app");
    let after_stop = client.app_status(&app_name).await.expect("app_status RPCs");
    assert!(after_stop.is_none(), "{after_stop:?}");
}
