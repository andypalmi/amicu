//! `amicu runpod status` / `logs` / `metrics` / `stop` - operate a RunPod
//! Serverless deployment recorded in this project's state.
//!
//! Each command finds the deployment's RunPod stop record (endpoint id) in
//! state; management calls use `RUNPOD_API_KEY`, the `/metrics` scrape the
//! deployment's own `api_key` through the gateway. `stop` is `destroy
//! --target <deployment>`: the record is dropped only once the stop is
//! confirmed.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use amicu_core::secret::SecretValue;
use amicu_runpod::logs::{DEFAULT_TAIL, NO_WORKERS, WORKER_LOG_LIMITS, merge};
use amicu_runpod::{Endpoint, RunPodError, RunPodSupervisor, WorkersList, endpoint_url};
use amicu_runtime::{EndpointExposure, GatewayAuth, RuntimeHandle};
use amicu_telemetry::scrape::MetricsSnapshot;
use serde_json::json;

use crate::OutputFormat;
use crate::commands::apply::Env;
use crate::commands::build_runtime;
use crate::error::CliError;
use crate::project::Project;
use crate::runpod_ops::{API_KEY_ENV_VAR, runpod_error};
use crate::stops::{Backend, StopRecord};

fn io_err(error: io::Error) -> CliError {
    CliError::io("write failed", error)
}

/// What hardware sections say: there is no probe into a RunPod worker.
const NO_HARDWARE: &str = "not available on RunPod";

/// The deployment's RunPod record and endpoint id, from state.
fn recorded_endpoint(
    project: &Project,
    deployment: &str,
) -> Result<(StopRecord, String), CliError> {
    let state = project.load_state()?;
    let record = state
        .iter()
        .flat_map(|s| s.stops.iter())
        .find(|r| r.backend == Backend::Runpod && r.deployment.as_str() == deployment)
        .cloned()
        .ok_or_else(|| {
            CliError::Message(format!(
                "no RunPod deployment named '{deployment}' in this project's state"
            ))
        })?;
    let id = record.app_id.clone().ok_or_else(|| {
        CliError::Message(format!(
            "deployment '{deployment}' has no RunPod endpoint id recorded yet (its start did \
             not finish); run `amicu apply` or `amicu destroy --target {deployment}`"
        ))
    })?;
    Ok((record, id))
}

fn endpoint_gone(deployment: &str, id: &str) -> CliError {
    CliError::Message(format!(
        "RunPod endpoint {id} of deployment '{deployment}' was not found in the account \
         {API_KEY_ENV_VAR} acts in"
    ))
}

/// Endpoint id, workers summary, URL, GPU and data centers.
pub(crate) fn status(
    project: &Project,
    deployment: &str,
    output: OutputFormat,
    out: &mut dyn Write,
    env: &Env<'_>,
) -> Result<u8, CliError> {
    let (_, id) = recorded_endpoint(project, deployment)?;
    let client = env.runpod.client()?;
    let rt = build_runtime()?;
    let (endpoint, workers) = rt.block_on(async {
        let endpoint = match client.get_endpoint(&id).await {
            Ok(e) => e,
            Err(RunPodError::NotFound { .. }) => return Err(endpoint_gone(deployment, &id)),
            Err(e) => return Err(runpod_error("could not read the RunPod endpoint", &e)),
        };
        let workers = client
            .list_workers(&id)
            .await
            .map_err(|e| runpod_error("could not list the RunPod workers", &e))?;
        Ok((endpoint, workers))
    })?;
    match output {
        OutputFormat::Human => write_status(out, deployment, &endpoint, &workers)?,
        OutputFormat::Json => {
            writeln!(out, "{}", status_json(deployment, &endpoint, &workers)).map_err(io_err)?;
        }
    }
    Ok(0)
}

fn gpu_text(endpoint: &Endpoint) -> String {
    endpoint.gpu.as_ref().map_or_else(
        || "n/a".to_owned(),
        |g| {
            let pools = if g.pools.is_empty() {
                "n/a".to_owned()
            } else {
                g.pools.join(", ")
            };
            match g.count {
                Some(n) => format!("{pools} x{n}"),
                None => pools,
            }
        },
    )
}

fn write_status(
    out: &mut dyn Write,
    deployment: &str,
    endpoint: &Endpoint,
    workers: &WorkersList,
) -> Result<(), CliError> {
    let s = &workers.summary;
    writeln!(
        out,
        "deployment {deployment}: RunPod endpoint {} ({})",
        endpoint.id, endpoint.name
    )
    .map_err(io_err)?;
    writeln!(out, "  url:          {}", endpoint_url(&endpoint.id)).map_err(io_err)?;
    let limits = endpoint
        .workers
        .as_ref()
        .map_or_else(String::new, |w| format!(" (min {}, max {})", w.min, w.max));
    writeln!(
        out,
        "  workers:      running={} idle={} initializing={} throttled={} unhealthy={}{limits}",
        s.running, s.idle, s.initializing, s.throttled, s.unhealthy
    )
    .map_err(io_err)?;
    if s.running + s.idle + s.initializing + s.throttled + s.unhealthy == 0 {
        writeln!(
            out,
            "                scaled to zero; the next request starts a worker"
        )
        .map_err(io_err)?;
    }
    writeln!(out, "  gpu:          {}", gpu_text(endpoint)).map_err(io_err)?;
    let dcs = if endpoint.data_center_ids.is_empty() {
        "any (RunPod chooses)".to_owned()
    } else {
        endpoint.data_center_ids.join(", ")
    };
    writeln!(out, "  data centers: {dcs}").map_err(io_err)
}

fn status_json(deployment: &str, endpoint: &Endpoint, workers: &WorkersList) -> serde_json::Value {
    let s = &workers.summary;
    json!({
        "deployment": deployment,
        "endpoint_id": endpoint.id,
        "endpoint_name": endpoint.name,
        "url": endpoint_url(&endpoint.id),
        "workers": {
            "running": s.running,
            "idle": s.idle,
            "initializing": s.initializing,
            "throttled": s.throttled,
            "unhealthy": s.unhealthy,
            "min": endpoint.workers.as_ref().map(|w| w.min),
            "max": endpoint.workers.as_ref().map(|w| w.max),
        },
        "gpu": endpoint.gpu.as_ref().map(|g| json!({ "pools": g.pools, "count": g.count })),
        "data_centers": endpoint.data_center_ids,
    })
}

/// Every worker's recent lines (`tail` per worker), `[<worker>]`-prefixed.
/// A bounded read: RunPod's stream is left after a short quiet spell.
pub(crate) fn logs(
    project: &Project,
    deployment: &str,
    tail: Option<u32>,
    output: OutputFormat,
    out: &mut dyn Write,
    env: &Env<'_>,
) -> Result<u8, CliError> {
    let (_, id) = recorded_endpoint(project, deployment)?;
    let client = env.runpod.client()?;
    let rt = build_runtime()?;
    let workers = rt.block_on(async {
        match client
            .endpoint_logs(&id, tail.unwrap_or(DEFAULT_TAIL), WORKER_LOG_LIMITS)
            .await
        {
            Ok(w) => Ok(w),
            Err(RunPodError::NotFound { .. }) => Err(endpoint_gone(deployment, &id)),
            Err(e) => Err(runpod_error("could not read the RunPod worker logs", &e)),
        }
    })?;
    if output == OutputFormat::Json {
        let doc = json!({
            "deployment": deployment,
            "endpoint_id": id,
            "workers": workers.iter().map(|w| json!({
                "id": w.worker_id,
                "status": w.status,
                "error": w.lines.as_ref().err(),
                "lines": w.lines.as_ref().map_or(&[][..], Vec::as_slice).iter().map(|l| json!({
                    "ts": l.ts, "source": l.source, "line": l.line,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        writeln!(out, "{doc}").map_err(io_err)?;
        return Ok(0);
    }
    if workers.is_empty() {
        writeln!(out, "{NO_WORKERS}").map_err(io_err)?;
        return Ok(0);
    }
    for line in merge(&workers) {
        writeln!(out, "{line}").map_err(io_err)?;
    }
    Ok(0)
}

/// The deployment's `api_key` (recorded in the state graph), resolved like
/// `apply` does: it is the bearer RunPod's gateway accepts.
fn gateway_key(
    project: &Project,
    deployment: &str,
    env: &Env<'_>,
) -> Result<SecretValue, CliError> {
    let state = project.load_state()?;
    let key_ref = state
        .as_ref()
        .and_then(|s| {
            s.graph
                .deployments
                .iter()
                .find(|d| d.id.as_str() == deployment)
        })
        .and_then(|d| d.api_key.clone())
        .ok_or_else(|| {
            CliError::Message(format!(
                "deployment '{deployment}' has no api_key in state; RunPod metrics are read \
                 through the gateway with it"
            ))
        })?;
    build_runtime()?
        .block_on(env.resolver.resolve(&key_ref))
        .map_err(|e| {
            CliError::Message(format!(
                "could not resolve the api_key of deployment '{deployment}' ({e})"
            ))
        })
}

/// How `metrics` scrapes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MetricsMode {
    /// Keep scraping every `interval_secs`.
    pub watch: bool,
    pub interval_secs: u64,
    /// Scrape even when no worker is up (starts a billed one).
    pub wake: bool,
}

/// A runtime-metrics snapshot (or one every `interval_secs` with `watch`)
/// scraped through the gateway; the load balancer picks the worker. A
/// scaled-to-zero endpoint is refused unless `wake`.
pub(crate) fn metrics(
    project: &Project,
    deployment: &str,
    mode: MetricsMode,
    output: OutputFormat,
    out: &mut dyn Write,
    env: &Env<'_>,
) -> Result<u8, CliError> {
    let MetricsMode {
        watch,
        interval_secs,
        wake,
    } = mode;
    let (_, id) = recorded_endpoint(project, deployment)?;
    let key = gateway_key(project, deployment, env)?;
    let supervisor = RunPodSupervisor::new(env.runpod.client()?, "", deployment)
        .with_probe_transport(env.runpod.probe())
        .with_gateway_key(key);
    let handle = RuntimeHandle {
        id: id.clone(),
        deployment: deployment.into(),
        endpoint: endpoint_url(&id),
        exposure: EndpointExposure::Public {
            gateway: GatewayAuth::RunPodBearer,
        },
    };
    let rt = build_runtime()?;
    let mut previous: Option<(MetricsSnapshot, Instant)> = None;
    loop {
        let snapshot = rt
            .block_on(supervisor.scrape_metrics(&handle, wake || previous.is_some()))
            .map_err(|e| CliError::Message(e.to_string()))?;
        let now = Instant::now();
        let snapshot = match &previous {
            Some((before, at)) => snapshot.rate_since(before, now.duration_since(*at)),
            None => snapshot,
        };
        match output {
            OutputFormat::Human => {
                crate::commands::modal::write_metrics(out, deployment, &snapshot, watch)?;
                writeln!(out, "hardware: n/a ({NO_HARDWARE})").map_err(io_err)?;
            }
            OutputFormat::Json => {
                let doc = json!({
                    "deployment": deployment,
                    "endpoint_id": id,
                    "runtime": crate::commands::modal::runtime_json(&snapshot),
                    "hardware": { "error": NO_HARDWARE, "containers": [] },
                });
                writeln!(out, "{doc}").map_err(io_err)?;
            }
        }
        out.flush().map_err(io_err)?;
        if !watch {
            return Ok(0);
        }
        previous = Some((snapshot, now));
        std::thread::sleep(Duration::from_secs(interval_secs));
    }
}

/// Stops the endpoint and deletes its secrets through `destroy`'s path, so
/// the deployment leaves state only once the stop is confirmed.
pub(crate) fn stop(
    project: &Project,
    deployment: &str,
    lock_timeout: Option<Duration>,
    output: OutputFormat,
    out: &mut dyn Write,
    env: &Env<'_>,
) -> Result<u8, CliError> {
    let state = project.load_state()?;
    let is_runpod = state
        .iter()
        .flat_map(|s| s.stops.iter())
        .any(|r| r.backend == Backend::Runpod && r.deployment.as_str() == deployment);
    if !is_runpod {
        return Err(CliError::Message(format!(
            "no RunPod deployment named '{deployment}' in this project's state"
        )));
    }
    crate::commands::destroy::run_with(
        project,
        &[deployment.to_owned()],
        true,
        lock_timeout,
        output,
        out,
        &mut io::empty(),
        env,
    )
}
