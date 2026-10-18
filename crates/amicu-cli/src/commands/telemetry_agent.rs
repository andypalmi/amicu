//! `amicu telemetry-agent` (hidden): the opt-in telemetry sidecar.
//!
//! When, and only when, telemetry consent resolves to on at `apply` time,
//! every host deployment (launchd/systemd) gets a second, secret-free job
//! next to its server: this agent. It
//!
//! 1. scrapes the server's own loopback `/metrics` once a minute (llama.cpp
//!    serves it because amicu always passes `--metrics`; vLLM by default). It
//!    never proxies or touches a request, so the inference path is unaffected;
//! 2. folds the scrapes into 5-minute rollups ([`amicu_telemetry::rollup`]);
//! 3. queues each rollup in a bounded, private file under the user's data dir
//!    ([`amicu_telemetry::queue`]);
//! 4. uploads the queue about once an hour, with jitter, to the ingestion
//!    endpoint over HTTPS, backing off on failure ([`amicu_telemetry::upload`]).
//!
//! Consent is re-read from the user's telemetry file before every scrape and
//! every upload: once it resolves to off, the agent stops collecting, deletes
//! its queue, and sends nothing, without a redeploy. The agent never sees the
//! user's shell, so `DO_NOT_TRACK` / `AMICU_TELEMETRY` are enforced by the CLI:
//! every `apply` (a no-op one too) and `amicu telemetry disable` stop every
//! agent on the host when consent resolves to off
//! (`apply::stop_agents_if_opted_out`).
//!
//! Modal deployments get no agent: the container image does not carry the
//! amicu binary, and the CLI does not run continuously beside them.

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use amicu_core::graph::Graph;
use amicu_core::id::DeploymentId;
use amicu_core::resource::{Model, Runtime, SourceRegistry};
use amicu_llamacpp::{Catalog, ServiceSpec, SidecarPlanner, sidecar_label};
use amicu_runtime::LaunchPlan;
use amicu_telemetry::queue::{DiskQueue, QueueLimits, ensure_private_dir};
use amicu_telemetry::rollup::{ClosedWindow, Scrape, ScrapeRuntime, WindowAggregator};
use amicu_telemetry::upload::{Attempt, Backoff, Delivery, Transport, Uploader};
use amicu_telemetry::{
    AnonDeploymentId, HardwareInfo, Identity, IdentityStore, InstallId, Label, ModelInfo,
    RuntimeInfo, StoreError, TELEMETRY_SCHEMA_VERSION, TelemetryReport,
};
use serde::{Deserialize, Serialize};

use crate::commands::telemetry::{ConfigStore, effective};
use crate::error::CliError;
use crate::planner::PlanResult;

/// Where batches go unless `AMICU_TELEMETRY_ENDPOINT` says otherwise.
pub(crate) const DEFAULT_ENDPOINT: &str = "https://api.amicu.dev/v1/telemetry/rollups";

/// Overrides [`DEFAULT_ENDPOINT`] at apply time (must be `https://`).
const ENDPOINT_ENV: &str = "AMICU_TELEMETRY_ENDPOINT";

const SCRAPE_EVERY: Duration = Duration::from_secs(60);
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(5);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// Uploads happen every 45 to 75 minutes (hourly on average).
const UPLOAD_EVERY_MIN_S: f64 = 2700.0;
const UPLOAD_JITTER_S: f64 = 1800.0;
const BACKOFF_BASE: Duration = Duration::from_secs(300);
const BACKOFF_MAX: Duration = Duration::from_secs(6 * 3600);
const MAX_METRICS_BYTES: usize = 4 * 1024 * 1024;

/// Hugging Face publishers whose repos are public, so their repo id is sent
/// as is. Any other model (local file, URL, Ollama, other repos, which may be
/// private) is sent as `custom`.
const PUBLIC_PUBLISHERS: &[&str] = &[
    "meta-llama",
    "qwen",
    "mistralai",
    "google",
    "microsoft",
    "deepseek-ai",
    "openai",
    "nvidia",
    "ibm-granite",
    "huggingfacetb",
    "allenai",
    "tiiuae",
    "coherelabs",
    "cohereforai",
    "thudm",
    "zai-org",
    "moonshotai",
    "unsloth",
    "bartowski",
    "thebloke",
    "ggml-org",
    "lmstudio-community",
];

/// The model label sent for `model`: its public repo id, or `custom`.
pub(crate) fn public_model_name(model: &Model) -> String {
    let reference = model.source.reference.as_str();
    let public = model.source.registry == SourceRegistry::HuggingFace
        && reference.split_once('/').is_some_and(|(org, repo)| {
            !repo.is_empty()
                && !repo.contains('/')
                && PUBLIC_PUBLISHERS.contains(&org.to_ascii_lowercase().as_str())
        });
    if public && Label::new(reference).is_ok() {
        reference.to_owned()
    } else {
        "custom".to_owned()
    }
}

/// The user-level telemetry data dir: `$XDG_DATA_HOME/amicu/telemetry`, else
/// `~/.local/share/amicu/telemetry`.
pub(crate) fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/share"))
        })?;
    Some(base.join("amicu").join("telemetry"))
}

/// The directory holding every agent's queue file.
pub(crate) fn queue_dir(data: &Path) -> PathBuf {
    data.join("queue")
}

// ---- identity file ----------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct IdentityFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    install_id: Option<InstallId>,
    /// Local only: instance label -> anonymous deployment id.
    #[serde(default)]
    deployments: HashMap<String, AnonDeploymentId>,
}

/// [`IdentityStore`] over `<data>/identity.json` (0600). The label-to-id map
/// never leaves the machine.
pub(crate) struct FileIdentity {
    path: PathBuf,
}

impl FileIdentity {
    pub(crate) fn new(data: &Path) -> Self {
        Self {
            path: data.join("identity.json"),
        }
    }

    fn read(&self) -> Result<IdentityFile, StoreError> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => serde_json::from_str(&t).map_err(|e| StoreError(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IdentityFile::default()),
            Err(e) => Err(StoreError(e.to_string())),
        }
    }

    /// Read-modify-write under `identity.lock`, so two CLI runs cannot lose
    /// each other's ids. Only the CLI writes this file; agents only read it.
    fn update(&self, change: impl FnOnce(&mut IdentityFile)) -> Result<(), StoreError> {
        let _lock = FileLock::acquire(&self.path.with_extension("lock"))
            .map_err(|e| StoreError(e.to_string()))?;
        let mut file = self.read()?;
        change(&mut file);
        let text = serde_json::to_string_pretty(&file).map_err(|e| StoreError(e.to_string()))?;
        write_private(&self.path, text.as_bytes()).map_err(|e| StoreError(e.to_string()))
    }

    /// Forget `main_label`'s anonymous id and delete its queue file. Returns
    /// the id that was forgotten.
    pub(crate) fn forget(&self, main_label: &str) -> Option<AnonDeploymentId> {
        let id = self.read().ok()?.deployments.get(main_label).copied()?;
        let data = self.path.parent()?;
        let _ = std::fs::remove_file(queue_dir(data).join(format!("{id}.jsonl")));
        self.update(|f| {
            f.deployments.remove(main_label);
        })
        .ok()?;
        Some(id)
    }
}

impl IdentityStore for FileIdentity {
    fn load_install_id(&self) -> Result<Option<InstallId>, StoreError> {
        Ok(self.read()?.install_id)
    }
    fn save_install_id(&self, id: &InstallId) -> Result<(), StoreError> {
        // Under the lock, an id another run wrote first wins.
        self.update(|f| {
            f.install_id.get_or_insert(*id);
        })
    }
    fn load_deployment_id(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Option<AnonDeploymentId>, StoreError> {
        Ok(self.read()?.deployments.get(deployment.as_str()).copied())
    }
    fn save_deployment_id(
        &self,
        deployment: &DeploymentId,
        id: &AnonDeploymentId,
    ) -> Result<(), StoreError> {
        self.update(|f| {
            f.deployments
                .entry(deployment.as_str().to_owned())
                .or_insert(*id);
        })
    }
    fn clear_deployment_ids(&self) -> Result<(), StoreError> {
        self.update(|f| f.deployments.clear())
    }
}

/// An exclusive lock file, removed on drop. A lock older than a minute is
/// left over from a crash and is taken over.
struct FileLock(PathBuf);

impl FileLock {
    fn acquire(path: &Path) -> std::io::Result<Self> {
        ensure_private_dir(path.parent().unwrap_or(Path::new(".")))?;
        for _ in 0..100 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => return Ok(Self(path.to_path_buf())),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(60));
                    if stale {
                        let _ = std::fs::remove_file(path);
                    } else {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("{} is held by another amicu", path.display()),
        ))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write `bytes` to `path` via a uniquely named 0600 temp file renamed into
/// place, in a 0700 directory.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or(Path::new("."));
    ensure_private_dir(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = opts
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

// ---- the apply-side planner -------------------------------------------------

/// What the agent is told about one deployment, decided at apply time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Descriptor {
    model: String,
    quantization: Option<String>,
    provider: String,
}

/// Renders the agent's job for each opted-in host deployment.
pub(crate) struct TelemetrySidecar {
    exe: PathBuf,
    endpoint: String,
    config: PathBuf,
    data: PathBuf,
    descriptors: HashMap<DeploymentId, Descriptor>,
}

/// The agent's argv for one instance. Every value is local configuration;
/// none of it is sent anywhere except as the report fields it becomes.
fn agent_argv(
    s: &TelemetrySidecar,
    d: &Descriptor,
    plan: &LaunchPlan,
    anon: AnonDeploymentId,
) -> Vec<String> {
    let (runtime, port, context, batch) = match &plan.settings {
        Runtime::LlamaCpp(x) => ("llama.cpp", x.port, x.context_length, x.batch_size),
        Runtime::Vllm(x) => ("vllm", x.port, x.context_length, x.max_num_seqs),
    };
    let mut argv = vec![
        s.exe.to_string_lossy().into_owned(),
        "telemetry-agent".to_owned(),
        "--metrics-url".to_owned(),
        format!("http://127.0.0.1:{port}/metrics"),
        "--deployment-id".to_owned(),
        anon.to_uuid_string(),
        "--runtime".to_owned(),
        runtime.to_owned(),
        "--model".to_owned(),
        d.model.clone(),
        "--provider".to_owned(),
        d.provider.clone(),
        "--context-length".to_owned(),
        context.to_string(),
        "--endpoint".to_owned(),
        s.endpoint.clone(),
        "--config".to_owned(),
        s.config.to_string_lossy().into_owned(),
        "--data-dir".to_owned(),
        s.data.to_string_lossy().into_owned(),
    ];
    if let Some(q) = &d.quantization {
        argv.extend(["--quantization".to_owned(), q.clone()]);
    }
    if let Some(b) = batch {
        argv.extend(["--batch-size".to_owned(), b.to_string()]);
    }
    argv
}

impl SidecarPlanner for TelemetrySidecar {
    fn sidecar(&self, plan: &LaunchPlan, main_label: &str) -> Option<ServiceSpec> {
        let d = self.descriptors.get(&plan.deployment)?;
        let anon = Identity::new(FileIdentity::new(&self.data))
            .deployment_id(&DeploymentId::new(main_label))
            .ok()?;
        Some(ServiceSpec {
            label: sidecar_label(main_label),
            argv: agent_argv(self, d, plan, anon),
            env: Vec::new(),
        })
    }
}

impl TelemetrySidecar {
    fn new(
        exe: PathBuf,
        endpoint: String,
        config: PathBuf,
        data: PathBuf,
        graph: &Graph,
        catalog: &dyn Catalog,
        result: &PlanResult,
    ) -> Self {
        let mut descriptors = HashMap::new();
        for item in &result.items {
            let Some(d) = graph.deployments.iter().find(|d| d.id == item.deployment) else {
                continue;
            };
            let model = catalog.resolve_model(&d.model);
            descriptors.insert(
                item.deployment.clone(),
                Descriptor {
                    model: model
                        .as_ref()
                        .map_or_else(|| "custom".to_owned(), |m| public_model_name(&m.model)),
                    quantization: model
                        .and_then(|m| m.model.artifact.quantization)
                        .and_then(|q| serde_json::to_value(q).ok())
                        .and_then(|v| v.as_str().map(str::to_owned)),
                    provider: item
                        .action
                        .provider
                        .clone()
                        .unwrap_or_else(|| "local".to_owned()),
                },
            );
        }
        Self {
            exe,
            endpoint,
            config,
            data,
            descriptors,
        }
    }
}

/// The endpoint to configure: [`ENDPOINT_ENV`] if set (HTTPS only), else the
/// default.
fn endpoint_from(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    match env(ENDPOINT_ENV).filter(|v| !v.is_empty()) {
        Some(url) if url.starts_with("https://") => Some(url),
        Some(_) => None,
        None => Some(DEFAULT_ENDPOINT.to_owned()),
    }
}

/// A stable path to this binary for the service definition: the `PATH` entry
/// that resolves to it (e.g. Homebrew's `bin/amicu` symlink, which survives
/// upgrades) when there is one, else the executable itself.
fn stable_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let real = std::fs::canonicalize(&exe).ok()?;
    let on_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("amicu"))
            .find(|p| p.is_absolute() && std::fs::canonicalize(p).ok().as_ref() == Some(&real))
    });
    Some(on_path.unwrap_or(real))
}

/// What `apply` and `destroy` need from telemetry. A trait so tests never
/// read the real consent file or touch the real data dir.
pub(crate) trait TelemetryHooks: Sync {
    /// Whether consent resolves to on in this process (file and env).
    fn consent_on(&self) -> bool;
    /// The sidecar planner for this run; `None` unless consent is on.
    fn planner(
        &self,
        graph: &Graph,
        catalog: &dyn Catalog,
        result: &PlanResult,
    ) -> Option<Arc<dyn SidecarPlanner>>;
    /// Delete every agent's queue.
    fn clear_queues(&self);
    /// An instance (and so its agent) is confirmed stopped for good: drop its
    /// anonymous id and its queue.
    fn forget(&self, main_label: &str);
}

/// The real [`TelemetryHooks`]: the user's consent file and data dir.
pub(crate) struct HostTelemetry;

impl TelemetryHooks for HostTelemetry {
    fn consent_on(&self) -> bool {
        ConfigStore::host().is_some_and(|store| consent_on(store.path()))
    }

    fn planner(
        &self,
        graph: &Graph,
        catalog: &dyn Catalog,
        result: &PlanResult,
    ) -> Option<Arc<dyn SidecarPlanner>> {
        let store = ConfigStore::host()?;
        if !consent_on(store.path()) {
            return None;
        }
        let endpoint = endpoint_from(&|k: &str| std::env::var(k).ok())?;
        let data = data_dir()?;
        // The install id is created here, by the CLI, under the identity
        // lock; agents only ever read it.
        Identity::new(FileIdentity::new(&data)).install_id().ok()?;
        Some(Arc::new(TelemetrySidecar::new(
            stable_exe()?,
            endpoint,
            store.path().to_path_buf(),
            data,
            graph,
            catalog,
            result,
        )))
    }

    fn clear_queues(&self) {
        if let Some(data) = data_dir() {
            let _ = std::fs::remove_dir_all(queue_dir(&data));
        }
    }

    fn forget(&self, main_label: &str) {
        if let Some(data) = data_dir() {
            FileIdentity::new(&data).forget(main_label);
        }
    }
}

// ---- the agent --------------------------------------------------------------

/// `amicu telemetry-agent` arguments, written by apply into the sidecar job.
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct AgentArgs {
    /// The server's loopback metrics URL (`http://127.0.0.1:<port>/metrics`).
    #[arg(long)]
    metrics_url: String,
    /// The deployment's anonymous id.
    #[arg(long)]
    deployment_id: String,
    /// `llama.cpp` or `vllm`.
    #[arg(long)]
    runtime: String,
    /// Public model id or `custom`.
    #[arg(long)]
    model: String,
    #[arg(long)]
    quantization: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    context_length: Option<u32>,
    #[arg(long)]
    batch_size: Option<u32>,
    /// Ingestion endpoint (`https://` only).
    #[arg(long)]
    endpoint: String,
    /// The user's telemetry consent file.
    #[arg(long)]
    config: PathBuf,
    /// The user's telemetry data dir (identity and queue).
    #[arg(long)]
    data_dir: PathBuf,
}

/// The fixed part of every report this agent writes.
struct Static {
    deployment_id: AnonDeploymentId,
    model: ModelInfo,
    runtime: RuntimeInfo,
    provider: Option<Label>,
    context_length: Option<u32>,
    batch_size: Option<u32>,
}

impl Static {
    fn from_args(a: &AgentArgs) -> Result<Self, CliError> {
        let bad = |what: &str| CliError::Message(format!("telemetry-agent: invalid {what}"));
        let deployment_id: AnonDeploymentId =
            serde_json::from_value(serde_json::Value::String(a.deployment_id.clone()))
                .map_err(|_| bad("--deployment-id"))?;
        if !is_loopback_metrics_url(&a.metrics_url) {
            return Err(bad("--metrics-url (must be loopback)"));
        }
        if !a.endpoint.starts_with("https://") {
            return Err(bad("--endpoint (must be https)"));
        }
        let runtime = Label::new(a.runtime.as_str()).map_err(|_| bad("--runtime"))?;
        let model = Label::sanitized(&a.model)
            .unwrap_or_else(|| Label::new("custom").expect("valid label"));
        let mut model = ModelInfo::new(model);
        if let Some(q) = &a.quantization {
            model = model.with_quantization(q);
        }
        Ok(Self {
            deployment_id,
            model,
            runtime: RuntimeInfo::new(runtime),
            // One of the fixed provider values, anything else `other`.
            provider: a
                .provider
                .as_deref()
                .and_then(TelemetryReport::provider_label),
            context_length: a.context_length,
            batch_size: a.batch_size,
        })
    }

    fn report(
        &self,
        install_id: InstallId,
        hardware: &HardwareInfo,
        closed: ClosedWindow,
    ) -> TelemetryReport {
        TelemetryReport {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            install_id,
            deployment_id: self.deployment_id,
            window: closed.window,
            amicu_version: Label::sanitized_version(env!("CARGO_PKG_VERSION"))
                .unwrap_or_else(|| Label::new("unknown").expect("valid label")),
            hardware: hardware.clone(),
            model: self.model.clone(),
            runtime: self.runtime.clone(),
            provider: self.provider.clone(),
            region: None,
            context_length: self.context_length,
            batch_size: self.batch_size,
            metrics: closed.metrics,
        }
    }
}

/// `http://127.0.0.1:<port>/metrics` or `http://[::1]:<port>/metrics`,
/// parsed, with no userinfo, query or fragment.
fn is_loopback_metrics_url(raw: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
    url.scheme() == "http"
        && loopback
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_some()
        && url.path() == "/metrics"
        && url.query().is_none()
        && url.fragment().is_none()
}

/// A short accelerator model label: `NVIDIA GeForce RTX 4090` becomes
/// `nvidia-geforce-rtx-4090`.
fn accelerator_label(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(64).collect()
}

fn gpu_label(name: &str, vram_bytes: Option<u64>) -> Option<String> {
    let memory = vram_bytes.map(amicu_core::ByteSize::from_bytes);
    amicu_core::resource::resolve_gpu(name, memory)
        .ok()
        .map(|card| card.id.to_owned())
}

/// Static hardware facts from one probe: model, count, per-device memory and
/// CPU cores. No serials, bus ids or host names exist in the probe output.
fn hardware_info(sample: &amicu_telemetry::hardware::HardwareSample) -> HardwareInfo {
    let accs = &sample.accelerators;
    let count = u32::try_from(accs.len()).unwrap_or(u32::MAX);
    let mut info = HardwareInfo {
        vram_bytes: accs.iter().filter_map(|a| a.mem_total_bytes).max(),
        cpu_cores: std::thread::available_parallelism()
            .ok()
            .and_then(|n| u32::try_from(n.get()).ok()),
        memory_bytes: sample.host.mem_limit_bytes,
        ..HardwareInfo::default()
    };
    if let Some(name) = accs.iter().find_map(|a| a.name.as_deref()) {
        let label = gpu_label(name, info.vram_bytes).unwrap_or_else(|| accelerator_label(name));
        info = info.with_accelerator(&label, count);
    } else {
        info.accelerator_count = count;
    }
    info.coarsened()
}

/// The HTTPS uploader.
pub(crate) struct HttpTransport {
    client: reqwest::Client,
    endpoint: String,
}

impl HttpTransport {
    /// A client for `endpoint`, which must be `https://` (plain `http://` is
    /// accepted only for a loopback address, which tests use).
    pub(crate) fn new(endpoint: &str) -> Result<Self, CliError> {
        let loopback = endpoint.starts_with("http://127.0.0.1:");
        if !endpoint.starts_with("https://") && !loopback {
            return Err(CliError::Message(
                "telemetry endpoint must be https".to_owned(),
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("amicu/", env!("CARGO_PKG_VERSION")))
            .timeout(UPLOAD_TIMEOUT)
            .https_only(!loopback)
            .build()
            .map_err(|e| CliError::Message(format!("telemetry client: {e}")))?;
        Ok(Self {
            client,
            endpoint: endpoint.to_owned(),
        })
    }
}

impl Transport for HttpTransport {
    async fn post(&self, body: String, idempotency_key: String) -> Result<u16, String> {
        self.client
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("idempotency-key", idempotency_key)
            .body(body)
            .send()
            .await
            .map(|r| r.status().as_u16())
            .map_err(|e| {
                if e.is_timeout() {
                    "timed out"
                } else {
                    "request failed"
                }
                .to_owned()
            })
    }
}

/// `GET` the loopback metrics text, bounded in time and size.
async fn scrape(client: &reqwest::Client, url: &str) -> Option<(ScrapeRuntime, Scrape)> {
    let mut response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_METRICS_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Scrape::from_text(std::str::from_utf8(&body).ok()?)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A uniform random number in `[0, 1)` from the OS random source.
#[allow(clippy::cast_precision_loss)]
fn jitter() -> f64 {
    (uuid::Uuid::new_v4().as_u128() >> 75) as f64 / (1u64 << 53) as f64
}

fn consent_on(config: &Path) -> bool {
    let env = |k: &str| std::env::var(k).ok();
    // An unreadable file counts as not opted in.
    effective(&env, ConfigStore::new(config).load().ok().flatten()).enabled
}

/// `amicu telemetry-agent ...`: runs until the service manager stops it.
pub(crate) fn run(args: &AgentArgs) -> Result<u8, CliError> {
    let fixed = Static::from_args(args)?;
    let transport = HttpTransport::new(&args.endpoint)?;
    let runtime = crate::commands::build_runtime()?;
    runtime.block_on(agent_loop(args, &fixed, &transport))
}

async fn agent_loop(
    args: &AgentArgs,
    fixed: &Static,
    transport: &HttpTransport,
) -> Result<u8, CliError> {
    let scraper = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(SCRAPE_TIMEOUT)
        .build()
        .map_err(|e| CliError::Message(format!("telemetry scrape client: {e}")))?;
    let queue = DiskQueue::new(
        queue_dir(&args.data_dir).join(format!("{}.jsonl", fixed.deployment_id)),
        QueueLimits::DEFAULT,
    );
    // Read-only: apply created the install id.
    let identity = FileIdentity::new(&args.data_dir);
    let mut aggregator = WindowAggregator::new();
    let mut uploader = Uploader::new();
    let mut backoff = Backoff::new(BACKOFF_BASE, BACKOFF_MAX);
    let mut hardware = HardwareInfo::default();
    // The first upload lands at a random point in the first hour, so agents
    // started together (a reboot) do not upload together.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mut next_upload = unix_now() + (jitter() * 3600.0) as u64;
    loop {
        let now = unix_now();
        if consent_on(&args.config) {
            let metrics = scrape(&scraper, &args.metrics_url).await.map(|(_, s)| s);
            let sample = tokio::task::spawn_blocking(|| {
                amicu_telemetry::hardware::probe_local(amicu_telemetry::hardware::PROBE_TIMEOUT)
                    .ok()
            })
            .await
            .ok()
            .flatten();
            if let Some(s) = &sample
                && hardware == HardwareInfo::default()
            {
                hardware = hardware_info(s);
            }
            if let Some(closed) = aggregator.observe(now, metrics, sample.as_ref())
                && let Ok(Some(install_id)) = identity.load_install_id()
            {
                let report = fixed.report(install_id, &hardware, closed);
                if report.validate().is_ok() {
                    let _ = queue.push(report, now);
                }
            }
            if now >= next_upload {
                next_upload = upload(&mut uploader, &mut backoff, &queue, transport, args).await;
            }
        } else {
            // Opted out since apply: collect nothing, keep nothing, send nothing.
            let _ = queue.clear();
            aggregator = WindowAggregator::new();
        }
        tokio::time::sleep(SCRAPE_EVERY).await;
    }
}

/// Upload what is queued (consent re-checked first); returns when to try next.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
async fn upload(
    uploader: &mut Uploader,
    backoff: &mut Backoff,
    queue: &DiskQueue,
    transport: &HttpTransport,
    args: &AgentArgs,
) -> u64 {
    let hourly = || unix_now() + (UPLOAD_EVERY_MIN_S + jitter() * UPLOAD_JITTER_S) as u64;
    // Drain in batches while the server accepts; stop at the first failure.
    loop {
        if !consent_on(&args.config) {
            return hourly();
        }
        match uploader.upload_once(queue, transport, unix_now()).await {
            Ok(Attempt::Sent(Delivery::Accepted | Delivery::Rejected, _)) => backoff.reset(),
            Ok(Attempt::Empty) => return hourly(),
            Ok(Attempt::Kept | Attempt::Sent(Delivery::Retry, _)) | Err(_) => {
                return unix_now() + backoff.fail(jitter()).as_secs();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::resource::{Artifact, ArtifactFormat, ModelSource};
    use amicu_telemetry::rollup::WINDOW_SECONDS;
    use amicu_telemetry::{Rollup, Window};

    fn model(registry: SourceRegistry, reference: &str) -> Model {
        Model {
            id: "m".into(),
            source: ModelSource {
                registry,
                reference: reference.to_owned(),
            },
            architecture: "llama".into(),
            modalities: vec![],
            artifact: Artifact {
                format: ArtifactFormat::Gguf,
                quantization: None,
            },
            context_length: None,
            variants: vec![],
            n_layers: None,
            n_kv_heads: None,
            head_dim: None,
            n_kv_layers: None,
        }
    }

    #[test]
    fn only_well_known_public_models_are_named() {
        use SourceRegistry::{HuggingFace, Local, Ollama, Url};
        assert_eq!(
            public_model_name(&model(HuggingFace, "meta-llama/Llama-3.1-8B-Instruct")),
            "meta-llama/Llama-3.1-8B-Instruct"
        );
        assert_eq!(
            public_model_name(&model(HuggingFace, "Qwen/Qwen3.5-9B")),
            "Qwen/Qwen3.5-9B"
        );
        for (registry, reference) in [
            (HuggingFace, "my-company/secret-finetune"),
            (HuggingFace, "andrea/private-model"),
            (Local, "/Users/me/models/x.gguf"),
            (Url, "https://example.com/m.gguf"),
            (Ollama, "llama3"),
            (HuggingFace, "meta-llama"),
        ] {
            assert_eq!(public_model_name(&model(registry, reference)), "custom");
        }
    }

    #[test]
    fn catalog_gpus_are_labelled_with_their_canonical_id() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(
            gpu_label("NVIDIA H100 80GB HBM3", Some(81_559 * MIB)).as_deref(),
            Some("h100-sxm-80gb")
        );
        assert_eq!(
            gpu_label("NVIDIA A100 80GB PCIe", Some(81_920 * MIB)).as_deref(),
            Some("a100-pcie-80gb")
        );
        assert_eq!(
            gpu_label("NVIDIA L4", Some(23_034 * MIB)).as_deref(),
            Some("l4-24gb")
        );
        assert_eq!(gpu_label("Apple M3 Max", None), None);
        assert_eq!(gpu_label("NVIDIA Quadro P400", Some(2048 * MIB)), None);
        let label = Label::new(gpu_label("NVIDIA H100 NVL", None).unwrap());
        assert!(label.is_ok());
    }

    #[test]
    fn accelerator_names_become_short_labels() {
        assert_eq!(
            accelerator_label("NVIDIA GeForce RTX 4090"),
            "nvidia-geforce-rtx-4090"
        );
        assert_eq!(accelerator_label(" Apple M3 Max "), "apple-m3-max");
        assert!(Label::new(accelerator_label("NVIDIA H100 80GB HBM3")).is_ok());
    }

    #[test]
    fn endpoint_defaults_and_refuses_plain_http() {
        assert_eq!(endpoint_from(&|_| None).as_deref(), Some(DEFAULT_ENDPOINT));
        let custom = |_: &str| Some("https://t.example/v1".to_owned());
        assert_eq!(
            endpoint_from(&custom).as_deref(),
            Some("https://t.example/v1")
        );
        assert_eq!(
            endpoint_from(&|_| Some("http://t.example".to_owned())),
            None
        );
        assert!(HttpTransport::new("http://t.example").is_err());
        assert!(HttpTransport::new(DEFAULT_ENDPOINT).is_ok());
    }

    fn args() -> AgentArgs {
        AgentArgs {
            metrics_url: "http://127.0.0.1:8080/metrics".into(),
            deployment_id: AnonDeploymentId::generate().to_uuid_string(),
            runtime: "llama.cpp".into(),
            model: "custom".into(),
            quantization: Some("q4_k_m".into()),
            provider: Some("local".into()),
            context_length: Some(8192),
            batch_size: None,
            endpoint: DEFAULT_ENDPOINT.into(),
            config: "/nonexistent/telemetry.json".into(),
            data_dir: "/nonexistent".into(),
        }
    }

    #[test]
    fn agent_arguments_are_validated() {
        assert!(Static::from_args(&args()).is_ok());
        for bad in [
            "http://10.0.0.5:8080/metrics",
            "http://127.0.0.1:8080@evil.example/metrics",
            "http://user:pw@127.0.0.1:8080/metrics",
            "http://127.0.0.1.evil.example:8080/metrics",
            "https://127.0.0.1:8080/metrics",
            "http://127.0.0.1:8080/other",
            "http://localhost:8080/metrics",
        ] {
            let mut a = args();
            a.metrics_url = bad.into();
            assert!(Static::from_args(&a).is_err(), "{bad}");
        }
        assert!(is_loopback_metrics_url("http://[::1]:8080/metrics"));
        let mut a = args();
        a.endpoint = "http://api.amicu.dev".into();
        assert!(Static::from_args(&a).is_err());
        let mut a = args();
        a.deployment_id = "chat_prod".into();
        assert!(Static::from_args(&a).is_err(), "only a random uuid");
        let mut a = args();
        a.provider = Some("hetzner".into());
        assert_eq!(
            Static::from_args(&a).unwrap().provider.unwrap().as_str(),
            "other"
        );
        let mut a = args();
        a.model = "has spaces".into();
        assert_eq!(Static::from_args(&a).unwrap().model.name.as_str(), "custom");
    }

    #[test]
    fn a_report_built_by_the_agent_validates() {
        let fixed = Static::from_args(&args()).unwrap();
        let closed = ClosedWindow {
            window: Window {
                start_unix_s: 1_750_000_200,
                duration_s: WINDOW_SECONDS,
            },
            metrics: Rollup {
                output_tokens: 100,
                ..Rollup::default()
            },
        };
        let r = fixed.report(InstallId::generate(), &HardwareInfo::default(), closed);
        r.validate().expect("valid");
        assert_eq!(r.runtime.name.as_str(), "llama.cpp");
        assert_eq!(r.model.quantization.unwrap().as_str(), "q4_k_m");
    }

    #[test]
    fn no_consent_means_off_and_the_file_decides_at_send_time() {
        let dir = std::env::temp_dir().join(format!("amicu-agent-consent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::new(dir.join("telemetry.json"));
        // Not asked: off. This test process has neither override set unless
        // the developer's shell does, in which case it still must not be on
        // without the file.
        if std::env::var_os("AMICU_TELEMETRY").is_none() {
            assert!(!consent_on(store.path()));
            store.save(true).unwrap();
            assert_eq!(
                consent_on(store.path()),
                std::env::var_os("DO_NOT_TRACK").is_none()
            );
            store.save(false).unwrap();
            assert!(!consent_on(store.path()), "disable stops sending");
        }
        std::fs::write(store.path(), "{garbage").unwrap();
        assert!(!consent_on(store.path()), "unreadable counts as off");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_identity_file_is_private_and_stable() {
        let dir = std::env::temp_dir().join(format!("amicu-agent-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ident = Identity::new(FileIdentity::new(&dir));
        let a = ident.install_id().unwrap();
        assert_eq!(
            a,
            Identity::new(FileIdentity::new(&dir)).install_id().unwrap()
        );
        let label = DeploymentId::new("io.amicu.llama.a1b2c3d4.chat");
        let d = ident.deployment_id(&label).unwrap();
        assert_eq!(d, ident.deployment_id(&label).unwrap());
        let text = std::fs::read_to_string(dir.join("identity.json")).unwrap();
        assert!(text.contains(&d.to_uuid_string()));
        // Forgetting drops the entry and the queue file.
        let q = queue_dir(&dir).join(format!("{d}.jsonl"));
        std::fs::create_dir_all(q.parent().unwrap()).unwrap();
        std::fs::write(&q, "{}\n").unwrap();
        assert_eq!(FileIdentity::new(&dir).forget(label.as_str()), Some(d));
        assert!(!q.exists());
        assert_eq!(
            FileIdentity::new(&dir).load_deployment_id(&label).unwrap(),
            None
        );
        assert_eq!(FileIdentity::new(&dir).load_install_id().unwrap(), Some(a));
        // Concurrent writers under the lock never lose an id.
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    Identity::new(FileIdentity::new(&dir))
                        .deployment_id(&DeploymentId::new(format!("io.amicu.llama.x.d{i}")))
                        .unwrap()
                })
            })
            .collect();
        let ids: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        for (i, id) in ids.iter().enumerate() {
            let got = FileIdentity::new(&dir)
                .load_deployment_id(&DeploymentId::new(format!("io.amicu.llama.x.d{i}")))
                .unwrap();
            assert_eq!(got, Some(*id));
        }
        assert!(!dir.join("identity.lock").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("identity.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    fn sidecar() -> TelemetrySidecar {
        let mut descriptors = HashMap::new();
        descriptors.insert(
            DeploymentId::new("chat"),
            Descriptor {
                model: "Qwen/Qwen3.5-9B".into(),
                quantization: Some("q4_k_m".into()),
                provider: "local".into(),
            },
        );
        TelemetrySidecar {
            exe: "/opt/homebrew/bin/amicu".into(),
            endpoint: DEFAULT_ENDPOINT.into(),
            config: "/home/u/.config/amicu/telemetry.json".into(),
            data: "/home/u/.local/share/amicu/telemetry".into(),
            descriptors,
        }
    }

    #[derive(clap::Parser)]
    struct P {
        #[command(flatten)]
        a: AgentArgs,
    }

    #[test]
    fn the_agent_job_is_secret_free_and_parses_back() {
        let plan = LaunchPlan {
            deployment: DeploymentId::new("chat"),
            runtime: amicu_runtime::RuntimeKind::LlamaCpp,
            rendered: amicu_runtime::RenderedLaunch::Command {
                argv: vec!["llama-server".into()],
                text: "llama-server".into(),
            },
            settings: Runtime::LlamaCpp(amicu_core::resource::LlamaCppSettings {
                context_length: 8192,
                port: 8091,
                batch_size: Some(512),
                ..Default::default()
            }),
            secret_refs: vec![],
            api_key: None,
            service_label: None,
            estimate: amicu_runtime::ResourceEstimate {
                estimated_memory: amicu_core::ByteSize::from_bytes(0),
                gpu_count: 0,
                kv_cache: None,
            },
        };
        let s = sidecar();
        let anon = AnonDeploymentId::generate();
        let argv = agent_argv(&s, &s.descriptors[&plan.deployment], &plan, anon);
        assert!(argv.contains(&"http://127.0.0.1:8091/metrics".to_owned()));
        assert!(
            !argv.iter().any(|a| a.contains("chat")),
            "no deployment name"
        );
        let parsed = <P as clap::Parser>::try_parse_from(&argv[1..]).unwrap().a;
        let fixed = Static::from_args(&parsed).unwrap();
        assert_eq!(fixed.deployment_id, anon);
        assert_eq!(fixed.batch_size, Some(512));
        // Unknown deployments (e.g. Modal ones) get no agent.
        let mut other = plan.clone();
        other.deployment = DeploymentId::new("other");
        assert!(s.sidecar(&other, "io.amicu.llama.x.other").is_none());
    }

    /// One-shot HTTP server on loopback answering `status`, returning the URL.
    type Seen = Arc<std::sync::Mutex<Vec<String>>>;

    /// A loopback HTTP server answering `statuses` in order; returns its URL
    /// and the raw requests it received.
    async fn serve(statuses: Vec<&'static str>) -> (String, Seen) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Seen = Arc::default();
        let log = requests.clone();
        tokio::spawn(async move {
            for status in statuses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 256 * 1024];
                let mut seen = Vec::new();
                // Read headers and the declared body.
                loop {
                    let n = sock.read(&mut buf).await.unwrap();
                    seen.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&seen);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if seen.len() >= end + 4 + len {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&seen).into_owned());
                let reply =
                    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                sock.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{addr}/v1/telemetry/rollups"), requests)
    }

    #[tokio::test]
    async fn a_fake_endpoint_202_drains_and_503_keeps() {
        let dir = std::env::temp_dir().join(format!("amicu-agent-http-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let queue = DiskQueue::new(dir.join("q.jsonl"), QueueLimits::DEFAULT);
        let fixed = Static::from_args(&args()).unwrap();
        for i in 0..3 {
            let closed = ClosedWindow {
                window: Window {
                    start_unix_s: 1_750_000_200 + i * 300,
                    duration_s: WINDOW_SECONDS,
                },
                metrics: Rollup {
                    output_tokens: 5,
                    ..Rollup::default()
                },
            };
            let r = fixed.report(InstallId::generate(), &HardwareInfo::default(), closed);
            queue.push(r, 1_750_000_200).unwrap();
        }
        let (url, requests) = serve(vec!["503 Service Unavailable", "202 Accepted"]).await;
        let transport = HttpTransport::new(&url).unwrap();
        let mut up = Uploader::new();
        assert_eq!(
            up.upload_once(&queue, &transport, 1_750_000_200)
                .await
                .unwrap(),
            Attempt::Kept
        );
        assert_eq!(queue.load().unwrap().len(), 3, "503 keeps the queue");
        assert_eq!(
            up.upload_once(&queue, &transport, 1_750_000_200)
                .await
                .unwrap(),
            Attempt::Sent(Delivery::Accepted, 3)
        );
        assert!(queue.load().unwrap().is_empty(), "202 drains it");
        // The request shape the ingestion service expects.
        let sent = requests.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        for req in &sent {
            let lower = req.to_ascii_lowercase();
            assert!(
                req.starts_with("POST /v1/telemetry/rollups HTTP/1.1"),
                "{req}"
            );
            assert!(lower.contains("content-type: application/json"));
            assert!(lower.contains(&format!("user-agent: amicu/{}", env!("CARGO_PKG_VERSION"))));
            assert!(!lower.contains("authorization"), "no token header yet");
        }
        let key = |r: &str| {
            r.lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("idempotency-key: ")
                        .map(str::to_owned)
                })
                .unwrap()
        };
        assert_eq!(key(&sent[0]), key(&sent[1]), "a retry reuses the key");
        let body = &sent[1][sent[1].find("\r\n\r\n").unwrap() + 4..];
        let batch = amicu_telemetry::TelemetryBatch::from_json(body).unwrap();
        assert_eq!(batch.batch_id.to_uuid_string(), key(&sent[1]).trim());
        // Nothing listening: a network error keeps the data too.
        queue
            .push(
                fixed.report(
                    InstallId::generate(),
                    &HardwareInfo::default(),
                    ClosedWindow {
                        window: Window {
                            start_unix_s: 1_750_000_200,
                            duration_s: WINDOW_SECONDS,
                        },
                        metrics: Rollup {
                            output_tokens: 1,
                            ..Rollup::default()
                        },
                    },
                ),
                1_750_000_200,
            )
            .unwrap();
        let dead = HttpTransport::new("http://127.0.0.1:9/v1/telemetry/rollups").unwrap();
        assert_eq!(
            up.upload_once(&queue, &dead, 1_750_000_200).await.unwrap(),
            Attempt::Kept
        );
        assert_eq!(queue.load().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
