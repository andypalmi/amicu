//! [`RunPodSupervisor`]: runs a rendered vLLM / llama.cpp launch as a RunPod
//! Serverless load-balancing endpoint, behind the same
//! [`amicu_llamacpp::ProcessSupervisor`] contract the local and Modal
//! supervisors implement.
//!
//! # What runs
//!
//! The stock image ([`DEFAULT_VLLM_IMAGE`](amicu_llamacpp::remote::DEFAULT_VLLM_IMAGE)
//! or the llama.cpp `server-cuda` build) with amicu's own rendered argv, made
//! container-ready by the same shared rewrite Modal uses
//! ([`amicu_llamacpp::remote::launch_argv`]: `--host 0.0.0.0`, `-hf`/`-hff`
//! for a GGUF, no local paths or secret flags). `argv[0]` becomes the
//! container `ENTRYPOINT` (`llama-server` is mapped to the image's
//! `/app/llama-server`), the rest the `CMD`. No image is built.
//!
//! The load balancer routes to the worker's `PORT` and polls
//! `HEALTH_CHECK_PATH` on `PORT_HEALTH` (`200` ready, `204` initializing);
//! both runtimes serve `/health` on their own port, so all three are set to
//! the deployment's port and [`HEALTH_PATH`]. The endpoint is reachable at
//! `https://<id>.api.runpod.ai` ([`endpoint_url`]).
//!
//! # Authentication: the gateway only
//!
//! RunPod's gateway authenticates every request to `*.api.runpod.ai` with
//! `Authorization: Bearer <RunPod API key>` and **strips that header** before
//! the request reaches the worker (custom headers are forwarded). The gateway
//! is therefore the only auth layer ([`GatewayAuth::RunPodBearer`]): the
//! deployment's `api_key` must be a RunPod API key with `api.runpod.ai` access
//! (ideally restricted to this endpoint), and it is a RunPod credential, so it
//! never reaches the worker. `start` takes the runtime's key variable
//! (`VLLM_API_KEY` / `LLAMA_API_KEY`) out of the resolved secrets, uses it only
//! to probe readiness through the gateway, and sets no runtime `--api-key`
//! (the runtime would reject every request, since the header never arrives).
//! Neither key variable may appear in the deployment's `env` block.
//!
//! # Secrets
//!
//! Every other resolved secret (`HF_TOKEN`, any `secret_refs`) is stored as a
//! RunPod account secret and referenced from the endpoint env as
//! `{{ RUNPOD_SECRET_<name> }}` ([`secret_reference`]), so the endpoint
//! configuration RunPod stores holds only the template, never a value.
//!
//! Names are content-addressed ([`secret_name`]): the deployment's readable
//! prefix plus a truncated SHA-256 of the value, salted with the endpoint's
//! [`owner_marker`] and the variable name. The same value always maps to the
//! same name, so `start` skips a name that already exists and creates only
//! new ones; a changed value is a new name, so the live endpoint keeps
//! resolving its old secret until the PATCH that switches it over succeeds.
//! Only then are the previous generation's secrets (recorded in state via
//! [`RunPodSupervisor::with_recorded_secrets`], or written by this instance)
//! deleted. The digest is 48 bits of a salted hash: it does not reveal a
//! high-entropy token, but a guessable low-entropy value could be confirmed
//! by someone able to list the account's secret names.
//!
//! [`RunPodSupervisor::planned_secret_names`] gives the exact names before
//! `start` so a caller can record them write-ahead;
//! [`RunPodSupervisor::created_secrets`] tracks every name that may exist
//! for this deployment as far as this instance knows (recorded, written, or
//! left behind by a failed delete).
//!
//! Residual risk: a secret whose name already exists is trusted to hold the
//! value its name encodes. Names include the owner marker (workspace,
//! project, deployment), so another amicu deployment never computes the same
//! name, but someone with access to the account could pre-create one.
//! `start` and `stop` only ever delete names they recorded or wrote.
//!
//! # Create, never blind-retry; adopt only what is ours
//!
//! `start` prefers the endpoint id recorded in state
//! ([`RunPodSupervisor::with_recorded_endpoint`]). Without one (or when it is
//! gone) it looks the name up: RunPod does not enforce unique names, so
//! several matches are refused, and a single match is adopted only when its
//! env carries this deployment's [`OWNER_ENV_VAR`] marker, which `start`
//! sets on every endpoint it writes. A create whose outcome is unknown
//! ([`RunPodError::AmbiguousCreate`]) is resolved by looking the name up
//! again (with the same owner check), never by sending the create twice. The
//! endpoint id lands in [`RunPodSupervisor::deployed_endpoint`] as soon as it
//! is known, so a caller can record it even when `start` then fails.
//!
//! # GPU placement
//!
//! The static table in [`crate::hardware`] resolves the requested card to a
//! pool and type id; before writing anything, `start` reads
//! `GET /v2/catalog/gpus` and excludes every other type RunPod lists in that
//! pool ([`crate::hardware::exclusions_from_catalog`]), so a card RunPod adds
//! later can never be picked instead. A catalog that cannot be read, or that
//! does not list the card in that pool, stops the deploy.
//!
//! `metrics`/`logs` are not implemented for RunPod yet and return a clear
//! error.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use amicu_core::naming::{project_app_name, project_endpoint_name};
use amicu_core::resource::{Accelerator, HardwareProfile, Model, Region, Runtime, ScalingSettings};
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
    CreateEndpoint, CreateSecret, DeleteOutcome, Endpoint, GpuRequest, GpuType, OWNER_ENV_VAR,
    RunPodClient, UpdateEndpoint, WorkersConfig,
};
use crate::error::RunPodError;
use crate::hardware::{exclusions_from_catalog, gpu_placement};
use crate::regions::select_data_centers;

/// The provider name the shared remote-launch renderer puts in its errors.
pub const PROVIDER: &str = "RunPod";

/// Operator environment variable overriding the vLLM image.
pub const VLLM_IMAGE_ENV_VAR: &str = "AMICU_RUNPOD_VLLM_IMAGE";

/// Operator environment variable overriding the llama.cpp image.
pub const LLAMACPP_IMAGE_ENV_VAR: &str = "AMICU_RUNPOD_LLAMACPP_IMAGE";

/// The health route the load balancer polls and `start` waits on. Both
/// llama-server and vLLM serve it on their own port, unauthenticated by the
/// runtime (the RunPod gateway still requires the bearer key).
///
/// llama-server answers `503` while it loads the model, and RunPod's load
/// balancer treats anything but `200`/`204` as unhealthy, so a slow model
/// load may show as unhealthy workers (and possibly get them recycled)
/// rather than initializing ones.
// TODO(#306): check in the live test whether RunPod recycles a llama.cpp
// worker that answers 503 during a long model load; if it does, point the
// health check at a route that answers 204 while loading.
pub const HEALTH_PATH: &str = "/health";

/// Idle seconds before a worker scales down, unless the deployment's
/// `scaling { scaledown_window }` says otherwise (same default as Modal).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u32 = 900;

/// RunPod's ceiling for `workers.idleTimeout`.
pub const MAX_IDLE_TIMEOUT_SECS: u32 = 3600;

/// Workers kept warm unless overridden: scale to zero.
pub const DEFAULT_MIN_WORKERS: u32 = 0;

/// Worker ceiling unless `scaling { max_replicas }` says otherwise. RunPod
/// needs a finite bound (Modal's default is unlimited).
pub const DEFAULT_MAX_WORKERS: u32 = 3;

/// In-flight requests per worker before RunPod scales out, unless
/// `scaling { target_concurrency }` says otherwise (same default as Modal).
pub const DEFAULT_REQUEST_COUNT: u32 = 32;

/// Container disk per worker, in GB. Without a network volume (a later
/// option) the model weights download onto this disk at each cold start.
pub const DEFAULT_CONTAINER_DISK_GB: u32 = 100;

/// Env variables the load balancer (or amicu's owner check) reads; the
/// deployment's `env` block and secrets may not set them.
const RESERVED_ENV: [&str; 4] = ["PORT", "PORT_HEALTH", "HEALTH_CHECK_PATH", OWNER_ENV_VAR];

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
    gpu_type: Option<String>,
    data_center_ids: Vec<String>,
    scaling: ScalingSettings,
    disk_gb: u32,
    gguf: Option<GgufSource>,
    readiness: ReadinessTiming,
    stop_retry: StopRetry,
    confirmed_account: bool,
    recorded_endpoint: Option<String>,
    recorded_secrets: Vec<String>,
    deployed_endpoint: Arc<Mutex<Option<String>>>,
    created_secrets: Arc<Mutex<Vec<String>>>,
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
        Self {
            client: Arc::new(client),
            probe: Arc::new(ReqwestTransport::new()),
            workspace_id: None,
            project_id: project_id.to_owned(),
            deployment_id: deployment_id.to_owned(),
            endpoint_name: project_endpoint_name(project_id, deployment_id),
            gpu: None,
            gpu_type: None,
            data_center_ids: Vec::new(),
            scaling: ScalingSettings::default(),
            disk_gb: DEFAULT_CONTAINER_DISK_GB,
            gguf: None,
            readiness: ReadinessTiming::DEFAULT,
            stop_retry: StopRetry::DEFAULT,
            confirmed_account: false,
            recorded_endpoint: None,
            recorded_secrets: Vec::new(),
            deployed_endpoint: Arc::default(),
            created_secrets: Arc::default(),
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
        self.gpu_type = placement.as_ref().map(|p| p.type_id.clone());
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
        let (Some(type_id), [pool]) = (&self.gpu_type, gpu.pools.as_slice()) else {
            return Err(LlamaCppError::Supervisor(
                "RunPod GPU placement needs exactly one pool and a known GPU type".to_owned(),
            ));
        };
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
                select_data_centers(&r.code, Some(&r.residency)).map_err(|e| runpod_err(&e))?
            }
            None => Vec::new(),
        };
        Ok(self)
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

    /// Readiness polling bounds (default [`ReadinessTiming::DEFAULT`]).
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

    /// Names of the RunPod secrets that may exist for this deployment as far
    /// as this instance knows: the recorded ones until they are deleted, and
    /// each one `start` writes (added before its create call). Deletions
    /// remove names, so after `start` or `stop` this is the set to record.
    #[must_use]
    pub fn created_secrets(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.created_secrets)
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
        remote::launch_argv(plan, self.gguf.as_ref(), PROVIDER)
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
        let port = match &plan.settings {
            Runtime::Vllm(s) => s.port,
            Runtime::LlamaCpp(s) => s.port,
        };
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
        if let Some(k) = RESERVED_ENV
            .iter()
            .find(|k| plain.contains_key(**k) || secret.contains_key(**k))
        {
            return Err(LlamaCppError::Supervisor(format!(
                "env var '{k}' is reserved on RunPod (the load balancer or amicu reads it); \
                 remove it from the deployment's `env` block"
            )));
        }
        let gpu = self.gpu.clone().ok_or_else(|| {
            LlamaCppError::Supervisor(
                "RunPod Serverless needs a GPU accelerator in the deployment's hardware profile"
                    .to_owned(),
            )
        })?;

        let owner = self.owner();
        let mut env = plain;
        env.insert("PORT".to_owned(), port.to_string());
        env.insert("PORT_HEALTH".to_owned(), port.to_string());
        env.insert("HEALTH_CHECK_PATH".to_owned(), HEALTH_PATH.to_owned());
        env.insert(OWNER_ENV_VAR.to_owned(), owner.clone());
        let mut create_secrets = Vec::with_capacity(secret.len());
        for (key, value) in &secret {
            let name = secret_name(&self.project_id, &self.deployment_id, &owner, key, value);
            env.insert(key.clone(), secret_reference(&name));
            create_secrets.push(CreateSecret {
                name,
                value: value.clone(),
                description: Some(format!(
                    "amicu: {key} for deployment {}",
                    self.deployment_id
                )),
            });
        }

        let (program, rest) = argv.split_first().ok_or_else(|| {
            LlamaCppError::Supervisor("the rendered launch command is empty".to_owned())
        })?;
        let program = if program == "llama-server" {
            LLAMACPP_IMAGE_SERVER_PATH.to_owned()
        } else {
            program.clone()
        };

        let mut endpoint = CreateEndpoint::new(self.endpoint_name.clone(), image, gpu);
        endpoint.entrypoint = vec![program];
        endpoint.cmd = rest.to_vec();
        endpoint.env = env;
        endpoint.ports = vec![format!("{port}/http")];
        endpoint.disk_gb = Some(self.disk_gb);
        endpoint.workers = WorkersConfig {
            min: self.scaling.min_replicas.unwrap_or(DEFAULT_MIN_WORKERS),
            max: self.scaling.max_replicas.unwrap_or(DEFAULT_MAX_WORKERS),
            idle_timeout: Some(
                self.scaling
                    .scaledown_window_secs
                    .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS)
                    .clamp(1, MAX_IDLE_TIMEOUT_SECS),
            ),
        };
        endpoint.request_count = self
            .scaling
            .target_concurrency
            .unwrap_or(DEFAULT_REQUEST_COUNT)
            .max(1);
        endpoint.data_center_ids.clone_from(&self.data_center_ids);

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

    fn note_secret(&self, name: &str) {
        if let Ok(mut created) = self.created_secrets.lock()
            && !created.iter().any(|n| n == name)
        {
            created.push(name.to_owned());
        }
    }

    fn forget_secret(&self, name: &str) {
        if let Ok(mut created) = self.created_secrets.lock() {
            created.retain(|n| n != name);
        }
    }

    fn known_secrets(&self) -> Vec<String> {
        self.created_secrets
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    /// Makes sure `secret` exists. Names are content-addressed, so an
    /// existing name already holds this value and is left alone. Returns
    /// whether this call created it.
    async fn ensure_secret(&self, secret: &CreateSecret) -> Result<bool, LlamaCppError> {
        let name = &secret.name;
        let exists = self
            .client
            .find_secret(name)
            .await
            .map_err(|e| secret_err(name, &e))?
            .is_some();
        self.note_secret(name);
        if exists {
            return Ok(false);
        }
        match self.client.create_secret(secret).await {
            Ok(_) => Ok(true),
            // Created concurrently under the same name: the same value.
            Err(RunPodError::Conflict { .. }) => Ok(false),
            // The response was lost: whether it exists is a lookup away.
            Err(RunPodError::Transport(detail)) => match self.client.find_secret(name).await {
                Ok(Some(_)) => Ok(true),
                Ok(None) => {
                    self.forget_secret(name);
                    Err(LlamaCppError::Supervisor(format!(
                        "writing RunPod secret `{name}` failed in transport ({detail}) and it \
                         does not exist; run the command again"
                    )))
                }
                Err(e) => Err(LlamaCppError::Supervisor(format!(
                    "writing RunPod secret `{name}` failed in transport ({detail}) and looking \
                     it up failed too ({e}); it may exist"
                ))),
            },
            Err(e) => {
                // RunPod answered and refused: nothing was written.
                self.forget_secret(name);
                Err(secret_err(name, &e))
            }
        }
    }

    /// Best-effort removal of secrets this run created for an endpoint write
    /// RunPod refused; whatever cannot be removed stays in
    /// [`created_secrets`](Self::created_secrets) for the caller.
    async fn discard_secrets(&self, names: &[String]) {
        for name in names {
            if self.delete_secret_by_name(name).await.is_ok() {
                self.forget_secret(name);
            }
        }
    }

    /// Once the endpoint references only `current`: deletes every other
    /// known secret (the previous generation). Best effort; what cannot be
    /// deleted stays in [`created_secrets`](Self::created_secrets).
    async fn retire_stale_secrets(&self, current: &[CreateSecret]) {
        let current: BTreeSet<&str> = current.iter().map(|s| s.name.as_str()).collect();
        for name in self.known_secrets() {
            if !current.contains(name.as_str()) && self.delete_secret_by_name(&name).await.is_ok() {
                self.forget_secret(&name);
            }
        }
    }

    async fn delete_secret_by_name(&self, name: &str) -> Result<(), RunPodError> {
        if let Some(existing) = self.client.find_secret(name).await? {
            self.client.delete_secret(&existing.id).await?;
        }
        Ok(())
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
        prepared: &Prepared,
        fresh: &[String],
    ) -> Result<String, LlamaCppError> {
        self.set_deployed(id);
        match self
            .client
            .update_endpoint(id, &update_from(&prepared.endpoint))
            .await
        {
            Ok(_) => Ok(id.to_owned()),
            Err(e) => {
                if !matches!(e, RunPodError::Transport(_)) {
                    self.discard_secrets(fresh).await;
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
                    return self.patch_endpoint(&ep.id, prepared, fresh).await;
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
                return self.patch_endpoint(&ep.id, prepared, fresh).await;
            }
            many => return Err(duplicate_err(name, many.iter().map(|e| e.id.as_str()))),
        }
        match self.client.create_endpoint(&prepared.endpoint).await {
            Ok(created) => {
                self.set_deployed(&created.id);
                Ok(created.id)
            }
            Err(RunPodError::AmbiguousCreate(detail)) => {
                self.recover_ambiguous(name, &detail).await
            }
            Err(e) => {
                // RunPod answered and refused: nothing references the
                // secrets written for it.
                self.discard_secrets(fresh).await;
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
    /// accept the key, waiting will not help).
    async fn wait_ready(&self, id: &str, key: &SecretValue) -> Result<(), LlamaCppError> {
        let key = key.expose_secret_str().map_err(|_| {
            LlamaCppError::Supervisor("the endpoint API key is not valid UTF-8".to_owned())
        })?;
        let url = format!("{}{HEALTH_PATH}", endpoint_url(id));
        let timing = self.readiness;
        let deadline = tokio::time::Instant::now() + timing.timeout;
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
                     {last}); it is left in place",
                    timing.timeout.as_secs()
                )));
            }
            tokio::time::sleep(backoff.min(deadline - now)).await;
            backoff = (backoff * 2).min(timing.max_backoff);
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
            self.note_secret(name);
        }
        let mut names = self.known_secrets();
        names.sort();
        if outcome == DeleteOutcome::AlreadyGone && !self.confirmed_account {
            return Ok(StopOutcome::AlreadyGone {
                kept_secrets: names,
            });
        }
        let mut failed: BTreeMap<String, String> = BTreeMap::new();
        for name in names {
            match self.delete_secret_by_name(&name).await {
                Ok(()) => self.forget_secret(&name),
                Err(e) => {
                    failed.insert(name, e.to_string());
                }
            }
        }
        if !failed.is_empty() {
            return Err(LlamaCppError::Supervisor(format!(
                "RunPod endpoint {} was deleted, but these secrets could not be: {}",
                handle.id,
                failed
                    .iter()
                    .map(|(n, e)| format!("`{n}` ({e})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(match outcome {
            DeleteOutcome::Deleted => StopOutcome::Deleted,
            DeleteOutcome::AlreadyGone => StopOutcome::AlreadyGone {
                kept_secrets: Vec::new(),
            },
        })
    }
}

/// The PATCH that makes an existing endpoint match `create`.
fn update_from(create: &CreateEndpoint) -> UpdateEndpoint {
    UpdateEndpoint {
        name: None,
        image: Some(create.image.clone()),
        entrypoint: Some(create.entrypoint.clone()),
        cmd: Some(create.cmd.clone()),
        env: Some(create.env.clone()),
        ports: Some(create.ports.clone()),
        disk_gb: create.disk_gb,
        gpu: Some(create.gpu.clone()),
        workers: Some(create.workers),
        request_count: Some(create.request_count),
        data_center_ids: (!create.data_center_ids.is_empty())
            .then(|| create.data_center_ids.clone()),
        flashboot: Some(create.flashboot),
        network_volume_ids: None,
        timeout_ms: None,
        registry_id: None,
    }
}

/// Folds a [`RunPodError`] into the trait's error. `RunPodError` never carries
/// a key or secret value, so its text is always safe to surface.
fn runpod_err(error: &RunPodError) -> LlamaCppError {
    LlamaCppError::Supervisor(error.to_string())
}

fn secret_err(name: &str, error: &RunPodError) -> LlamaCppError {
    LlamaCppError::Supervisor(format!("writing RunPod secret `{name}` failed: {error}"))
}

fn duplicate_err<'a>(name: &str, ids: impl Iterator<Item = &'a str>) -> LlamaCppError {
    LlamaCppError::Supervisor(format!(
        "several RunPod endpoints are named `{name}` ({}); delete the extra ones first",
        ids.collect::<Vec<_>>().join(", ")
    ))
}

fn unsupported(what: &str) -> LlamaCppError {
    LlamaCppError::Supervisor(format!(
        "{what} are not yet supported for RunPod deployments"
    ))
}

#[async_trait]
impl ProcessSupervisor for RunPodSupervisor {
    async fn start(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        let runtime = RuntimeKind::of(&plan.settings);
        let image_var = match runtime {
            RuntimeKind::Vllm => VLLM_IMAGE_ENV_VAR,
            RuntimeKind::LlamaCpp => LLAMACPP_IMAGE_ENV_VAR,
        };
        let image =
            std::env::var(image_var).unwrap_or_else(|_| remote::default_image(runtime).to_owned());
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
            self.note_secret(name);
        }
        let mut fresh = Vec::new();
        for secret in &prepared.secrets {
            if self.ensure_secret(secret).await? {
                fresh.push(secret.name.clone());
            }
        }
        let id = self.upsert_endpoint(&prepared, &fresh).await?;
        // The endpoint now references only the current generation.
        self.retire_stale_secrets(&prepared.secrets).await;
        self.wait_ready(&id, &prepared.api_key).await?;
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

    async fn metrics(&self, _handle: &RuntimeHandle) -> Result<RuntimeMetrics, LlamaCppError> {
        Err(unsupported("metrics"))
    }

    async fn logs(
        &self,
        _handle: &RuntimeHandle,
        _options: LogOptions,
    ) -> Result<LogStream, LlamaCppError> {
        Err(unsupported("logs"))
    }

    /// See [`RunPodSupervisor::stop_endpoint`]; an unconfirmed
    /// [`StopOutcome::AlreadyGone`] keeps its secrets in
    /// [`created_secrets`](RunPodSupervisor::created_secrets).
    async fn stop(&self, handle: &RuntimeHandle) -> Result<(), LlamaCppError> {
        match self.stop_endpoint(handle).await? {
            StopOutcome::Deleted => Ok(()),
            StopOutcome::AlreadyGone { kept_secrets } if kept_secrets.is_empty() => Ok(()),
            StopOutcome::AlreadyGone { kept_secrets } => Err(LlamaCppError::Supervisor(format!(
                "RunPod endpoint {} was already gone, but the account could not be \
                     confirmed, so these secrets were kept: {}",
                handle.id,
                kept_secrets
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }
}

#[cfg(test)]
mod tests;
