//! [`RunPodSupervisor`]: runs a rendered vLLM / llama.cpp launch as a RunPod
//! Serverless load-balancing endpoint (`https://<id>.api.runpod.ai`), using
//! the stock image and the shared container rewrite
//! ([`amicu_llamacpp::remote::launch_argv`]). The load balancer routes to and
//! health-checks the deployment's own port.
//!
//! Auth is the gateway only ([`GatewayAuth::RunPodBearer`]): RunPod checks
//! `Authorization: Bearer <RunPod API key>` and strips the header before the
//! worker, so the deployment's `api_key` must be a RunPod key with
//! api.runpod.ai access, is used only to probe readiness, and the runtime gets
//! no `--api-key` (it would reject every request).
//!
//! Every other secret becomes a RunPod account secret referenced as
//! `{{ RUNPOD_SECRET_<name> }}`, so the stored endpoint config holds no value.
//! Names are content-addressed ([`secret_name`]): a changed value is a new
//! name, and the previous generation is deleted only after the endpoint
//! update succeeds. [`RunPodSupervisor::planned_secret_names`] gives the names
//! before `start` for write-ahead recording; `start` and `stop` only delete
//! names they recorded or wrote. A pre-existing secret with a computed name is
//! trusted to hold its value.
//!
//! `start` never blind-retries a create: it prefers the recorded endpoint id,
//! otherwise adopts a single same-named endpoint only when it carries this
//! deployment's [`OWNER_ENV_VAR`] marker, and resolves an ambiguous create by
//! that same lookup. The live GPU catalog narrows `excludedTypes` to the
//! requested card before anything is written.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use amicu_core::naming::{project_app_name, project_endpoint_name};
use amicu_core::resource::{
    Accelerator, HardwareProfile, Model, Region, Residency, Runtime, ScalingSettings,
};
use amicu_core::secret::SecretValue;
use amicu_llamacpp::remote::{
    self, GgufSource, HF_TOKEN_ENV_VAR, LLAMACPP_IMAGE_SERVER_PATH, RemoteEnv,
};
use amicu_llamacpp::{LlamaCppError, ProcessSupervisor, ResolvedSecret};
use amicu_providers::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport};
use amicu_runtime::{
    EndpointExposure, GatewayAuth, LaunchPlan, LogOptions, LogStream, RuntimeHandle, RuntimeKind,
    RuntimeMetrics, RuntimeStatus,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::client::{
    CreateEndpoint, CreateSecret, DeleteOutcome, Endpoint, FlashBoot, GpuRequest, GpuType,
    NetworkVolume, OWNER_ENV_VAR, RunPodClient, UpdateEndpoint, WorkerSummary, WorkersConfig,
};
use crate::error::RunPodError;
use crate::hardware::{exclusions_from_catalog, gpu_placement, verify_catalog_memory};
use crate::logs::{TIMEOUT_LOG_LIMITS, TIMEOUT_TAIL, merge, tail_block};
use crate::regions::{place_on_volume, restrict_to_residencies, select_data_centers};
use crate::secrets::{SecretLedger, reference_secrets, stop_result};

/// The provider name the shared remote-launch renderer puts in its errors.
pub const PROVIDER: &str = "RunPod";

/// Operator environment variable overriding the vLLM image.
pub const VLLM_IMAGE_ENV_VAR: &str = "AMICU_RUNPOD_VLLM_IMAGE";

/// Operator environment variable overriding the llama.cpp image.
pub const LLAMACPP_IMAGE_ENV_VAR: &str = "AMICU_RUNPOD_LLAMACPP_IMAGE";

/// The health route `start` waits on through the gateway (and the pod
/// proxy). Both llama-server and vLLM serve it on their own port,
/// unauthenticated by the runtime (the RunPod gateway still requires the
/// bearer key).
pub const HEALTH_PATH: &str = "/health";

/// The route RunPod's load balancer really polls before it routes to a
/// worker. Live tests show it polls only `GET /ping`, whatever
/// `HEALTH_CHECK_PATH` says: vLLM serves `/ping`, llama-server is given it
/// through its Vertex AI mode ([`llamacpp_ping_env`]).
///
/// A worker whose `/ping` is not `200` gets `RUNPOD_INIT_TIMEOUT` seconds
/// (RunPod's default is 7 minutes) to become healthy before it is restarted;
/// amicu raises it to [`DEFAULT_INIT_TIMEOUT_SECS`].
pub const LB_PING_PATH: &str = "/ping";

/// Env variable that sets how long RunPod waits for a worker to answer
/// `/ping` with `200` before restarting it.
pub const INIT_TIMEOUT_ENV: &str = "RUNPOD_INIT_TIMEOUT";

/// Default for [`INIT_TIMEOUT_ENV`] on serverless endpoints: 30 minutes, for
/// large models that download and load slowly.
pub const DEFAULT_INIT_TIMEOUT_SECS: u32 = 1800;

/// The env that makes llama-server serve its `/health` handler at
/// [`LB_PING_PATH`] too: its Vertex AI compatibility mode (`AIP_MODE`) adds
/// `AIP_HEALTH_ROUTE` as a second health route, and would move the server to
/// `AIP_HTTP_PORT` (default 8080), so that is pinned to `port`.
#[must_use]
pub fn llamacpp_ping_env(port: u16) -> [(&'static str, String); 3] {
    [
        ("AIP_MODE", "PREDICTION".to_owned()),
        ("AIP_HEALTH_ROUTE", LB_PING_PATH.to_owned()),
        ("AIP_HTTP_PORT", port.to_string()),
    ]
}

/// How often the CLI has `start` report progress while it waits for
/// readiness.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(30);

/// Receives one readiness progress line (see
/// [`RunPodSupervisor::with_progress`]).
pub(crate) type ProgressSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Idle seconds before a worker scales down, unless the deployment's
/// `scaling { scaledown_window }` says otherwise (same default as Modal).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u32 = 900;

/// RunPod's ceiling for `workers.idleTimeout`.
pub const MAX_IDLE_TIMEOUT_SECS: u32 = 3600;

/// Workers kept warm unless overridden: scale to zero.
pub const DEFAULT_MIN_WORKERS: u32 = 0;

/// Worker ceiling unless `scaling { max_replicas }` says otherwise: one GPU
/// at most, so cost stays bounded until the user raises it. RunPod needs a
/// finite bound (Modal's default is unlimited).
pub const DEFAULT_MAX_WORKERS: u32 = 1;

/// In-flight requests per worker before RunPod scales out, unless
/// `scaling { target_concurrency }` says otherwise (same default as Modal).
pub const DEFAULT_REQUEST_COUNT: u32 = 32;

/// Container disk per worker, in GB. Without a network volume the model
/// weights download onto this disk at each cold start.
pub const DEFAULT_CONTAINER_DISK_GB: u32 = 100;

/// `HF_HOME` on a mounted network volume: vLLM and llama.cpp both cache
/// Hugging Face downloads under `<HF_HOME>/hub`.
pub const VOLUME_HF_HOME: &str = "/runpod-volume/huggingface";

/// Points the Hugging Face cache at the network volume, unless the
/// deployment set `HF_HOME` itself. llama.cpp (b11515) reads `LLAMA_CACHE`
/// before `HF_HOME`, so it is pinned to the same hub directory unless set.
pub(crate) fn insert_volume_cache_env(
    env: &mut BTreeMap<String, String>,
    secret: &BTreeMap<String, String>,
    runtime: RuntimeKind,
) {
    let user_set = |k: &str| env.contains_key(k) || secret.contains_key(k);
    if user_set("HF_HOME") {
        return;
    }
    if runtime == RuntimeKind::LlamaCpp && !user_set("LLAMA_CACHE") {
        env.insert("LLAMA_CACHE".to_owned(), format!("{VOLUME_HF_HOME}/hub"));
    }
    env.insert("HF_HOME".to_owned(), VOLUME_HF_HOME.to_owned());
}

/// A warning when the volume's data center lists no stock of GPU
/// `type_id` right now. RunPod's per-data-center list only carries types in
/// stock (live: L4 left EU-RO-1's list within minutes), so this cannot tell
/// "never offered" from "sold out" and is never a refusal.
pub(crate) async fn volume_gpu_note(
    client: &RunPodClient,
    volume: Option<&(String, String)>,
    type_id: &str,
) -> Option<String> {
    let (id, dc) = volume?;
    let listed = client.data_center_gpus(dc).await.ok()?;
    (!listed.iter().any(|g| g == type_id)).then(|| {
        format!(
            "RunPod lists no `{type_id}` in stock in {dc}, where network volume `{id}` lives; \
             workers wait for capacity there"
        )
    })
}

/// Env variables the load balancer (or amicu's owner check) reads; the
/// deployment's `env` block and secrets may not set them, nor any
/// [`RESERVED_ENV_PREFIX`] one.
const RESERVED_ENV: [&str; 4] = ["PORT", "PORT_HEALTH", "HEALTH_CHECK_PATH", OWNER_ENV_VAR];

/// llama-server's Vertex AI mode variables: amicu sets some, and an
/// `AIP_PREDICT_ROUTE` that collides with a route makes llama-server exit.
pub(crate) const RESERVED_ENV_PREFIX: &str = "AIP_";

/// The first variable of `plain` or `secret` that is in `names` or starts
/// with [`RESERVED_ENV_PREFIX`].
pub(crate) fn reserved_env_var<'a>(
    names: &[&str],
    plain: &'a BTreeMap<String, String>,
    secret: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    plain
        .keys()
        .chain(secret.keys())
        .map(String::as_str)
        .find(|k| names.contains(k) || k.starts_with(RESERVED_ENV_PREFIX))
}

/// Hex digits of the value digest in a secret name (48 bits).
const SECRET_DIGEST_HEX: usize = 12;

/// The public base URL of endpoint `id`.
#[must_use]
pub fn endpoint_url(id: &str) -> String {
    format!("https://{id}.api.runpod.ai")
}

/// The first `hex_len` hex digits of SHA-256 over `domain` and the
/// length-prefixed `parts` (so part boundaries can never shift).
fn sha256_hex(domain: &str, parts: &[&str], hex_len: usize) -> String {
    let mut h = Sha256::new();
    h.update(domain.as_bytes());
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p.as_bytes());
    }
    let mut hex = String::with_capacity(64);
    for b in h.finalize() {
        let _ = write!(hex, "{b:02x}");
    }
    hex.truncate(hex_len);
    hex
}

/// The owner marker amicu sets as [`OWNER_ENV_VAR`] on a deployment's
/// endpoint: a digest of the workspace (empty when unknown), project and
/// deployment ids. Not a secret; it lets `start` tell its own endpoint from
/// another one that happens to carry the same name.
#[must_use]
pub fn owner_marker(workspace_id: Option<&str>, project_id: &str, deployment_id: &str) -> String {
    sha256_hex(
        "amicu-runpod-owner/v1",
        &[workspace_id.unwrap_or(""), project_id, deployment_id],
        32,
    )
}

/// The RunPod secret holding `value` for env variable `env_key` of a
/// deployment, from the same [`amicu_core::naming`] scheme as the endpoint
/// name (`amicu-<project>-<deployment>-<env key>-<digest>-<hash>`, at most 63
/// characters of `[a-z0-9-]`; the readable part may be shortened). `<digest>`
/// is a truncated SHA-256 of the value salted with `owner` (the
/// [`owner_marker`]) and `env_key`: equal values give equal names, a changed
/// value gives a new name, and the name does not reveal a high-entropy value.
#[must_use]
pub fn secret_name(
    project_id: &str,
    deployment_id: &str,
    owner: &str,
    env_key: &str,
    value: &str,
) -> String {
    let digest = sha256_hex(
        "amicu-runpod-secret/v1",
        &[owner, env_key, value],
        SECRET_DIGEST_HEX,
    );
    project_app_name(project_id, &format!("{deployment_id}/{env_key}/{digest}"))
}

/// The endpoint-env template RunPod replaces with secret `name`'s value when
/// it starts a worker.
#[must_use]
pub fn secret_reference(name: &str) -> String {
    format!("{{{{ RUNPOD_SECRET_{name} }}}}")
}

/// Polling bounds for `start`'s readiness wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadinessTiming {
    /// Give up after this long (cold start includes downloading the model).
    pub timeout: Duration,
    /// First pause between health polls.
    pub initial_backoff: Duration,
    /// Longest pause between health polls (the backoff doubles up to it).
    pub max_backoff: Duration,
}

impl ReadinessTiming {
    /// Twenty minutes overall, polling every 2 s backing off to 15 s.
    pub const DEFAULT: Self = Self {
        timeout: Duration::from_mins(20),
        initial_backoff: Duration::from_secs(2),
        max_backoff: Duration::from_secs(15),
    };

    /// [`DEFAULT`](Self::DEFAULT) with a wait that outlasts
    /// [`DEFAULT_INIT_TIMEOUT_SECS`], so `start` never gives up on a worker
    /// RunPod still lets load.
    pub const SERVERLESS: Self = Self {
        timeout: Duration::from_mins(35),
        ..Self::DEFAULT
    };
}

impl Default for ReadinessTiming {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How `stop` retries an endpoint delete RunPod refuses because workers are
/// still live (after `stop` has scaled them to zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopRetry {
    /// Delete attempts in total (at least one is always made).
    pub attempts: u32,
    /// Pause between attempts.
    pub delay: Duration,
}

impl StopRetry {
    /// Six attempts ten seconds apart (under a minute of waiting).
    pub const DEFAULT: Self = Self {
        attempts: 6,
        delay: Duration::from_secs(10),
    };
}

impl Default for StopRetry {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What [`RunPodSupervisor::stop_endpoint`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// The endpoint was deleted and its secrets with it.
    Deleted,
    /// RunPod reported the endpoint gone (`404`). Its secrets are deleted
    /// only with [`with_confirmed_account`](RunPodSupervisor::with_confirmed_account)
    /// (a `404` from the wrong account must not cost the right one its
    /// secrets); otherwise `kept_secrets` lists the names left in place,
    /// which also stay in [`created_secrets`](RunPodSupervisor::created_secrets).
    AlreadyGone {
        /// Secret names not deleted because the account was not confirmed.
        kept_secrets: Vec<String>,
    },
}

/// Everything `start` sends, built purely from the plan and the resolved
/// secrets. `Debug` never prints a secret value.
struct Prepared {
    endpoint: CreateEndpoint,
    secrets: Vec<CreateSecret>,
    /// The deployment's `api_key` (a RunPod key): only for the readiness
    /// probe through the gateway, never sent to the endpoint.
    api_key: SecretValue,
}

impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prepared")
            .field("endpoint", &self.endpoint)
            .field(
                "secrets",
                &self.secrets.iter().map(|s| &s.name).collect::<Vec<_>>(),
            )
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// Starts, inspects and stops one deployment's RunPod Serverless endpoint.
/// See the module docs for the mapping.
///
/// One instance targets one deployment (`project_id`/`deployment_id` fix the
/// endpoint name and, with [`with_workspace`](Self::with_workspace), the
/// owner marker and secret names). `status`/`stop` address the endpoint
/// through the [`RuntimeHandle`] alone; `start` additionally needs
/// [`with_accelerator`](Self::with_accelerator).
pub struct RunPodSupervisor {
    client: Arc<RunPodClient>,
    probe: Arc<dyn HttpTransport>,
    workspace_id: Option<String>,
    project_id: String,
    deployment_id: String,
    endpoint_name: String,
    gpu: Option<GpuRequest>,
    /// RunPod GPU type id and nominal VRAM in GB.
    gpu_type: Option<(String, u32)>,
    region: Option<String>,
    data_center_ids: Vec<String>,
    network_volume: Option<String>,
    /// `(volume id, data center)` once the volume was read from RunPod.
    placed_volume: Option<(String, String)>,
    flashboot: bool,
    scaling: ScalingSettings,
    disk_gb: u32,
    gguf: Option<GgufSource>,
    readiness: ReadinessTiming,
    stop_retry: StopRetry,
    confirmed_account: bool,
    recorded_endpoint: Option<String>,
    recorded_secrets: Vec<String>,
    gateway_key: Option<SecretValue>,
    progress: Option<(Duration, ProgressSink)>,
    deployed_endpoint: Arc<Mutex<Option<String>>>,
    created_endpoint: Arc<AtomicBool>,
    secrets: SecretLedger,
}

impl fmt::Debug for RunPodSupervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunPodSupervisor")
            .field("client", &self.client)
            .field("endpoint_name", &self.endpoint_name)
            .field("gpu", &self.gpu)
            .field("data_center_ids", &self.data_center_ids)
            .finish_non_exhaustive()
    }
}

impl RunPodSupervisor {
    /// A supervisor for `deployment_id` in `project_id`, talking to RunPod
    /// through `client` and probing endpoint health over a `reqwest`
    /// transport. The endpoint is named
    /// [`project_endpoint_name`]`(project_id, deployment_id)`.
    #[must_use]
    pub fn new(client: RunPodClient, project_id: &str, deployment_id: &str) -> Self {
        let client = Arc::new(client);
        Self {
            secrets: SecretLedger::new(Arc::clone(&client)),
            client,
            probe: Arc::new(ReqwestTransport::new()),
            workspace_id: None,
            project_id: project_id.to_owned(),
            deployment_id: deployment_id.to_owned(),
            endpoint_name: project_endpoint_name(project_id, deployment_id),
            gpu: None,
            gpu_type: None,
            region: None,
            data_center_ids: Vec::new(),
            network_volume: None,
            placed_volume: None,
            flashboot: false,
            scaling: ScalingSettings::default(),
            disk_gb: DEFAULT_CONTAINER_DISK_GB,
            gguf: None,
            readiness: ReadinessTiming::SERVERLESS,
            stop_retry: StopRetry::DEFAULT,
            confirmed_account: false,
            recorded_endpoint: None,
            recorded_secrets: Vec::new(),
            gateway_key: None,
            progress: None,
            deployed_endpoint: Arc::default(),
            created_endpoint: Arc::default(),
        }
    }

    /// Probe endpoint health through `transport` instead (tests inject a mock).
    #[must_use]
    pub fn with_probe_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.probe = transport;
        self
    }

    /// The workspace the deployment belongs to. It goes into the
    /// [`owner_marker`] (and so into the secret names), so two workspaces
    /// sharing a RunPod account never adopt each other's endpoint. Set it
    /// consistently: a different value makes `start` refuse the existing
    /// endpoint unless its id is [recorded](Self::with_recorded_endpoint).
    #[must_use]
    pub fn with_workspace(mut self, workspace_id: &str) -> Self {
        self.workspace_id = Some(workspace_id.to_owned());
        self
    }

    /// The GPU request from a hardware profile's accelerator. RunPod
    /// Serverless is GPU-only, so `None` leaves `start` failing with a clear
    /// error.
    ///
    /// # Errors
    /// [`LlamaCppError::Supervisor`] if the accelerator is not a GPU or has
    /// no RunPod pool.
    pub fn with_accelerator(
        mut self,
        accelerator: Option<&Accelerator>,
    ) -> Result<Self, LlamaCppError> {
        let placement = accelerator
            .map(gpu_placement)
            .transpose()
            .map_err(|e| runpod_err(&e))?;
        self.gpu_type = placement.as_ref().map(|p| (p.type_id.clone(), p.vram_gb));
        self.gpu = placement.map(GpuRequest::from);
        Ok(self)
    }

    /// Replaces the static `excludedTypes` with every other GPU type the live
    /// catalog lists in the requested pool.
    fn exclude_from_catalog(
        &self,
        gpu: &mut GpuRequest,
        catalog: &[GpuType],
    ) -> Result<(), LlamaCppError> {
        let (Some((type_id, vram_gb)), [pool]) = (&self.gpu_type, gpu.pools.as_slice()) else {
            return Err(LlamaCppError::Supervisor(
                "RunPod GPU placement needs exactly one pool and a known GPU type".to_owned(),
            ));
        };
        verify_catalog_memory(type_id, *vram_gb, catalog).map_err(|e| runpod_err(&e))?;
        gpu.excluded_types =
            exclusions_from_catalog(type_id, pool, catalog).map_err(|e| runpod_err(&e))?;
        Ok(())
    }

    /// Restricts placement to the data centers of `region` (a region group
    /// such as `eu` or an exact data center), enforcing its residency. `None`
    /// lets RunPod choose.
    ///
    /// # Errors
    /// [`LlamaCppError::Supervisor`] for an unknown region or one outside its
    /// declared residency.
    pub fn with_region(mut self, region: Option<&Region>) -> Result<Self, LlamaCppError> {
        self.data_center_ids = match region {
            Some(r) => {
                select_data_centers(&r.code, r.declared_residency()).map_err(|e| runpod_err(&e))?
            }
            None => Vec::new(),
        };
        self.region = region.map(|r| r.code.clone());
        Ok(self)
    }

    /// Holds placement to the `allowed` residencies (the project's
    /// `data_class` allowlist): without a region, only their data centers;
    /// with one, it must lie inside them. Call after
    /// [`with_region`](Self::with_region); empty `allowed` is a no-op.
    ///
    /// # Errors
    /// [`LlamaCppError::Supervisor`] when the region lies outside `allowed`.
    pub fn with_allowed_residencies(
        mut self,
        allowed: &[Residency],
    ) -> Result<Self, LlamaCppError> {
        self.data_center_ids = restrict_to_residencies(
            self.region.as_deref(),
            std::mem::take(&mut self.data_center_ids),
            allowed,
        )
        .map_err(|e| runpod_err(&e))?;
        Ok(self)
    }

    /// Mounts volume `id` as the Hugging Face cache without reading it, so
    /// placement is not narrowed to its data center (for display; `start`
    /// should get [`with_network_volume`](Self::with_network_volume)).
    #[must_use]
    pub fn with_network_volume_id(mut self, id: &str) -> Self {
        self.network_volume = Some(id.to_owned());
        self
    }

    /// Mounts `volume` on every worker as the Hugging Face cache and places
    /// the endpoint in the volume's data center. Call after
    /// [`with_allowed_residencies`](Self::with_allowed_residencies).
    ///
    /// # Errors
    /// [`LlamaCppError::Supervisor`] when the volume lies outside the
    /// deployment's data centers.
    pub fn with_network_volume(mut self, volume: &NetworkVolume) -> Result<Self, LlamaCppError> {
        self.data_center_ids =
            place_on_volume(&self.data_center_ids, volume).map_err(|e| runpod_err(&e))?;
        self.network_volume = Some(volume.id.clone());
        self.placed_volume = Some((volume.id.clone(), volume.data_center.clone()));
        Ok(self)
    }

    /// A warning when the [network volume](Self::with_network_volume)'s data
    /// center has no stock of the requested GPU right now (see
    /// `volume_gpu_note`); `None` otherwise or when it cannot be read.
    pub async fn volume_gpu_note(&self) -> Option<String> {
        let (type_id, _) = self.gpu_type.as_ref()?;
        volume_gpu_note(&self.client, self.placed_volume.as_ref(), type_id).await
    }

    /// FlashBoot on or off (default off).
    #[must_use]
    pub fn with_flashboot(mut self, on: bool) -> Self {
        self.flashboot = on;
        self
    }

    /// Applies a deployment's `scaling {}` overrides; omitted fields keep the
    /// module defaults.
    #[must_use]
    pub fn with_scaling(mut self, scaling: Option<&ScalingSettings>) -> Self {
        self.scaling = scaling.cloned().unwrap_or_default();
        self
    }

    /// The Hugging Face GGUF a llama.cpp launch downloads (see
    /// [`GgufSource::for_model`]). A no-op for vLLM.
    #[must_use]
    pub fn with_gguf_model(mut self, model: &Model) -> Self {
        self.gguf = GgufSource::for_model(model);
        self
    }

    /// Container disk per worker in GB (default [`DEFAULT_CONTAINER_DISK_GB`]).
    #[must_use]
    pub fn with_container_disk_gb(mut self, gb: u32) -> Self {
        self.disk_gb = gb;
        self
    }

    /// Readiness polling bounds (default [`ReadinessTiming::SERVERLESS`]).
    #[must_use]
    pub fn with_readiness_timing(mut self, timing: ReadinessTiming) -> Self {
        self.readiness = timing;
        self
    }

    /// How `stop` retries a delete refused for live workers (default
    /// [`StopRetry::DEFAULT`]).
    #[must_use]
    pub fn with_stop_retry(mut self, retry: StopRetry) -> Self {
        self.stop_retry = retry;
        self
    }

    /// Whether the caller has confirmed these credentials act in the account
    /// recorded in state (e.g. [`RunPodClient::account_identity`] matches the
    /// recorded fingerprint). Only then does a `404` on `stop` count as "gone,
    /// delete its secrets"; without it (the default) the secrets are left and
    /// reported in [`StopOutcome::AlreadyGone`].
    #[must_use]
    pub fn with_confirmed_account(mut self, confirmed: bool) -> Self {
        self.confirmed_account = confirmed;
        self
    }

    /// The endpoint id recorded in state for this deployment. `start` updates
    /// it in place (falling back to the name lookup only when RunPod no
    /// longer has it) instead of relying on a name match.
    #[must_use]
    pub fn with_recorded_endpoint(mut self, id: &str) -> Self {
        self.recorded_endpoint = Some(id.to_owned());
        self
    }

    /// Secret names recorded in state for this deployment: `start` deletes
    /// those the new configuration no longer references (once the endpoint
    /// write succeeded), and `stop` deletes them all.
    #[must_use]
    pub fn with_recorded_secrets(mut self, names: Vec<String>) -> Self {
        self.recorded_secrets = names;
        self
    }

    /// The deployment's `api_key` (a RunPod key), which `metrics` sends as the
    /// gateway bearer.
    #[must_use]
    pub fn with_gateway_key(mut self, key: SecretValue) -> Self {
        self.gateway_key = Some(key);
        self
    }

    /// Where `start` reports readiness progress, once per `every` (the CLI
    /// uses [`PROGRESS_EVERY`]); without a sink it stays silent.
    #[must_use]
    pub fn with_progress(
        mut self,
        every: Duration,
        sink: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        self.progress = Some((every, Arc::new(sink)));
        self
    }

    /// Convenience: a supervisor configured from a resolved hardware profile
    /// (accelerator and region).
    ///
    /// # Errors
    /// See [`with_accelerator`](Self::with_accelerator) and
    /// [`with_region`](Self::with_region).
    pub fn for_hardware(
        client: RunPodClient,
        project_id: &str,
        deployment_id: &str,
        hardware: &HardwareProfile,
    ) -> Result<Self, LlamaCppError> {
        Self::new(client, project_id, deployment_id)
            .with_accelerator(hardware.accelerator.as_ref())?
            .with_region(hardware.region.as_ref())
    }

    /// The endpoint name `start` creates or updates.
    #[must_use]
    pub fn endpoint_name(&self) -> &str {
        &self.endpoint_name
    }

    /// The [`owner_marker`] this deployment's endpoint carries.
    #[must_use]
    pub fn owner(&self) -> String {
        owner_marker(
            self.workspace_id.as_deref(),
            &self.project_id,
            &self.deployment_id,
        )
    }

    /// The management client, e.g. for
    /// [`RunPodClient::account_identity`] before recording state.
    #[must_use]
    pub fn client(&self) -> &RunPodClient {
        &self.client
    }

    /// Holds the endpoint id as soon as RunPod has one for this deployment
    /// (created, adopted after an ambiguous create, or found for an update),
    /// kept even when `start` then fails, so the caller can record it.
    #[must_use]
    pub fn deployed_endpoint(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.deployed_endpoint)
    }

    /// Set once this instance's create (`POST`) produced the endpoint in
    /// [`deployed_endpoint`](Self::deployed_endpoint), including one found by
    /// name after an ambiguous create. Unset for an endpoint adopted by name
    /// or by recorded id.
    #[must_use]
    pub fn created_endpoint(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.created_endpoint)
    }

    /// Names of the RunPod secrets that may exist for this deployment as far
    /// as this instance knows: the recorded ones until they are deleted, and
    /// each one `start` writes (added before its create call). Deletions
    /// remove names, so after `start` or `stop` this is the set to record.
    #[must_use]
    pub fn created_secrets(&self) -> Arc<Mutex<Vec<String>>> {
        self.secrets.shared()
    }

    /// The exact RunPod secret names a `start` with `plan` and `secrets`
    /// references (they depend on the values), for recording before the
    /// call alongside the names already recorded. Reads the operator's
    /// `HF_TOKEN` fallback from the environment, as `start` does.
    ///
    /// # Errors
    /// Whatever `start` refuses before its first call (no `api_key`, a
    /// reserved env variable, no GPU, ...).
    pub fn planned_secret_names(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<Vec<String>, LlamaCppError> {
        let hf_token_fallback = std::env::var(HF_TOKEN_ENV_VAR).ok();
        let prepared = self.prepare(plan, secrets, "", hf_token_fallback.as_deref())?;
        let mut names: Vec<String> = prepared.secrets.into_iter().map(|s| s.name).collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// The argv the RunPod worker runs for `plan` (for display and tests).
    ///
    /// # Errors
    /// See [`amicu_llamacpp::remote::launch_argv`].
    pub fn launch_argv(&self, plan: &LaunchPlan) -> Result<Vec<String>, LlamaCppError> {
        container_argv(plan, self.gguf.as_ref())
    }

    /// The create body `start` would send for `plan`, with placeholder
    /// secret values and before the live GPU catalog refines
    /// `excludedTypes`. For `amicu plan`.
    ///
    /// # Errors
    /// Whatever `start` refuses before its first call.
    pub fn planned_endpoint(&self, plan: &LaunchPlan) -> Result<CreateEndpoint, LlamaCppError> {
        let image = runtime_image(RuntimeKind::of(&plan.settings));
        let hf_token = planned_hf_token();
        self.prepare(plan, &placeholder_secrets(plan), &image, hf_token)
            .map(|p| p.endpoint)
    }

    /// Builds the endpoint request and the secrets for `plan`. Pure: the
    /// image and the `HF_TOKEN` fallback are passed in rather than read from
    /// the environment.
    fn prepare(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
        image: &str,
        hf_token_fallback: Option<&str>,
    ) -> Result<Prepared, LlamaCppError> {
        let runtime = RuntimeKind::of(&plan.settings);
        let port = runtime_port(plan);
        let argv = self.launch_argv(plan)?;
        let RemoteEnv { plain, mut secret } = remote::split_env(plan, secrets, hf_token_fallback)?;

        // The gateway strips `Authorization`, so a runtime key would reject
        // every request: neither key variable may reach the worker.
        let key_vars = [
            RuntimeKind::Vllm.api_key_env_var(),
            RuntimeKind::LlamaCpp.api_key_env_var(),
        ];
        if let Some(k) = key_vars.iter().find(|k| plain.contains_key(**k)) {
            return Err(LlamaCppError::Supervisor(format!(
                "env var '{k}' cannot be set on RunPod: the gateway strips the Authorization \
                 header, so a runtime API key would reject every request; RunPod's gateway \
                 checks the deployment's `api_key` instead"
            )));
        }
        let api_key = secret.remove(runtime.api_key_env_var()).ok_or_else(|| {
            LlamaCppError::Supervisor(
                "RunPod deployments need an `api_key`: a RunPod API key with api.runpod.ai \
                 access (ideally restricted to this endpoint), which RunPod's gateway checks \
                 as the bearer key"
                    .to_owned(),
            )
        })?;
        for k in key_vars {
            secret.remove(k);
        }
        if let Some(k) = reserved_env_var(&RESERVED_ENV, &plain, &secret) {
            return Err(LlamaCppError::Supervisor(format!(
                "env var '{k}' is reserved on RunPod (the load balancer or amicu reads it); \
                 remove it from the deployment's `env` block"
            )));
        }
        let mut gpu = self.gpu.clone().ok_or_else(|| {
            LlamaCppError::Supervisor(
                "RunPod Serverless needs a GPU accelerator in the deployment's hardware profile"
                    .to_owned(),
            )
        })?;
        gpu.min_cuda_version = image_min_cuda(image, runtime);

        let owner = self.owner();
        let mut env = plain;
        env.insert("PORT".to_owned(), port.to_string());
        env.insert("PORT_HEALTH".to_owned(), port.to_string());
        env.insert("HEALTH_CHECK_PATH".to_owned(), HEALTH_PATH.to_owned());
        if runtime == RuntimeKind::LlamaCpp {
            env.extend(llamacpp_ping_env(port).map(|(k, v)| (k.to_owned(), v)));
        }
        env.insert(OWNER_ENV_VAR.to_owned(), owner.clone());
        if !secret.contains_key(INIT_TIMEOUT_ENV) {
            env.entry(INIT_TIMEOUT_ENV.to_owned())
                .or_insert_with(|| DEFAULT_INIT_TIMEOUT_SECS.to_string());
        }
        if self.network_volume.is_some() {
            insert_volume_cache_env(&mut env, &secret, runtime);
        }
        let create_secrets = reference_secrets(
            &self.project_id,
            &self.deployment_id,
            &owner,
            &secret,
            &mut env,
        );
        let (entrypoint, cmd) = container_command(&argv)?;

        let mut endpoint = CreateEndpoint::new(self.endpoint_name.clone(), image, gpu);
        endpoint.entrypoint = entrypoint;
        endpoint.cmd = cmd;
        endpoint.env = env;
        endpoint.ports = vec![format!("{port}/http")];
        endpoint.disk_gb = Some(self.disk_gb);
        endpoint.workers = workers_config(Some(&self.scaling));
        endpoint.request_count = self
            .scaling
            .target_concurrency
            .unwrap_or(DEFAULT_REQUEST_COUNT)
            .max(1);
        endpoint.data_center_ids.clone_from(&self.data_center_ids);
        endpoint.network_volume_ids = self.network_volume.iter().cloned().collect();
        if self.flashboot {
            endpoint.flashboot = FlashBoot::Flashboot;
        }

        Ok(Prepared {
            endpoint,
            secrets: create_secrets,
            api_key: SecretValue::new(api_key),
        })
    }

    fn set_deployed(&self, id: &str) {
        if let Ok(mut slot) = self.deployed_endpoint.lock() {
            *slot = Some(id.to_owned());
        }
    }

    /// Refuses an endpoint unless it carries this deployment's owner marker.
    fn check_owner(&self, endpoint: &Endpoint) -> Result<(), LlamaCppError> {
        let owner = self.owner();
        let why = match endpoint.owner.as_deref() {
            Some(o) if o == owner => return Ok(()),
            Some(_) => format!("its {OWNER_ENV_VAR} marker belongs to another amicu deployment"),
            None => format!("it carries no {OWNER_ENV_VAR} marker for this deployment"),
        };
        Err(LlamaCppError::Supervisor(format!(
            "RunPod endpoint {} is named `{}` but {why}; refusing to adopt it. Delete or \
             rename it, or record its id if it is this deployment's",
            endpoint.id, endpoint.name
        )))
    }

    /// PATCHes endpoint `id` to `prepared`. A refused PATCH removes the
    /// secrets this run created (`fresh`): the endpoint still references the
    /// previous ones.
    async fn patch_endpoint(
        &self,
        id: &str,
        live: &Endpoint,
        prepared: &Prepared,
        fresh: &[String],
    ) -> Result<String, LlamaCppError> {
        self.set_deployed(id);
        match self
            .client
            .update_endpoint(id, &update_from(&prepared.endpoint, live))
            .await
        {
            Ok(_) => Ok(id.to_owned()),
            Err(e) => {
                if !matches!(e, RunPodError::Transport(_)) {
                    self.secrets.discard(fresh).await;
                }
                Err(runpod_err(&e))
            }
        }
    }

    /// Updates the recorded endpoint, else the one owned endpoint carrying
    /// the name, else creates it. Returns the endpoint id. `fresh` are the
    /// secrets this run created.
    async fn upsert_endpoint(
        &self,
        prepared: &Prepared,
        fresh: &[String],
    ) -> Result<String, LlamaCppError> {
        if let Some(id) = &self.recorded_endpoint {
            match self.client.get_endpoint(id).await {
                Ok(ep) => {
                    // Recorded in state, so ours; a foreign marker still stops it.
                    if ep.owner.is_some() {
                        self.check_owner(&ep)?;
                    }
                    return self.patch_endpoint(&ep.id, &ep, prepared, fresh).await;
                }
                Err(RunPodError::NotFound { .. }) => {}
                Err(e) => return Err(runpod_err(&e)),
            }
        }
        let name = &prepared.endpoint.name;
        let existing = self
            .client
            .find_endpoints_by_name(name)
            .await
            .map_err(|e| runpod_err(&e))?;
        match existing.as_slice() {
            [] => {}
            [one] => {
                let ep = self
                    .client
                    .get_endpoint(&one.id)
                    .await
                    .map_err(|e| runpod_err(&e))?;
                self.check_owner(&ep)?;
                return self.patch_endpoint(&ep.id, &ep, prepared, fresh).await;
            }
            many => return Err(duplicate_err(name, many.iter().map(|e| e.id.as_str()))),
        }
        match self.client.create_endpoint(&prepared.endpoint).await {
            Ok(created) => {
                self.set_deployed(&created.id);
                self.created_endpoint.store(true, Ordering::SeqCst);
                Ok(created.id)
            }
            Err(RunPodError::AmbiguousCreate(detail)) => {
                self.recover_ambiguous(name, &detail).await
            }
            Err(e) => {
                // RunPod answered and refused: nothing references the
                // secrets written for it.
                self.secrets.discard(fresh).await;
                Err(runpod_err(&e))
            }
        }
    }

    /// The create's response was lost: the endpoint may exist. Look it up by
    /// name (with the owner check); never send the create again.
    async fn recover_ambiguous(&self, name: &str, detail: &str) -> Result<String, LlamaCppError> {
        let lookup_err = |e: &RunPodError| {
            LlamaCppError::Supervisor(format!(
                "RunPod create outcome unknown ({detail}) and looking up endpoint `{name}` \
                 failed ({e}); look it up by name before retrying"
            ))
        };
        let found = self
            .client
            .find_endpoints_by_name(name)
            .await
            .map_err(|e| lookup_err(&e))?;
        match found.as_slice() {
            [one] => {
                let ep = self
                    .client
                    .get_endpoint(&one.id)
                    .await
                    .map_err(|e| lookup_err(&e))?;
                self.check_owner(&ep)?;
                self.set_deployed(&ep.id);
                self.created_endpoint.store(true, Ordering::SeqCst);
                Ok(ep.id)
            }
            [] => Err(LlamaCppError::Supervisor(format!(
                "RunPod create outcome unknown ({detail}) and no endpoint named `{name}` exists \
                 yet; it was not retried - run the command again to create it"
            ))),
            many => Err(duplicate_err(name, many.iter().map(|e| e.id.as_str()))),
        }
    }

    /// Polls `<endpoint>/health` through the gateway with the bearer key
    /// until a worker answers `200`. `204` (initializing), "no workers"
    /// answers and transport failures are retried with a doubling backoff
    /// until the timeout; `401`/`403` fail at once (the gateway does not
    /// accept the key, waiting will not help). Reports the elapsed time and
    /// worker states to the [progress sink](Self::with_progress), if any.
    async fn wait_ready(
        &self,
        id: &str,
        key: &SecretValue,
        deployment: &str,
    ) -> Result<(), LlamaCppError> {
        let key = key.expose_secret_str().map_err(|_| {
            LlamaCppError::Supervisor("the endpoint API key is not valid UTF-8".to_owned())
        })?;
        let url = format!("{}{HEALTH_PATH}", endpoint_url(id));
        let timing = self.readiness;
        let started = tokio::time::Instant::now();
        let deadline = started + timing.timeout;
        let mut next_progress = started + self.progress.as_ref().map_or(Duration::ZERO, |p| p.0);
        let mut backoff = timing.initial_backoff;
        let mut last = "no answer".to_owned();
        loop {
            let req = HttpRequest::new(HttpMethod::Get, url.clone()).with_bearer(key);
            match self.probe.execute(req).await {
                Ok(r) if r.status == 200 => return Ok(()),
                Ok(r) if matches!(r.status, 401 | 403) => {
                    return Err(LlamaCppError::Supervisor(format!(
                        "RunPod's gateway rejected the deployment's api_key for endpoint {id} \
                         (HTTP {}); on RunPod the api_key must be a RunPod API key with \
                         api.runpod.ai access to this endpoint",
                        r.status
                    )));
                }
                Ok(r) if r.status == 204 => "initializing (HTTP 204)".clone_into(&mut last),
                Ok(r) => last = format!("HTTP {}", r.status),
                Err(e) => last = e.to_string(),
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(LlamaCppError::Supervisor(format!(
                    "RunPod endpoint {id} did not become ready within {}s (last health answer: \
                     {last}; workers: {}); see `amicu runpod logs {deployment}`{}",
                    timing.timeout.as_secs(),
                    self.worker_summary(id).await,
                    self.log_tail(id).await
                )));
            }
            if let Some((every, sink)) = &self.progress
                && now >= next_progress
            {
                next_progress = now + *every;
                sink(&format!(
                    "waiting for RunPod endpoint {id}: {} elapsed, workers: {}",
                    elapsed(now - started),
                    self.worker_summary(id).await
                ));
            }
            tokio::time::sleep(backoff.min(deadline - now)).await;
            backoff = (backoff * 2).min(timing.max_backoff);
        }
    }

    /// The last worker log lines for a timeout message (empty when none can
    /// be read: the error is what matters).
    async fn log_tail(&self, id: &str) -> String {
        match self
            .client
            .endpoint_logs(id, TIMEOUT_TAIL, TIMEOUT_LOG_LIMITS)
            .await
        {
            Ok(workers) => tail_block(&merge(&workers)),
            Err(_) => String::new(),
        }
    }

    /// The endpoint's worker states for a progress or timeout message.
    async fn worker_summary(&self, id: &str) -> String {
        match self.client.list_workers(id).await {
            Ok(w) => describe_workers(&w.summary),
            Err(e) => format!("unknown ({e})"),
        }
    }

    /// `DELETE`s endpoint `id`, retrying (bounded by
    /// [`with_stop_retry`](Self::with_stop_retry)) while RunPod refuses it
    /// with HTTP 400 or 409 (live workers; the detail may be withheld).
    async fn delete_endpoint_retrying(&self, id: &str) -> Result<DeleteOutcome, LlamaCppError> {
        let attempts = self.stop_retry.attempts.max(1);
        let mut attempt = 1;
        loop {
            match self.client.delete_endpoint(id).await {
                Ok(outcome) => return Ok(outcome),
                Err(RunPodError::Conflict { .. } | RunPodError::BadRequest { status: 400, .. })
                    if attempt < attempts =>
                {
                    attempt += 1;
                    tokio::time::sleep(self.stop_retry.delay).await;
                }
                Err(e) => return Err(runpod_err(&e)),
            }
        }
    }

    /// [`ProcessSupervisor::stop`] with its outcome: scales the endpoint to
    /// zero workers (best effort), deletes it, then deletes every known
    /// secret - unless RunPod reported the endpoint already gone and the
    /// account is not [confirmed](Self::with_confirmed_account), in which
    /// case the secrets are kept and listed.
    ///
    /// # Errors
    /// [`LlamaCppError::Supervisor`] when the endpoint delete is not
    /// confirmed (no secret is touched) or a secret could not be deleted
    /// (named in the message and kept in
    /// [`created_secrets`](Self::created_secrets)).
    pub async fn stop_endpoint(
        &self,
        handle: &RuntimeHandle,
    ) -> Result<StopOutcome, LlamaCppError> {
        // Live workers can keep a delete from going through; drain them
        // first. Any failure here (a 404 included) is left to the delete.
        let drain = UpdateEndpoint {
            workers: Some(WorkersConfig {
                min: 0,
                max: 0,
                idle_timeout: None,
            }),
            ..UpdateEndpoint::default()
        };
        let drained = self
            .client
            .update_endpoint(&handle.id, &drain)
            .await
            .is_ok();

        // The endpoint first: its secrets are only removed once RunPod has
        // confirmed it is gone, never before.
        let outcome = match self.delete_endpoint_retrying(&handle.id).await {
            Ok(o) => o,
            Err(e) if drained => {
                return Err(LlamaCppError::Supervisor(format!(
                    "RunPod endpoint {} was scaled to zero workers but could not be deleted, \
                     so it is still recorded (the deployment is down but tracked): {e}",
                    handle.id
                )));
            }
            Err(e) => return Err(e),
        };
        for name in &self.recorded_secrets {
            self.secrets.note(name);
        }
        self.secrets
            .finish_stop("endpoint", &handle.id, outcome, self.confirmed_account)
            .await
    }
}

fn describe_workers(s: &WorkerSummary) -> String {
    format!(
        "running={} idle={} initializing={} throttled={} unhealthy={}",
        s.running, s.idle, s.initializing, s.throttled, s.unhealthy
    )
}

pub(crate) fn elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

impl RunPodSupervisor {
    fn gateway_key_str(&self) -> Result<&str, LlamaCppError> {
        self.gateway_key
            .as_ref()
            .ok_or_else(|| {
                LlamaCppError::Supervisor(
                    "RunPod metrics need the deployment's api_key as the gateway key".to_owned(),
                )
            })?
            .expose_secret_str()
            .map_err(|_| {
                LlamaCppError::Supervisor("the endpoint API key is not valid UTF-8".to_owned())
            })
    }

    /// Refuses unless a worker is running or idle: any request through the
    /// gateway would otherwise start (and bill) one. `wake` skips the check.
    async fn ensure_awake(&self, id: &str, wake: bool) -> Result<(), LlamaCppError> {
        if wake {
            return Ok(());
        }
        let s = self
            .client
            .list_workers(id)
            .await
            .map_err(|e| runpod_err(&e))?
            .summary;
        if s.running.saturating_add(s.idle) > 0 {
            return Ok(());
        }
        if s.initializing.saturating_add(s.throttled) > 0 {
            return Err(LlamaCppError::Supervisor(format!(
                "a RunPod worker of endpoint {id} is starting ({} initializing, {} waiting \
                 for capacity); retry shortly",
                s.initializing, s.throttled
            )));
        }
        Err(LlamaCppError::Supervisor(format!(
            "no RunPod worker of endpoint {id} is up, and scraping /metrics would start one \
             (billed GPU time); pass --wake to start one anyway"
        )))
    }

    /// One `/metrics` scrape through the gateway with the
    /// [gateway key](Self::with_gateway_key). Without `wake`, a scaled-to-zero
    /// endpoint is refused rather than woken.
    ///
    /// # Errors
    /// No worker up (without `wake`), no gateway key, or a failed scrape.
    pub async fn scrape_metrics(
        &self,
        handle: &RuntimeHandle,
        wake: bool,
    ) -> Result<amicu_telemetry::scrape::MetricsSnapshot, LlamaCppError> {
        self.ensure_awake(&handle.id, wake).await?;
        let key = self.gateway_key_str()?;
        crate::metrics::scrape(&*self.probe, &handle.id, key)
            .await
            .map_err(|e| LlamaCppError::Supervisor(e.to_string()))
    }
}

/// A deployment's `scaling {}` block as RunPod workers, defaults filled in:
/// a lone `min_replicas` above the default ceiling raises the ceiling with it.
#[must_use]
pub fn workers_config(scaling: Option<&ScalingSettings>) -> WorkersConfig {
    let none = ScalingSettings::default();
    let scaling = scaling.unwrap_or(&none);
    let min = scaling.min_replicas.unwrap_or(DEFAULT_MIN_WORKERS);
    WorkersConfig {
        min,
        max: scaling.max_replicas.unwrap_or(DEFAULT_MAX_WORKERS.max(min)),
        idle_timeout: Some(
            scaling
                .scaledown_window_secs
                .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS)
                .clamp(1, MAX_IDLE_TIMEOUT_SECS),
        ),
    }
}

/// The PATCH that makes `live` match `create`. Volumes are only restated
/// when either side has one, so a volume-less endpoint gets the same PATCH
/// as before volumes existed.
fn update_from(create: &CreateEndpoint, live: &Endpoint) -> UpdateEndpoint {
    let had_volume = live.network_volumes.as_ref().is_some_and(|v| !v.is_empty());
    // On PATCH, `""` clears a CUDA floor and `[]` data centers clear a placement
    // the previous configuration set.
    let mut gpu = create.gpu.clone();
    gpu.min_cuda_version.get_or_insert_with(String::new);
    UpdateEndpoint {
        name: None,
        image: Some(create.image.clone()),
        entrypoint: Some(create.entrypoint.clone()),
        cmd: Some(create.cmd.clone()),
        env: Some(create.env.clone()),
        ports: Some(create.ports.clone()),
        disk_gb: create.disk_gb,
        gpu: Some(gpu),
        workers: Some(create.workers),
        request_count: Some(create.request_count),
        data_center_ids: Some(create.data_center_ids.clone()),
        flashboot: Some(create.flashboot),
        network_volume_ids: (had_volume || !create.network_volume_ids.is_empty())
            .then(|| create.network_volume_ids.clone()),
        timeout_ms: None,
        registry_id: None,
    }
}

/// Folds a [`RunPodError`] into the trait's error, with a hint when RunPod is
/// out of capacity. `RunPodError` never carries a key or secret value, so its
/// text is always safe to surface.
pub(crate) fn runpod_err(error: &RunPodError) -> LlamaCppError {
    let hint = if is_capacity_error(error) {
        CAPACITY_HINT
    } else {
        ""
    };
    LlamaCppError::Supervisor(format!("{error}{hint}"))
}

const CAPACITY_HINT: &str = "; RunPod has no free machine of this kind right now: retry \
     later, or choose another GPU (`accelerator`) or `region`, or for a pod the other cloud \
     (`runpod { cloud = \"community\" }` or \"secure\")";

/// RunPod's "There are no longer any instances available with the requested
/// specifications" (and its variants) on create.
fn is_capacity_error(error: &RunPodError) -> bool {
    let detail = match error {
        RunPodError::BadRequest { detail, .. }
        | RunPodError::Conflict { detail }
        | RunPodError::Api { detail, .. } => detail.to_ascii_lowercase(),
        _ => return false,
    };
    detail.contains("no longer any instances available")
}

fn duplicate_err<'a>(name: &str, ids: impl Iterator<Item = &'a str>) -> LlamaCppError {
    LlamaCppError::Supervisor(format!(
        "several RunPod endpoints are named `{name}` ({}); delete the extra ones first",
        ids.collect::<Vec<_>>().join(", ")
    ))
}

pub(crate) fn unsupported(what: &str) -> LlamaCppError {
    LlamaCppError::Supervisor(format!(
        "{what} are not yet supported for RunPod deployments"
    ))
}

/// The port the runtime listens on inside the container. llama.cpp uses the
/// stock image's port whatever the deployment says: only the gateway or
/// proxy URL is public, and the image's own health check targets that port.
pub(crate) fn runtime_port(plan: &LaunchPlan) -> u16 {
    match &plan.settings {
        Runtime::Vllm(s) => s.port,
        Runtime::LlamaCpp(_) => remote::LLAMACPP_IMAGE_PORT,
    }
}

/// [`remote::launch_argv`] with `--port` set to [`runtime_port`].
pub(crate) fn container_argv(
    plan: &LaunchPlan,
    gguf: Option<&GgufSource>,
) -> Result<Vec<String>, LlamaCppError> {
    let mut argv = remote::launch_argv(plan, gguf, PROVIDER)?;
    let port = runtime_port(plan).to_string();
    match argv.iter().position(|a| a == "--port") {
        Some(i) if i + 1 < argv.len() => argv[i + 1] = port,
        _ => argv.extend(["--port".to_owned(), port]),
    }
    Ok(argv)
}

const PLACEHOLDER: &str = "amicu-plan-placeholder";

/// Placeholder values for every secret `resolve_launch_secrets` would hand
/// `start`, the endpoint key always included.
pub(crate) fn placeholder_secrets(plan: &LaunchPlan) -> Vec<ResolvedSecret> {
    plan.secret_refs
        .iter()
        .map(|r| r.name.as_str())
        .chain([plan.runtime.api_key_env_var()])
        .map(|name| ResolvedSecret {
            name: name.to_owned(),
            value: SecretValue::new(PLACEHOLDER),
        })
        .collect()
}

/// The operator's `HF_TOKEN` fallback as `start` would see it, value replaced.
pub(crate) fn planned_hf_token() -> Option<&'static str> {
    std::env::var_os(HF_TOKEN_ENV_VAR).map(|_| PLACEHOLDER)
}

/// The host CUDA floor for `image`: the stock image's requirement, `None`
/// for an operator override (its requirement is unknown).
pub(crate) fn image_min_cuda(image: &str, runtime: RuntimeKind) -> Option<String> {
    (image == remote::default_image(runtime))
        .then(|| remote::default_image_min_cuda(runtime).to_owned())
}

/// The container image for `runtime`: the operator override, else the stock
/// image.
pub(crate) fn runtime_image(runtime: RuntimeKind) -> String {
    let var = match runtime {
        RuntimeKind::Vllm => VLLM_IMAGE_ENV_VAR,
        RuntimeKind::LlamaCpp => LLAMACPP_IMAGE_ENV_VAR,
    };
    std::env::var(var).unwrap_or_else(|_| remote::default_image(runtime).to_owned())
}

/// Splits a rendered argv into the container ENTRYPOINT and CMD;
/// `llama-server` maps to the stock image's binary path.
pub(crate) fn container_command(
    argv: &[String],
) -> Result<(Vec<String>, Vec<String>), LlamaCppError> {
    let (program, rest) = argv.split_first().ok_or_else(|| {
        LlamaCppError::Supervisor("the rendered launch command is empty".to_owned())
    })?;
    let program = if program == "llama-server" {
        LLAMACPP_IMAGE_SERVER_PATH.to_owned()
    } else {
        program.clone()
    };
    Ok((vec![program], rest.to_vec()))
}

#[async_trait]
impl ProcessSupervisor for RunPodSupervisor {
    async fn start(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        let image = runtime_image(RuntimeKind::of(&plan.settings));
        let hf_token_fallback = std::env::var(HF_TOKEN_ENV_VAR).ok();
        let mut prepared = self.prepare(plan, secrets, &image, hf_token_fallback.as_deref())?;
        // Before anything is written: exclude every other card RunPod lists
        // in the pool today, not just the ones the static table knows.
        let catalog = self
            .client
            .get_gpu_catalog()
            .await
            .map_err(|e| runpod_err(&e))?;
        self.exclude_from_catalog(&mut prepared.endpoint.gpu, &catalog)?;

        // Recorded secrets may still exist until this run retires them.
        for name in &self.recorded_secrets {
            self.secrets.note(name);
        }
        let mut fresh = Vec::new();
        for secret in &prepared.secrets {
            if self.secrets.ensure(secret).await? {
                fresh.push(secret.name.clone());
            }
        }
        let id = self.upsert_endpoint(&prepared, &fresh).await?;
        // The endpoint now references only the current generation.
        self.secrets.retire_stale(&prepared.secrets).await;
        self.wait_ready(&id, &prepared.api_key, plan.deployment.as_str())
            .await?;
        Ok(RuntimeHandle {
            endpoint: endpoint_url(&id),
            id,
            deployment: plan.deployment.clone(),
            exposure: EndpointExposure::Public {
                gateway: GatewayAuth::RunPodBearer,
            },
        })
    }

    async fn status(&self, handle: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError> {
        match self.client.get_endpoint(&handle.id).await {
            Ok(_) => {}
            Err(RunPodError::NotFound { .. }) => {
                return Ok(RuntimeStatus::Stopped {
                    detail: "RunPod endpoint not found".to_owned(),
                });
            }
            Err(e) => return Err(runpod_err(&e)),
        }
        let s = self
            .client
            .list_workers(&handle.id)
            .await
            .map_err(|e| runpod_err(&e))?
            .summary;
        let ready = s.running.saturating_add(s.idle);
        Ok(if ready > 0 {
            RuntimeStatus::Running {
                detail: format!("{ready} worker(s) ready"),
            }
        } else if s.initializing > 0 || s.throttled > 0 {
            // A cold start or a wait for capacity: not a failure.
            RuntimeStatus::Running {
                detail: format!(
                    "starting ({} initializing, {} throttled)",
                    s.initializing, s.throttled
                ),
            }
        } else if s.unhealthy > 0 {
            RuntimeStatus::Failed {
                detail: format!("{} worker(s) unhealthy", s.unhealthy),
            }
        } else {
            RuntimeStatus::Running {
                detail: "scaled to zero; the next request starts a worker".to_owned(),
            }
        })
    }

    /// Scrapes `/metrics` through the gateway when a worker is up (never
    /// waking a scaled-to-zero endpoint); needs
    /// [`with_gateway_key`](RunPodSupervisor::with_gateway_key).
    async fn metrics(&self, handle: &RuntimeHandle) -> Result<RuntimeMetrics, LlamaCppError> {
        self.ensure_awake(&handle.id, false).await?;
        let key = self.gateway_key_str()?;
        crate::metrics::scrape_with_rate(&*self.probe, &handle.id, key, Duration::from_secs(1))
            .await
            .map(|m| m.to_runtime_metrics())
            .map_err(|e| LlamaCppError::Supervisor(e.to_string()))
    }

    /// Every worker's recent lines, `[<worker>]`-prefixed; a bounded read, so
    /// `follow` is refused. A scaled-to-zero endpoint yields one line saying so.
    async fn logs(
        &self,
        handle: &RuntimeHandle,
        options: LogOptions,
    ) -> Result<LogStream, LlamaCppError> {
        if options.follow {
            return Err(LlamaCppError::Supervisor(
                "following RunPod logs is not supported; read them without follow".to_owned(),
            ));
        }
        let tail = options.tail.unwrap_or(crate::logs::DEFAULT_TAIL);
        let workers = self
            .client
            .endpoint_logs(&handle.id, tail, crate::logs::WORKER_LOG_LIMITS)
            .await
            .map_err(|e| runpod_err(&e))?;
        if workers.is_empty() {
            return Ok(LogStream(vec![crate::logs::NO_WORKERS.to_owned()]));
        }
        Ok(LogStream(crate::logs::merge(&workers)))
    }

    /// See [`RunPodSupervisor::stop_endpoint`]; an unconfirmed
    /// [`StopOutcome::AlreadyGone`] keeps its secrets in
    /// [`created_secrets`](RunPodSupervisor::created_secrets).
    async fn stop(&self, handle: &RuntimeHandle) -> Result<(), LlamaCppError> {
        stop_result("endpoint", &handle.id, self.stop_endpoint(handle).await?)
    }
}

#[cfg(test)]
pub(crate) mod tests;
