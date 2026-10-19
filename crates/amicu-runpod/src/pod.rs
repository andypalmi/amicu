//! [`PodSupervisor`]: runs a vLLM / llama.cpp launch as an always-on RunPod
//! GPU Pod. The pod proxy adds no authentication, so the runtime's own API key
//! is required and `start` fails closed unless anonymous requests are refused.
//! `stop` always terminates (a stopped pod still bills for its disk).

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use amicu_core::naming::project_endpoint_name;
use amicu_core::resource::{Accelerator, HardwareProfile, Model, Region};
use amicu_core::secret::SecretValue;
use amicu_llamacpp::remote::{self, GgufSource, HF_TOKEN_ENV_VAR, RemoteEnv};
use amicu_llamacpp::{LlamaCppError, ProcessSupervisor, ResolvedSecret};
use amicu_providers::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport};
use amicu_runtime::exposure::{PROBE_PATH, validate_api_key_value};
use amicu_runtime::{
    EndpointExposure, GatewayAuth, LaunchPlan, LogOptions, LogStream, RuntimeHandle, RuntimeKind,
    RuntimeMetrics, RuntimeStatus,
};
use async_trait::async_trait;

use crate::client::{
    Cloud, CreatePod, CreateSecret, DeleteOutcome, OWNER_ENV_VAR, Pod, PodAction, PodStatus,
    RunPodClient, UpdatePod, pod_proxy_url,
};
use crate::error::RunPodError;
use crate::hardware::{pod_gpu_placement, verify_catalog_memory};
use crate::regions::select_data_centers;
use crate::secrets::{SecretLedger, reference_secrets, stop_result};
use crate::supervisor::{
    DEFAULT_CONTAINER_DISK_GB, HEALTH_PATH, ReadinessTiming, StopOutcome, container_argv,
    container_command, image_min_cuda, owner_marker, runpod_err, runtime_image, runtime_port,
    unsupported,
};

/// Env variables the pod env may not set: amicu's owner marker, and the ones
/// RunPod images read to start sshd (`PUBLIC_KEY`) or Jupyter.
const RESERVED_ENV: [&str; 3] = [OWNER_ENV_VAR, "PUBLIC_KEY", "JUPYTER_PASSWORD"];

/// Everything `start` sends. `Debug` never prints a secret value.
struct Prepared {
    pod: CreatePod,
    secrets: Vec<CreateSecret>,
    api_key: SecretValue,
    port: u16,
}

impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prepared")
            .field("pod", &self.pod)
            .field(
                "secrets",
                &self.secrets.iter().map(|s| &s.name).collect::<Vec<_>>(),
            )
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// Route for the anonymous exposure check. llama-server exempts `/health`
/// and, in some versions, `/v1/models` from `--api-key`; this one is always
/// guarded.
const ANON_PROBE_PATH: &str = "/v1/chat/completions";

/// How long a just (re)started pod may still report its previous container's
/// `EXITED` / `ERROR` before that counts as a failure.
const START_GRACE: Duration = Duration::from_secs(120);

/// The pod `start` deployed to.
struct Rollout {
    id: String,
    expect: Expect,
}

/// Which container readiness may trust.
enum Expect {
    /// Whatever runs (a new pod, or one left as it was).
    Any,
    /// One started after this run (re)started the pod, whose `startedAt` was
    /// this before.
    StartedAfter(Option<String>),
}

/// Why `start` gave up while waiting.
enum NotReady {
    /// Leave the pod in place (recorded) and report.
    Leave(LlamaCppError),
    /// The pod must not keep running: terminate it, then report.
    Unsafe(String),
}

/// Starts, inspects and terminates one deployment's always-on RunPod Pod.
/// See the module docs.
pub struct PodSupervisor {
    client: Arc<RunPodClient>,
    probe: Arc<dyn HttpTransport>,
    workspace_id: Option<String>,
    project_id: String,
    deployment_id: String,
    pod_name: String,
    gpu_type_id: Option<String>,
    gpu_count: u32,
    gpu_vram_gb: Option<u32>,
    cloud: Cloud,
    data_center_ids: Vec<String>,
    disk_gb: u32,
    gguf: Option<GgufSource>,
    readiness: ReadinessTiming,
    start_grace: Duration,
    confirmed_account: bool,
    recorded_pod: Option<String>,
    recorded_secrets: Vec<String>,
    deployed_pod: Arc<Mutex<Option<String>>>,
    secrets: SecretLedger,
}

impl fmt::Debug for PodSupervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PodSupervisor")
            .field("client", &self.client)
            .field("pod_name", &self.pod_name)
            .field("gpu_type_id", &self.gpu_type_id)
            .field("gpu_count", &self.gpu_count)
            .field("cloud", &self.cloud)
            .field("data_center_ids", &self.data_center_ids)
            .finish_non_exhaustive()
    }
}

impl PodSupervisor {
    /// A supervisor for `deployment_id` in `project_id`; the pod is named
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
            pod_name: project_endpoint_name(project_id, deployment_id),
            gpu_type_id: None,
            gpu_count: 1,
            gpu_vram_gb: None,
            cloud: Cloud::Secure,
            data_center_ids: Vec::new(),
            disk_gb: DEFAULT_CONTAINER_DISK_GB,
            gguf: None,
            readiness: ReadinessTiming::DEFAULT,
            start_grace: START_GRACE,
            confirmed_account: false,
            recorded_pod: None,
            recorded_secrets: Vec::new(),
            deployed_pod: Arc::default(),
        }
    }

    /// Probe the pod through `transport` instead (tests inject a mock).
    #[must_use]
    pub fn with_probe_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.probe = transport;
        self
    }

    /// The workspace the deployment belongs to (part of the owner marker and
    /// secret names; see [`crate::supervisor::RunPodSupervisor::with_workspace`]).
    #[must_use]
    pub fn with_workspace(mut self, workspace_id: &str) -> Self {
        self.workspace_id = Some(workspace_id.to_owned());
        self
    }

    /// The GPU from a hardware profile's accelerator, resolved through the
    /// shared catalog to RunPod's exact type id (pods-only cards allowed).
    /// `None` leaves `start` failing with a clear error.
    ///
    /// # Errors
    /// [`LlamaCppError::Supervisor`] if the accelerator is not a GPU, has a
    /// zero count, or has no RunPod type.
    pub fn with_accelerator(
        mut self,
        accelerator: Option<&Accelerator>,
    ) -> Result<Self, LlamaCppError> {
        let placement = accelerator
            .map(pod_gpu_placement)
            .transpose()
            .map_err(|e| runpod_err(&e))?;
        if let Some(p) = placement {
            self.gpu_type_id = Some(p.type_id);
            self.gpu_count = p.count;
            self.gpu_vram_gb = Some(p.vram_gb);
        } else {
            self.gpu_type_id = None;
        }
        Ok(self)
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

    /// Cloud tier (default [`Cloud::Secure`]).
    #[must_use]
    pub fn with_cloud(mut self, cloud: Cloud) -> Self {
        self.cloud = cloud;
        self
    }

    /// Restricts placement to the data centers of `region`, enforcing its
    /// residency; `None` lets RunPod choose.
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
        Ok(self)
    }

    /// The Hugging Face GGUF a llama.cpp launch downloads. A no-op for vLLM.
    #[must_use]
    pub fn with_gguf_model(mut self, model: &Model) -> Self {
        self.gguf = GgufSource::for_model(model);
        self
    }

    /// Container disk in GB (default [`DEFAULT_CONTAINER_DISK_GB`]).
    #[must_use]
    pub fn with_container_disk_gb(mut self, gb: u32) -> Self {
        self.disk_gb = gb;
        self
    }

    /// Readiness and exposure-check polling bounds.
    #[must_use]
    pub fn with_readiness_timing(mut self, timing: ReadinessTiming) -> Self {
        self.readiness = timing;
        self
    }

    /// Whether these credentials are confirmed to act in the account recorded
    /// in state; only then does a `404` on `stop` release the secrets.
    #[must_use]
    pub fn with_confirmed_account(mut self, confirmed: bool) -> Self {
        self.confirmed_account = confirmed;
        self
    }

    /// The pod id recorded in state; `start` updates it in place instead of
    /// relying on a name match.
    #[must_use]
    pub fn with_recorded_pod(mut self, id: &str) -> Self {
        self.recorded_pod = Some(id.to_owned());
        self
    }

    /// Secret names recorded in state for this deployment.
    #[must_use]
    pub fn with_recorded_secrets(mut self, names: Vec<String>) -> Self {
        self.recorded_secrets = names;
        self
    }

    /// The pod name `start` creates or updates.
    #[must_use]
    pub fn pod_name(&self) -> &str {
        &self.pod_name
    }

    /// The owner marker this deployment's pod carries.
    #[must_use]
    pub fn owner(&self) -> String {
        owner_marker(
            self.workspace_id.as_deref(),
            &self.project_id,
            &self.deployment_id,
        )
    }

    /// The management client.
    #[must_use]
    pub fn client(&self) -> &RunPodClient {
        &self.client
    }

    /// Holds the pod id as soon as RunPod has one for this deployment, kept
    /// when `start` then fails; cleared once a fail-closed teardown is
    /// confirmed.
    #[must_use]
    pub fn deployed_pod(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.deployed_pod)
    }

    /// Names of the RunPod secrets that may exist for this deployment as far
    /// as this instance knows.
    #[must_use]
    pub fn created_secrets(&self) -> Arc<Mutex<Vec<String>>> {
        self.secrets.shared()
    }

    /// The exact secret names a `start` with `plan` and `secrets` writes
    /// (the api_key's included), for recording before the call.
    ///
    /// # Errors
    /// Whatever `start` refuses before its first call.
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

    /// The argv the pod runs for `plan`.
    ///
    /// # Errors
    /// See [`amicu_llamacpp::remote::launch_argv`].
    pub fn launch_argv(&self, plan: &LaunchPlan) -> Result<Vec<String>, LlamaCppError> {
        container_argv(plan, self.gguf.as_ref())
    }

    /// Why `pod` cannot be updated in place to this configuration (GPU type
    /// or count, cloud, or a data center outside the region), or `None`.
    #[must_use]
    pub fn replacement_reason(&self, pod: &Pod) -> Option<String> {
        if let (Some(want), Some(have)) = (&self.gpu_type_id, &pod.gpu) {
            if have.id != *want {
                return Some(format!("GPU type is `{}`, wanted `{want}`", have.id));
            }
            if let Some(count) = have.count
                && count != self.gpu_count
            {
                return Some(format!("GPU count is {count}, wanted {}", self.gpu_count));
            }
        }
        if let Some(cloud) = pod.cloud
            && cloud != Cloud::Unknown
            && cloud != self.cloud
        {
            return Some(format!(
                "cloud is {}, wanted {}",
                cloud.wire(),
                self.cloud.wire()
            ));
        }
        self.residency_violation(pod)
    }

    fn residency_violation(&self, pod: &Pod) -> Option<String> {
        let dc = pod.data_center_id.as_deref()?;
        (!self.data_center_ids.is_empty() && !self.data_center_ids.iter().any(|d| d == dc)).then(
            || {
                format!(
                    "placed in data center {dc}, outside the region's {}",
                    self.data_center_ids.join(", ")
                )
            },
        )
    }

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

        let key_var = runtime.api_key_env_var();
        let key_vars = [
            RuntimeKind::Vllm.api_key_env_var(),
            RuntimeKind::LlamaCpp.api_key_env_var(),
        ];
        if let Some(k) = key_vars.iter().find(|k| plain.contains_key(**k)) {
            return Err(LlamaCppError::Supervisor(format!(
                "env var '{k}' cannot be set in the `env` block of a RunPod pod (it would be \
                 stored in plain text); set the deployment's `api_key` instead"
            )));
        }
        let api_key = SecretValue::new(secret.get(key_var).cloned().ok_or_else(|| {
            LlamaCppError::Supervisor(
                "RunPod pods need an `api_key`: the pod proxy adds no authentication, so the \
                 runtime's own key is the only guard"
                    .to_owned(),
            )
        })?);
        validate_api_key_value(&api_key)
            .map_err(|why| LlamaCppError::Supervisor(format!("the deployment's api_key {why}")))?;
        for k in key_vars.iter().filter(|k| **k != key_var) {
            secret.remove(*k);
        }
        if let Some(k) = RESERVED_ENV
            .iter()
            .find(|k| plain.contains_key(**k) || secret.contains_key(**k))
        {
            return Err(LlamaCppError::Supervisor(format!(
                "env var '{k}' is reserved on a RunPod pod; remove it from the deployment's \
                 `env` block"
            )));
        }
        let gpu_type_id = self.gpu_type_id.clone().ok_or_else(|| {
            LlamaCppError::Supervisor(
                "a RunPod pod needs a GPU accelerator in the deployment's hardware profile"
                    .to_owned(),
            )
        })?;

        let owner = self.owner();
        let mut env = plain;
        env.insert(OWNER_ENV_VAR.to_owned(), owner.clone());
        let create_secrets = reference_secrets(
            &self.project_id,
            &self.deployment_id,
            &owner,
            &secret,
            &mut env,
        );
        let (entrypoint, cmd) = container_command(&argv)?;

        let mut pod = CreatePod::new(self.pod_name.clone(), image, gpu_type_id, self.gpu_count);
        pod.entrypoint = entrypoint;
        pod.cmd = cmd;
        pod.env = env;
        pod.ports = vec![format!("{port}/http")];
        pod.disk_gb = Some(self.disk_gb);
        pod.min_cuda_version = image_min_cuda(image, runtime);
        pod.cloud = self.cloud;
        pod.data_center_ids.clone_from(&self.data_center_ids);
        Ok(Prepared {
            pod,
            secrets: create_secrets,
            api_key,
            port,
        })
    }

    /// Before anything is written: the live catalog must list the type id
    /// with the card's memory, so a renamed or repurposed id stops the deploy.
    async fn check_catalog(&self, type_id: &str) -> Result<(), LlamaCppError> {
        let catalog = self
            .client
            .get_gpu_catalog()
            .await
            .map_err(|e| runpod_err(&e))?;
        match self.gpu_vram_gb {
            Some(vram) => verify_catalog_memory(type_id, vram, &catalog),
            None if catalog.iter().any(|g| g.id == type_id) => Ok(()),
            None => Err(RunPodError::InvalidGpu(format!(
                "RunPod's GPU catalog does not list `{type_id}`"
            ))),
        }
        .map_err(|e| runpod_err(&e))
    }

    fn set_deployed(&self, id: Option<&str>) {
        if let Ok(mut slot) = self.deployed_pod.lock() {
            *slot = id.map(str::to_owned);
        }
    }

    fn check_owner(&self, pod: &Pod) -> Result<(), LlamaCppError> {
        let owner = self.owner();
        let why = match pod.env.owner.as_deref() {
            Some(o) if o == owner => return Ok(()),
            Some(_) => format!("its {OWNER_ENV_VAR} marker belongs to another amicu deployment"),
            None => format!("it carries no {OWNER_ENV_VAR} marker for this deployment"),
        };
        Err(LlamaCppError::Supervisor(format!(
            "RunPod pod {} is named `{}` but {why}; refusing to adopt it. Terminate or rename \
             it, or record its id if it is this deployment's",
            pod.id, pod.name
        )))
    }

    /// Brings the existing pod (as just read) to `prepared`: PATCHed only when
    /// its config differs, then restarted (or started, if stopped) so the new
    /// container is what readiness sees. A refused write drops the secrets
    /// this run created.
    async fn update_existing(
        &self,
        pod: &Pod,
        prepared: &Prepared,
        fresh: &[String],
    ) -> Result<Rollout, LlamaCppError> {
        self.set_deployed(Some(&pod.id));
        if let Some(why) = self.replacement_reason(pod) {
            self.secrets.discard(fresh).await;
            return Err(LlamaCppError::Supervisor(format!(
                "RunPod pod {} must be replaced, not updated: {why}; stop it first",
                pod.id
            )));
        }
        let changed = !pod_matches(pod, &prepared.pod);
        let current = if changed {
            match self
                .client
                .update_pod(&pod.id, &update_from(&prepared.pod))
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    if !matches!(e, RunPodError::Transport(_)) {
                        self.secrets.discard(fresh).await;
                    }
                    return Err(runpod_err(&e));
                }
            }
        } else {
            pod.clone()
        };
        // A PATCH resets the container but leaves `startedAt` unchanged, while
        // `restart` moves it at once: restart explicitly so readiness can tell
        // the new container from the old one.
        let action = match current.status {
            PodStatus::Exited | PodStatus::Error => Some(PodAction::Start),
            _ if changed => Some(PodAction::Restart),
            _ => None,
        };
        if let Some(action) = action {
            self.pod_action_when_allowed(&pod.id, action).await?;
        }
        Ok(Rollout {
            id: pod.id.clone(),
            expect: if changed || action.is_some() {
                Expect::StartedAfter(pod.started_at.clone())
            } else {
                Expect::Any
            },
        })
    }

    /// Sends `action`, retrying while RunPod answers `409` (e.g. a `restart`
    /// while the PATCH's own reset is still in progress).
    async fn pod_action_when_allowed(
        &self,
        id: &str,
        action: PodAction,
    ) -> Result<(), LlamaCppError> {
        let deadline = tokio::time::Instant::now() + self.readiness.timeout;
        let mut backoff = self.readiness.initial_backoff;
        loop {
            match self.client.pod_action(id, action).await {
                Ok(_) => return Ok(()),
                Err(RunPodError::Conflict { .. }) if self.pause(deadline, &mut backoff).await => {}
                Err(e) => return Err(runpod_err(&e)),
            }
        }
    }

    async fn upsert_pod(
        &self,
        prepared: &Prepared,
        fresh: &[String],
    ) -> Result<Rollout, LlamaCppError> {
        if let Some(id) = &self.recorded_pod {
            match self.client.get_pod(id).await {
                Ok(pod) if pod.status != PodStatus::Terminated => {
                    if pod.env.owner.is_some() {
                        self.check_owner(&pod)?;
                    }
                    return self.update_existing(&pod, prepared, fresh).await;
                }
                Ok(_) | Err(RunPodError::NotFound { .. }) => {}
                Err(e) => return Err(runpod_err(&e)),
            }
        }
        let name = &prepared.pod.name;
        let existing = self
            .client
            .find_pods_by_name(name)
            .await
            .map_err(|e| runpod_err(&e))?;
        match existing.as_slice() {
            [] => {}
            [one] => {
                let pod = self
                    .client
                    .get_pod(&one.id)
                    .await
                    .map_err(|e| runpod_err(&e))?;
                self.check_owner(&pod)?;
                return self.update_existing(&pod, prepared, fresh).await;
            }
            many => return Err(duplicate_err(name, many)),
        }
        let id = match self.client.create_pod(&prepared.pod).await {
            Ok(created) => {
                self.set_deployed(Some(&created.id));
                Ok(created.id)
            }
            Err(RunPodError::AmbiguousCreate(detail)) => {
                self.recover_ambiguous(name, &detail).await
            }
            Err(e) => {
                self.secrets.discard(fresh).await;
                Err(runpod_err(&e))
            }
        }?;
        Ok(Rollout {
            id,
            expect: Expect::Any,
        })
    }

    /// The create's response was lost: look the pod up by name (with the
    /// owner check); never send the create again.
    async fn recover_ambiguous(&self, name: &str, detail: &str) -> Result<String, LlamaCppError> {
        let lookup_err = |e: &RunPodError| {
            LlamaCppError::Supervisor(format!(
                "RunPod pod create outcome unknown ({detail}) and looking up pod `{name}` failed \
                 ({e}); look it up by name before retrying"
            ))
        };
        let found = self
            .client
            .find_pods_by_name(name)
            .await
            .map_err(|e| lookup_err(&e))?;
        match found.as_slice() {
            [one] => {
                let pod = self
                    .client
                    .get_pod(&one.id)
                    .await
                    .map_err(|e| lookup_err(&e))?;
                self.check_owner(&pod)?;
                self.set_deployed(Some(&pod.id));
                Ok(pod.id)
            }
            [] => Err(LlamaCppError::Supervisor(format!(
                "RunPod pod create outcome unknown ({detail}) and no pod named `{name}` exists \
                 yet; it was not retried - run the command again to create it"
            ))),
            many => Err(duplicate_err(name, many)),
        }
    }

    async fn pause(&self, deadline: tokio::time::Instant, backoff: &mut Duration) -> bool {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep((*backoff).min(deadline - now)).await;
        *backoff = (*backoff * 2).min(self.readiness.max_backoff);
        true
    }

    /// Waits for `RUNNING` of the container this run deployed, then for
    /// `/health` on the proxy (public on both runtimes) to answer `200`.
    async fn wait_ready(&self, rollout: &Rollout, url: &str) -> Result<(), NotReady> {
        let id = rollout.id.as_str();
        let health = format!("{url}{HEALTH_PATH}");
        let started = tokio::time::Instant::now();
        let deadline = started + self.readiness.timeout;
        let grace_end = started + self.start_grace;
        let mut backoff = self.readiness.initial_backoff;
        let mut last = "no answer".to_owned();
        loop {
            match self.client.get_pod(id).await {
                Ok(pod) => {
                    if let Some(why) = self.residency_violation(&pod) {
                        return Err(NotReady::Unsafe(format!("RunPod pod {id} was {why}")));
                    }
                    let current = match &rollout.expect {
                        Expect::Any => true,
                        Expect::StartedAfter(before) => {
                            pod.started_at.is_some() && pod.started_at != *before
                        }
                    };
                    match pod.status {
                        PodStatus::Terminated => {
                            return Err(NotReady::Leave(LlamaCppError::Supervisor(format!(
                                "RunPod pod {id} was terminated while starting"
                            ))));
                        }
                        PodStatus::Error | PodStatus::Exited
                            if current || tokio::time::Instant::now() >= grace_end =>
                        {
                            return Err(NotReady::Leave(LlamaCppError::Supervisor(format!(
                                "RunPod pod {id} is {} instead of starting; it is left in place",
                                pod.status.as_str()
                            ))));
                        }
                        PodStatus::Running if !current => {
                            "RUNNING (previous container)".clone_into(&mut last);
                        }
                        PodStatus::Running => {
                            let req = HttpRequest::new(HttpMethod::Get, health.clone());
                            match self.probe.execute(req).await {
                                Ok(r) if r.status == 200 => return Ok(()),
                                Ok(r) => {
                                    last = format!("RUNNING, /health answered HTTP {}", r.status);
                                }
                                Err(e) => last = format!("RUNNING, /health unreachable ({e})"),
                            }
                        }
                        s => s.as_str().clone_into(&mut last),
                    }
                }
                Err(RunPodError::NotFound { .. }) => {
                    return Err(NotReady::Leave(LlamaCppError::Supervisor(format!(
                        "RunPod pod {id} disappeared while starting"
                    ))));
                }
                Err(e) if e.is_retryable() => last = e.to_string(),
                Err(e) => return Err(NotReady::Leave(runpod_err(&e))),
            }
            if !self.pause(deadline, &mut backoff).await {
                return Err(NotReady::Leave(LlamaCppError::Supervisor(format!(
                    "RunPod pod {id} did not become ready within {}s (last: {last}); it is left \
                     in place and keeps billing",
                    self.readiness.timeout.as_secs()
                ))));
            }
        }
    }

    /// An anonymous [`ANON_PROBE_PATH`] request must be refused (else
    /// [`NotReady::Unsafe`]), then the key must be accepted on [`PROBE_PATH`].
    async fn verify_exposure(&self, url: &str, key: &SecretValue) -> Result<(), NotReady> {
        let anon_url = format!("{url}{ANON_PROBE_PATH}");
        let probe_url = format!("{url}{PROBE_PATH}");
        let deadline = tokio::time::Instant::now() + self.readiness.timeout;
        let mut backoff = self.readiness.initial_backoff;
        loop {
            let req = HttpRequest::new(HttpMethod::Post, anon_url.clone())
                .with_json(serde_json::json!({}));
            match self.probe.execute(req).await {
                // Only the runtime's own 401; a 403 may come from a proxy in front of it.
                Ok(r) if r.status == 401 => break,
                // Reached the handler: success, or the empty body's validation error.
                Ok(r) if (200..300).contains(&r.status) || matches!(r.status, 400 | 422) => {
                    return Err(NotReady::Unsafe(format!(
                        "the pod answered an unauthenticated request to {ANON_PROBE_PATH} \
                         (HTTP {}); it was open to anyone with its URL",
                        r.status
                    )));
                }
                _ => {}
            }
            if !self.pause(deadline, &mut backoff).await {
                return Err(NotReady::Unsafe(format!(
                    "could not verify that the pod rejects unauthenticated requests to \
                     {ANON_PROBE_PATH} (no 401 answer)"
                )));
            }
        }
        let key = key.expose_secret_str().map_err(|_| {
            NotReady::Leave(LlamaCppError::Supervisor(
                "the deployment's api_key is not valid UTF-8".to_owned(),
            ))
        })?;
        let mut backoff = self.readiness.initial_backoff;
        loop {
            let req = HttpRequest::new(HttpMethod::Get, probe_url.clone()).with_bearer(key);
            match self.probe.execute(req).await {
                Ok(r) if (200..300).contains(&r.status) => return Ok(()),
                Ok(r) if r.status == 401 => {
                    return Err(NotReady::Leave(LlamaCppError::Supervisor(format!(
                        "the pod rejected the deployment's api_key on {PROBE_PATH} (HTTP {}); \
                         the key delivered to the runtime does not match",
                        r.status
                    ))));
                }
                _ => {}
            }
            if !self.pause(deadline, &mut backoff).await {
                return Err(NotReady::Leave(LlamaCppError::Supervisor(format!(
                    "could not verify that the pod accepts the api_key on {PROBE_PATH} before \
                     the deadline; it is left in place"
                ))));
            }
        }
    }

    /// Fail closed: terminate the pod, saying honestly whether that worked.
    async fn teardown(&self, id: &str, detail: &str) -> LlamaCppError {
        match self.stop_pod_id(id).await {
            Ok(_) => LlamaCppError::Supervisor(format!("{detail}; pod {id} was terminated")),
            Err(e) => LlamaCppError::Supervisor(format!(
                "{detail}; terminating pod {id} FAILED ({e}): terminate it by hand"
            )),
        }
    }

    /// [`ProcessSupervisor::stop`] with its outcome: terminates the pod
    /// (never just stops it), then deletes every known secret, unless RunPod
    /// answered `404` and the account is not
    /// [confirmed](Self::with_confirmed_account).
    ///
    /// # Errors
    /// [`LlamaCppError::Supervisor`] when the terminate is not confirmed (no
    /// secret is touched) or a secret could not be deleted.
    pub async fn stop_pod(&self, handle: &RuntimeHandle) -> Result<StopOutcome, LlamaCppError> {
        self.stop_pod_id(&handle.id).await
    }

    async fn stop_pod_id(&self, id: &str) -> Result<StopOutcome, LlamaCppError> {
        let outcome = match self.client.terminate_pod(id).await {
            Ok(o) => o,
            Err(RunPodError::Conflict { detail }) => match self.client.get_pod(id).await {
                // Seen by id in this account: confirmed gone.
                Ok(pod) if pod.status == PodStatus::Terminated => DeleteOutcome::Deleted,
                Err(RunPodError::NotFound { .. }) => DeleteOutcome::AlreadyGone,
                Ok(pod) => {
                    return Err(LlamaCppError::Supervisor(format!(
                        "RunPod refused to terminate pod {id} ({}): {detail}",
                        pod.status.as_str()
                    )));
                }
                Err(e) => return Err(runpod_err(&e)),
            },
            Err(e) => return Err(runpod_err(&e)),
        };
        for name in &self.recorded_secrets {
            self.secrets.note(name);
        }
        let result = self
            .secrets
            .finish_stop("pod", id, outcome, self.confirmed_account)
            .await;
        if self
            .deployed_pod
            .lock()
            .is_ok_and(|s| s.as_deref() == Some(id))
        {
            self.set_deployed(None);
        }
        result
    }
}

/// The PATCH that makes an existing pod match `create` (GPU, cloud and
/// placement cannot be patched).
fn update_from(create: &CreatePod) -> UpdatePod {
    UpdatePod {
        name: None,
        image: Some(create.image.clone()),
        entrypoint: Some(create.entrypoint.clone()),
        cmd: Some(create.cmd.clone()),
        env: Some(create.env.clone()),
        ports: Some(create.ports.clone()),
        disk_gb: create.disk_gb,
        registry_id: None,
    }
}

/// Whether `pod` already runs `want`'s container config (env compared as
/// written, secret references included).
fn pod_matches(pod: &Pod, want: &CreatePod) -> bool {
    pod.image.as_deref() == Some(want.image.as_str())
        && pod.entrypoint == want.entrypoint
        && pod.cmd == want.cmd
        && pod.ports == want.ports
        && (want.disk_gb.is_none() || pod.disk == want.disk_gb)
        && pod.env.matches(&want.env)
}

fn duplicate_err(name: &str, pods: &[Pod]) -> LlamaCppError {
    LlamaCppError::Supervisor(format!(
        "several RunPod pods are named `{name}` ({}); terminate the extra ones first",
        pods.iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[async_trait]
impl ProcessSupervisor for PodSupervisor {
    async fn start(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        let image = runtime_image(RuntimeKind::of(&plan.settings));
        let hf_token_fallback = std::env::var(HF_TOKEN_ENV_VAR).ok();
        let prepared = self.prepare(plan, secrets, &image, hf_token_fallback.as_deref())?;
        self.check_catalog(&prepared.pod.gpu_type_id).await?;
        for name in &self.recorded_secrets {
            self.secrets.note(name);
        }
        let mut fresh = Vec::new();
        for secret in &prepared.secrets {
            if self.secrets.ensure(secret).await? {
                fresh.push(secret.name.clone());
            }
        }
        let rollout = self.upsert_pod(&prepared, &fresh).await?;
        self.secrets.retire_stale(&prepared.secrets).await;

        let url = pod_proxy_url(&rollout.id, prepared.port);
        let checked = match self.wait_ready(&rollout, &url).await {
            Ok(()) => self.verify_exposure(&url, &prepared.api_key).await,
            Err(e) => Err(e),
        };
        let id = rollout.id;
        match checked {
            Ok(()) => Ok(RuntimeHandle {
                endpoint: url,
                id,
                deployment: plan.deployment.clone(),
                exposure: EndpointExposure::Public {
                    gateway: GatewayAuth::None,
                },
            }),
            Err(NotReady::Leave(e)) => Err(e),
            Err(NotReady::Unsafe(detail)) => Err(self.teardown(&id, &detail).await),
        }
    }

    async fn status(&self, handle: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError> {
        let pod = match self.client.get_pod(&handle.id).await {
            Ok(p) => p,
            Err(RunPodError::NotFound { .. }) => {
                return Ok(RuntimeStatus::Stopped {
                    detail: "RunPod pod not found".to_owned(),
                });
            }
            Err(e) => return Err(runpod_err(&e)),
        };
        let status = pod.status.as_str();
        Ok(match pod.status {
            PodStatus::Running => RuntimeStatus::Running {
                detail: match pod.cost {
                    Some(c) => format!("pod running · ${c:.2}/hr"),
                    None => "pod running".to_owned(),
                },
            },
            PodStatus::Provisioning | PodStatus::Starting | PodStatus::Unknown => {
                RuntimeStatus::Running {
                    detail: format!("pod {status}"),
                }
            }
            PodStatus::Exited | PodStatus::Terminated => RuntimeStatus::Stopped {
                detail: format!("pod {status}"),
            },
            PodStatus::Error => RuntimeStatus::Failed {
                detail: format!("pod {status}"),
            },
        })
    }

    // TODO: Pod logs come from `GET /pods/{id}/logs` and metrics through the
    // Pod proxy URL, not the serverless helpers in `logs`/`metrics`.
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
        stop_result("pod", &handle.id, self.stop_pod(handle).await?)
    }
}

#[cfg(test)]
mod tests;
