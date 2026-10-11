//! Live integration test for `ModalClient::probe_app_hardware`
//! (`TaskList` + `ContainerExec` + `ContainerExecGetOutput`). `#[ignore]`d,
//! and **skips** (not fails) without `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET`.
//!
//! ```sh
//! MODAL_TOKEN_ID=ak-... MODAL_TOKEN_SECRET=as-... \
//!     cargo test -p amicu-modal --test live_hardware -- --ignored --nocapture
//! ```
//!
//! Cheap (CPU, cents): a plain `python -m http.server` container, so no
//! accelerator is expected. What is checked is the exec transport end to end
//! and that the probe's container-memory section parses (a CPU container has
//! cgroup or `/proc/meminfo` memory and must report no accelerator).
//! Accelerator parsing is covered by the fixture unit tests in
//! `amicu-telemetry`; running this against a GPU function is the check for a
//! vendor's live output. An [`common::AppGuard`] stops the app however the
//! test ends.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use amicu_modal::{Credentials, ImageSpec, ServerSpec};

mod common;
use common::AppGuard;

const PORT: u16 = 8000;

fn unique_app_name(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    let suffix = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or_default();
    format!("{prefix}-{suffix:x}")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MODAL_TOKEN_*, network access, and a few cents"]
async fn probe_a_cpu_container_over_container_exec() {
    let test = "probe_a_cpu_container_over_container_exec";
    if let Err(missing) = Credentials::from_env() {
        println!("skipping {test}: {missing}");
        return;
    }

    let app_name = unique_app_name("amicu-live-hw");
    let spec = ServerSpec {
        app_name: app_name.clone(),
        gpu: None,
        region: None,
        proxy_regions: vec!["us-east".to_owned()],
        port: PORT,
        ready_path: None,
        argv: ["python", "-m", "http.server", &PORT.to_string()]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        image: ImageSpec {
            base_registry_image: "python:3.12-slim".to_owned(),
            pip_packages: Vec::new(),
            setup_commands: Vec::new(),
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

    let _guard = AppGuard::install(app_name.clone()).await;
    let mut client = common::connect().await;
    let deployment = Box::pin(client.deploy_server(&spec, &mut |line| print!("{line}")))
        .await
        .expect("deploy the CPU server");
    println!("deployed {} at {}", deployment.app_id, deployment.url);

    // The container may take a moment to appear in TaskList.
    let deadline = Instant::now() + Duration::from_secs(spec.startup_timeout_secs.into());
    let containers = loop {
        let found = client
            .probe_app_hardware(&deployment.app_id)
            .await
            .expect("list the app's containers");
        if !found.is_empty() {
            break found;
        }
        assert!(Instant::now() < deadline, "no container ever appeared");
        tokio::time::sleep(Duration::from_secs(5)).await;
    };

    for container in &containers {
        println!("{}: {:?}", container.task_id, container.outcome);
    }
    let sample = containers[0]
        .outcome
        .as_ref()
        .expect("the probe ran in the container");
    assert!(sample.accelerators.is_empty(), "a CPU container has no GPU");
    assert!(
        sample.host.mem_used_bytes.is_some(),
        "cgroup or /proc/meminfo memory should be readable"
    );
}
