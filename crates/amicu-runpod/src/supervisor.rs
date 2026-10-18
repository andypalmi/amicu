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
//! # Secrets and the API key
//!
//! Every resolved secret - the deployment's `api_key` (under the runtime's
//! own key variable, `VLLM_API_KEY` / `LLAMA_API_KEY`, which both runtimes
//! read as their `--api-key`), `HF_TOKEN`, any other `secret_refs` - is stored
//! as a RunPod account secret named deterministically by [`secret_name`] and
//! referenced from the endpoint env as `{{ RUNPOD_SECRET_<name> }}`
//! ([`secret_reference`]). The endpoint configuration RunPod stores (and
//! returns from its API) therefore holds only the template, never a value.
//! [`RunPodSupervisor::planned_secret_names`] gives the names before `start`
//! so a caller can record them write-ahead; [`RunPodSupervisor::created_secrets`]
//! tracks what `start` actually wrote.
//!
//! RunPod's gateway authenticates every request to `*.api.runpod.ai` with
//! `Authorization: Bearer <RunPod API key>` and forwards that header to the
//! worker, where the runtime checks it against its own `--api-key`. One
//! header must pass both, so the deployment's `api_key` has to be a RunPod API
//! key allowed to call this endpoint (ideally a restricted key scoped to it).
//! That is [`GatewayAuth::RunPodBearer`]: no extra gateway headers, end users
//! send `Authorization: Bearer <api_key>` and nothing else.
//!
//! # Create, never blind-retry
//!
//! RunPod does not enforce unique endpoint names, so `start` looks the name up
//! first (one match: update it in place; several: refuse), and a create whose
//! outcome is unknown ([`RunPodError::AmbiguousCreate`]) is resolved by
//! looking the name up again, never by sending the create twice. The endpoint
//! id lands in [`RunPodSupervisor::deployed_endpoint`] as soon as it is known,
//! so a caller can record it even when `start` then fails.
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

use std::collections::BTreeMap;
use std::fmt;
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

use crate::client::{
    CreateEndpoint, CreateSecret, DeleteOutcome, GpuRequest, GpuType, RunPodClient, UpdateEndpoint,
    WorkersConfig,
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

/// Env variables the load balancer reads; the deployment's `env` block may
/// not set them.
const RESERVED_ENV: [&str; 3] = ["PORT", "PORT_HEALTH", "HEALTH_CHECK_PATH"];

/// The public base URL of endpoint `id`.
#[must_use]
pub fn endpoint_url(id: &str) -> String {
    format!("https://{id}.api.runpod.ai")
}

/// The RunPod secret holding env variable `env_key` for a deployment:
/// `amicu-<project>-<deployment>-<env key>-<hash>`, from the same
/// [`amicu_core::naming`] scheme as the endpoint name, so it is stable across
/// runs, distinct per project / deployment / variable, and at most 63
/// characters of `[a-z0-9-]`.
#[must_use]
pub fn secret_name(project_id: &str, deployment_id: &str, env_key: &str) -> String {
    project_app_name(project_id, &format!("{deployment_id}/{env_key}"))
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

/// Everything `start` sends, built purely from the plan and the resolved
/// secrets. `Debug` never prints a secret value.
struct Prepared {
    endpoint: CreateEndpoint,
    secrets: Vec<CreateSecret>,
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
/// endpoint and secret names). `status`/`stop` address the endpoint through
/// the [`RuntimeHandle`] alone; `start` additionally needs
/// [`with_accelerator`](Self::with_accelerator).
pub struct RunPodSupervisor {
    client: Arc<RunPodClient>,
    probe: Arc<dyn HttpTransport>,
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

    /// Secret names recorded in state for this deployment, deleted by `stop`
    /// together with the ones this instance created and the standard ones
    /// ([`planned_secret_names`](Self::planned_secret_names) with no secrets).
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

    /// Names of the RunPod secrets `start` may have written (each is added
    /// before its create call). `stop` removes the ones it deletes.
    #[must_use]
    pub fn created_secrets(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.created_secrets)
    }

    /// The RunPod secret names a `start` with `secrets` writes, for recording
    /// before the call: one per resolved secret, plus the `HF_TOKEN` and both
    /// runtimes' API-key variables (which a start may add or a previous run
    /// may have left). Recording a name that is never created is harmless.
    #[must_use]
    pub fn planned_secret_names(&self, secrets: &[ResolvedSecret]) -> Vec<String> {
        let mut keys: Vec<&str> = secrets.iter().map(|s| s.name.as_str()).collect();
        keys.extend([
            HF_TOKEN_ENV_VAR,
            RuntimeKind::Vllm.api_key_env_var(),
            RuntimeKind::LlamaCpp.api_key_env_var(),
        ]);
        let mut names: Vec<String> = keys
            .into_iter()
            .map(|k| secret_name(&self.project_id, &self.deployment_id, k))
            .collect();
        names.sort();
        names.dedup();
        names
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
        let RemoteEnv { plain, secret } = remote::split_env(plan, secrets, hf_token_fallback)?;

        let key_var = runtime.api_key_env_var();
        let api_key = secret.get(key_var).ok_or_else(|| {
            LlamaCppError::Supervisor(
                "RunPod deployments need an `api_key`: the endpoint is public, and RunPod's \
                 gateway and the runtime both check it as the bearer key"
                    .to_owned(),
            )
        })?;
        if let Some(k) = RESERVED_ENV
            .iter()
            .find(|k| plain.contains_key(**k) || secret.contains_key(**k))
        {
            return Err(LlamaCppError::Supervisor(format!(
                "env var '{k}' is reserved on RunPod (the load balancer reads it); remove it from \
                 the deployment's `env` block"
            )));
        }
        let gpu = self.gpu.clone().ok_or_else(|| {
            LlamaCppError::Supervisor(
                "RunPod Serverless needs a GPU accelerator in the deployment's hardware profile"
                    .to_owned(),
            )
        })?;

        let mut env = plain;
        for k in RESERVED_ENV.iter().take(2) {
            env.insert((*k).to_owned(), port.to_string());
        }
        env.insert("HEALTH_CHECK_PATH".to_owned(), HEALTH_PATH.to_owned());
        let mut create_secrets = Vec::with_capacity(secret.len());
        for (key, value) in &secret {
            let name = secret_name(&self.project_id, &self.deployment_id, key);
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
            api_key: SecretValue::new(api_key.clone()),
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

    /// Writes one secret. Names are immutable on RunPod, so an existing
    /// secret of that name (a previous run, or a create whose response was
    /// lost) is deleted and written again with the current value.
    async fn put_secret(&self, secret: &CreateSecret) -> Result<(), LlamaCppError> {
        self.note_secret(&secret.name);
        match self.client.create_secret(secret).await {
            Ok(_) => return Ok(()),
            Err(RunPodError::Conflict { .. } | RunPodError::Transport(_)) => {}
            Err(e) => return Err(secret_err(&secret.name, &e)),
        }
        if let Some(existing) = self
            .client
            .find_secret(&secret.name)
            .await
            .map_err(|e| secret_err(&secret.name, &e))?
        {
            self.client
                .delete_secret(&existing.id)
                .await
                .map_err(|e| secret_err(&secret.name, &e))?;
        }
        self.client
            .create_secret(secret)
            .await
            .map(|_| ())
            .map_err(|e| secret_err(&secret.name, &e))
    }

    /// Best-effort removal of secrets written for a create that definitely
    /// failed; whatever cannot be removed stays in
    /// [`created_secrets`](Self::created_secrets) for the caller.
    async fn discard_secrets(&self, secrets: &[CreateSecret]) {
        for s in secrets {
            if self.delete_secret_by_name(&s.name).await.is_ok() {
                self.forget_secret(&s.name);
            }
        }
    }

    async fn delete_secret_by_name(&self, name: &str) -> Result<(), RunPodError> {
        if let Some(existing) = self.client.find_secret(name).await? {
            self.client.delete_secret(&existing.id).await?;
        }
        Ok(())
    }

    /// Creates the endpoint, or updates the one already carrying its name.
    /// Returns the endpoint id.
    async fn upsert_endpoint(&self, prepared: &Prepared) -> Result<String, LlamaCppError> {
        let name = &prepared.endpoint.name;
        let existing = self
            .client
            .find_endpoints_by_name(name)
            .await
            .map_err(|e| runpod_err(&e))?;
        match existing.as_slice() {
            [] => {}
            [one] => {
                self.set_deployed(&one.id);
                self.client
                    .update_endpoint(&one.id, &update_from(&prepared.endpoint))
                    .await
                    .map_err(|e| runpod_err(&e))?;
                return Ok(one.id.clone());
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
                self.discard_secrets(&prepared.secrets).await;
                Err(runpod_err(&e))
            }
        }
    }

    /// The create's response was lost: the endpoint may exist. Look it up by
    /// name; never send the create again.
    async fn recover_ambiguous(&self, name: &str, detail: &str) -> Result<String, LlamaCppError> {
        let found = self
            .client
            .find_endpoints_by_name(name)
            .await
            .map_err(|e| {
                LlamaCppError::Supervisor(format!(
                    "RunPod create outcome unknown ({detail}) and looking up endpoint `{name}` \
                 failed ({e}); look it up by name before retrying"
                ))
            })?;
        match found.as_slice() {
            [one] => {
                self.set_deployed(&one.id);
                Ok(one.id.clone())
            }
            [] => Err(LlamaCppError::Supervisor(format!(
                "RunPod create outcome unknown ({detail}) and no endpoint named `{name}` exists \
                 yet; it was not retried - run the command again to create it"
            ))),
            many => Err(duplicate_err(name, many.iter().map(|e| e.id.as_str()))),
        }
    }

    /// Polls `<endpoint>/health` with the bearer key until a worker answers
    /// `200`. `204` (initializing), "no workers" answers and transport
    /// failures are retried with a doubling backoff until the timeout; `401`/
    /// `403` fail at once (the key is not accepted, waiting will not help).
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
                        "RunPod endpoint {id} rejected the deployment's api_key on {HEALTH_PATH} \
                         (HTTP {}); on RunPod the api_key must be a RunPod API key allowed to \
                         call this endpoint",
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

    /// Every secret name `stop` should make sure is gone.
    fn secrets_to_delete(&self) -> Vec<String> {
        let mut names = self.planned_secret_names(&[]);
        names.extend(self.recorded_secrets.iter().cloned());
        if let Ok(created) = self.created_secrets.lock() {
            names.extend(created.iter().cloned());
        }
        names.sort();
        names.dedup();
        names
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

        for secret in &prepared.secrets {
            self.put_secret(secret).await?;
        }
        let id = self.upsert_endpoint(&prepared).await?;
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
        } else if s.unhealthy > 0 {
            RuntimeStatus::Failed {
                detail: format!("{} worker(s) unhealthy", s.unhealthy),
            }
        } else if s.initializing > 0 || s.throttled > 0 {
            RuntimeStatus::Failed {
                detail: format!(
                    "endpoint not ready ({} initializing, {} throttled)",
                    s.initializing, s.throttled
                ),
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

    async fn stop(&self, handle: &RuntimeHandle) -> Result<(), LlamaCppError> {
        // The endpoint first: its secrets are only removed once RunPod has
        // confirmed it is gone (deleted now or already), never before.
        match self.client.delete_endpoint(&handle.id).await {
            Ok(DeleteOutcome::Deleted | DeleteOutcome::AlreadyGone) => {}
            Err(e) => return Err(runpod_err(&e)),
        }
        let mut failed: BTreeMap<String, String> = BTreeMap::new();
        for name in self.secrets_to_delete() {
            match self.delete_secret_by_name(&name).await {
                Ok(()) => self.forget_secret(&name),
                Err(e) => {
                    failed.insert(name, e.to_string());
                }
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(LlamaCppError::Supervisor(format!(
                "RunPod endpoint {} was deleted, but these secrets could not be: {}",
                handle.id,
                failed
                    .iter()
                    .map(|(n, e)| format!("`{n}` ({e})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests;
