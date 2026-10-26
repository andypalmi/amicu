//! A vLLM deployment on Cloud Run: the record `apply` persists before anything
//! is created ([`CloudRunRecord`]), the supervisor `apply` drives
//! ([`CloudRunSupervisor`]) and the teardown `destroy` runs ([`teardown`]).
//!
//! The API key and the Hugging Face token go to Secret Manager, in the
//! service's own region, before the service exists; the service's runtime
//! account reads them as `VLLM_API_KEY` and `HF_TOKEN`. The service is public
//! ([`ServiceAccess::PublicApiKey`]): vLLM's key is the only guard on `/v1/*`,
//! behind Google-managed TLS on the `*.run.app` URL.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use amicu_core::secret::SecretValue;
use amicu_llamacpp::remote::{self, HF_TOKEN_ENV_VAR, RemoteEnv};
use amicu_llamacpp::{LlamaCppError, ProcessSupervisor, ResolvedSecret};
use amicu_providers::{HttpMethod, HttpTransport, ReqwestTransport};
use amicu_runtime::exposure::validate_api_key_value;
use amicu_runtime::{
    EndpointExposure, GatewayAuth, LaunchPlan, LogOptions, LogStream, RuntimeHandle, RuntimeKind,
    RuntimeMetrics, RuntimeStatus,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::health::{HealthTiming, wait_ready};
use super::lifecycle::{
    CloudRunLifecycle, DeleteOutcome, ReadyState, ServiceRecord, ServiceStatus,
};
use super::spec::{SecretRef, ServiceAccess, ServiceSpec, service_name};
use crate::client::{GcpClient, RUN_BASE_URL};
use crate::error::GcpError;
use crate::gce::VLLM_IMAGE_ENV_VAR;
use crate::labels::{OWNER_LABEL, owner_labels, owner_marker};
use crate::operations::PollTiming;
use crate::secrets::{
    Replication, SecretDelete, SecretKind, add_version, create_secret, delete_secret,
    grant_accessor, secret_id,
};
use crate::validate::{validate_project_id, validate_region};

/// What a missing service most likely means.
pub const GONE: &str = "the Cloud Run service is gone: deleted outside amicu";

/// What amicu persists for one deployment's service and secrets. Free of
/// secret values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudRunRecord {
    pub project: String,
    pub region: String,
    pub service: String,
    /// The owner marker the service and its secrets carry.
    pub owner: String,
    /// The service account amicu acted as (for destroy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impersonate: Option<String>,
    /// Ids of the Secret Manager secrets the deployment uses (some may never
    /// have been created).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

impl CloudRunRecord {
    /// The service as the lifecycle functions take it.
    #[must_use]
    pub fn service_record(&self) -> ServiceRecord {
        ServiceRecord {
            project: self.project.clone(),
            region: self.region.clone(),
            service: self.service.clone(),
            owner: self.owner.clone(),
        }
    }
}

/// Everything the service is configured from, resolved by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudRunLaunch {
    /// The Google project.
    pub project: String,
    pub region: String,
    /// amicu's project and deployment ids (names and owner marker).
    pub project_id: String,
    pub deployment_id: String,
    /// `nvidia-l4` or `nvidia-rtx-pro-6000` (or a catalog alias).
    pub gpu: String,
    /// The runtime service account; it reads the secrets.
    pub service_account: String,
    pub max_instances: u32,
    /// Requests one instance serves at once; `None` keeps the spec default.
    pub concurrency: Option<u32>,
    pub impersonate: Option<String>,
}

impl CloudRunLaunch {
    fn owner(&self) -> String {
        owner_marker(None, &self.project_id, &self.deployment_id)
    }

    /// The service id.
    #[must_use]
    pub fn service_name(&self) -> String {
        service_name(&self.project_id, &self.deployment_id)
    }

    /// The write-ahead record, before any request: no backend contact.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] for a bad project, region or name.
    pub fn record(&self) -> Result<CloudRunRecord, GcpError> {
        validate_project_id(&self.project)?;
        validate_region(&self.region)?;
        let service = self.service_name();
        let secrets = [SecretKind::ApiKey, SecretKind::HfToken]
            .into_iter()
            .map(|k| secret_id(&service, k))
            .collect::<Result<_, _>>()?;
        Ok(CloudRunRecord {
            project: self.project.clone(),
            region: self.region.clone(),
            service,
            owner: self.owner(),
            impersonate: self.impersonate.clone(),
            secrets,
        })
    }

    fn spec(&self, record: &CloudRunRecord, startup: &Startup) -> Result<ServiceSpec, GcpError> {
        let key_id = secret_id(&record.service, SecretKind::ApiKey)?;
        let mut spec = ServiceSpec::new(
            &self.project,
            &self.region,
            &self.deployment_id,
            &self.gpu,
            &startup.model,
            SecretRef::new(key_id, "latest"),
            &self.service_account,
        );
        spec.project_id.clone_from(&self.project_id);
        spec.image.clone_from(&startup.image);
        spec.max_model_len = startup.max_model_len;
        spec.extra_args.clone_from(&startup.extra_args);
        if startup.hf_token.is_some() {
            spec.hf_token = Some(SecretRef::new(
                secret_id(&record.service, SecretKind::HfToken)?,
                "latest",
            ));
        }
        spec.max_instances = self.max_instances;
        if let Some(c) = self.concurrency {
            spec.concurrency = c;
        }
        spec.access = ServiceAccess::PublicApiKey;
        Ok(spec)
    }
}

/// Waits and polls.
#[derive(Debug, Clone, Copy, Default)]
pub struct CloudRunTiming {
    pub poll: PollTiming,
    pub health: HealthTiming,
}

/// The container's launch, derived from the plan.
struct Startup {
    image: String,
    model: String,
    max_model_len: u32,
    extra_args: Vec<String>,
    api_key: SecretValue,
    hf_token: Option<SecretValue>,
}

type Sink = Arc<dyn Fn(&str) + Send + Sync>;

/// Starts, inspects and deletes one deployment's Cloud Run service. See the
/// module docs.
pub struct CloudRunSupervisor {
    client: Arc<GcpClient>,
    probe: Option<Arc<dyn HttpTransport>>,
    launch: CloudRunLaunch,
    timing: CloudRunTiming,
    recorded: Option<CloudRunRecord>,
    progress: Option<Sink>,
    created: Arc<AtomicBool>,
}

impl fmt::Debug for CloudRunSupervisor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudRunSupervisor")
            .field("launch", &self.launch)
            .finish_non_exhaustive()
    }
}

fn err(e: &GcpError) -> LlamaCppError {
    let hint = match e {
        GcpError::NotOwned { .. } => "; amicu leaves resources it did not create alone",
        _ => "",
    };
    LlamaCppError::Supervisor(format!("{e}{hint}"))
}

fn unsupported(what: &str) -> LlamaCppError {
    LlamaCppError::Supervisor(format!(
        "{what} are not yet supported for Cloud Run deployments"
    ))
}

impl CloudRunSupervisor {
    #[must_use]
    pub fn new(client: GcpClient, launch: CloudRunLaunch) -> Self {
        Self {
            client: Arc::new(client),
            probe: None,
            launch,
            timing: CloudRunTiming::default(),
            recorded: None,
            progress: None,
            created: Arc::default(),
        }
    }

    /// Send the readiness probe through `transport` (tests inject a mock).
    #[must_use]
    pub fn with_probe_transport(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.probe = Some(transport);
        self
    }

    #[must_use]
    pub fn with_timing(mut self, timing: CloudRunTiming) -> Self {
        self.timing = timing;
        self
    }

    #[must_use]
    pub fn with_progress(mut self, sink: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.progress = Some(Arc::new(sink));
        self
    }

    /// The record already in state: `start` deletes that service first, since
    /// `create` adopts an existing service of ours without changing it.
    #[must_use]
    pub fn with_recorded(mut self, record: CloudRunRecord) -> Self {
        self.recorded = Some(record);
        self
    }

    /// Set once `start` reached its first write (this run may own a service).
    #[must_use]
    pub fn created(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.created)
    }

    fn say(&self, line: &str) {
        if let Some(p) = &self.progress {
            p(line);
        }
    }

    fn lifecycle(&self) -> CloudRunLifecycle {
        CloudRunLifecycle::new(Arc::clone(&self.client)).with_timing(self.timing.poll)
    }

    fn prepare_startup(
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<Startup, LlamaCppError> {
        let amicu_core::resource::Runtime::Vllm(settings) = &plan.settings else {
            return Err(LlamaCppError::Supervisor(
                "Cloud Run deployments run vLLM only".to_owned(),
            ));
        };
        let mut rendered = remote::launch_argv(plan, None, "gcp")?;
        if rendered.starts_with(&["vllm".to_owned(), "serve".to_owned()]) {
            rendered.drain(..2);
        }
        if rendered.first().is_none_or(|a| a.starts_with('-')) {
            return Err(LlamaCppError::Supervisor(
                "the vLLM launch names no model".to_owned(),
            ));
        }
        let model = rendered.remove(0);
        // The service spec sets these itself: Cloud Run routes to a fixed port.
        let mut extra_args = Vec::new();
        let mut it = rendered.into_iter();
        while let Some(a) = it.next() {
            if a == "--host" || a == "--port" || a == "--max-model-len" {
                it.next();
            } else {
                extra_args.push(a);
            }
        }
        let hf_fallback = std::env::var(HF_TOKEN_ENV_VAR).ok();
        let RemoteEnv { plain, mut secret } =
            remote::split_env(plan, secrets, hf_fallback.as_deref())?;
        if !plain.is_empty() {
            return Err(LlamaCppError::Supervisor(
                "the `env` block is not supported on Cloud Run yet".to_owned(),
            ));
        }
        let api_key = secret
            .remove(RuntimeKind::Vllm.api_key_env_var())
            .map(SecretValue::new)
            .ok_or_else(|| {
                LlamaCppError::Supervisor(
                    "Cloud Run deployments need an `api_key`: the service is public and the \
                     key is its only guard"
                        .to_owned(),
                )
            })?;
        validate_api_key_value(&api_key)
            .map_err(|why| LlamaCppError::Supervisor(format!("the deployment's api_key {why}")))?;
        let hf_token = secret.remove(HF_TOKEN_ENV_VAR).map(SecretValue::new);
        if let Some(name) = secret.keys().next() {
            return Err(LlamaCppError::Supervisor(format!(
                "secret '{name}' cannot be delivered to a Cloud Run service yet; only the \
                 `api_key` and `HF_TOKEN` are"
            )));
        }
        Ok(Startup {
            image: std::env::var(VLLM_IMAGE_ENV_VAR)
                .unwrap_or_else(|_| remote::default_image(RuntimeKind::Vllm).to_owned()),
            model,
            max_model_len: settings.context_length,
            extra_args,
            api_key,
            hf_token,
        })
    }

    fn checked(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<(Startup, CloudRunRecord, ServiceSpec, ServiceRecord), LlamaCppError> {
        let startup = Self::prepare_startup(plan, secrets)?;
        let record = self.launch.record().map_err(|e| err(&e))?;
        let spec = self.launch.spec(&record, &startup).map_err(|e| err(&e))?;
        let service = self.lifecycle().prepare(&spec).map_err(|e| err(&e))?;
        if service != record.service_record() {
            return Err(LlamaCppError::Supervisor(format!(
                "the service record {service:?} does not match the write-ahead record"
            )));
        }
        Ok((startup, record, spec, service))
    }

    /// Checks everything `start` checks before its first write, so a bad
    /// launch fails before the record is written.
    ///
    /// # Errors
    /// Whatever `start` would refuse before writing.
    pub fn check_before_write(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<(), LlamaCppError> {
        self.checked(plan, secrets).map(|_| ())
    }

    async fn store_secrets(
        &self,
        record: &CloudRunRecord,
        startup: &Startup,
    ) -> Result<(), LlamaCppError> {
        let l = &self.launch;
        self.say(&format!(
            "storing the service's secrets in Secret Manager (region {})",
            l.region
        ));
        let labels = owner_labels(None, &l.project_id, &l.deployment_id);
        let replication = Replication::Region(l.region.clone());
        let mut items = vec![(SecretKind::ApiKey, &startup.api_key)];
        if let Some(t) = &startup.hf_token {
            items.push((SecretKind::HfToken, t));
        }
        for (kind, value) in items {
            let id = secret_id(&record.service, kind).map_err(|e| err(&e))?;
            async {
                create_secret(
                    &self.client,
                    &l.project,
                    &id,
                    &labels,
                    &record.owner,
                    &replication,
                )
                .await?;
                add_version(&self.client, &l.project, &id, &record.owner, value).await?;
                grant_accessor(
                    &self.client,
                    &l.project,
                    &id,
                    &record.owner,
                    &l.service_account,
                )
                .await
            }
            .await
            .map_err(|e| err(&e))?;
        }
        Ok(())
    }

    /// Polls until the service serves a ready revision. `create` normally
    /// returns that already; a lost or timed-out operation does not.
    async fn wait_serving(
        &self,
        service: &ServiceRecord,
        mut status: ServiceStatus,
    ) -> Result<ServiceStatus, LlamaCppError> {
        let deadline = Instant::now() + self.timing.health.timeout;
        loop {
            if status.is_ready() {
                return Ok(status);
            }
            if let ReadyState::Failed(why) = &status.state {
                return Err(LlamaCppError::Supervisor(format!(
                    "Cloud Run could not start service {}: {why}",
                    service.service
                )));
            }
            if Instant::now() >= deadline {
                return Err(LlamaCppError::Supervisor(format!(
                    "Cloud Run service {} was not ready before the deadline",
                    service.service
                )));
            }
            tokio::time::sleep(self.timing.poll.interval).await;
            status = self
                .lifecycle()
                .status(service)
                .await
                .map_err(|e| err(&e))?
                .ok_or_else(|| {
                    LlamaCppError::Supervisor(format!(
                        "Cloud Run service {} disappeared while it was starting",
                        service.service
                    ))
                })?;
        }
    }
}

#[async_trait]
impl ProcessSupervisor for CloudRunSupervisor {
    async fn start(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        let (startup, record, spec, service) = self.checked(plan, secrets)?;
        if let Some(old) = &self.recorded {
            self.say(&format!("removing the previous service {}", old.service));
            teardown(&self.client, old, self.timing.poll)
                .await
                .map_err(|e| err(&e))?;
        }
        self.created.store(true, Ordering::SeqCst);
        self.store_secrets(&record, &startup).await?;
        self.say(&format!(
            "creating Cloud Run service {} in {} (GPU {}, max {} instance(s), scales to zero); \
             the first start downloads the model and can take many minutes",
            record.service, record.region, self.launch.gpu, spec.max_instances
        ));
        let status = self
            .lifecycle()
            .create(&service, &spec)
            .await
            .map_err(|e| err(&e))?;
        let status = self.wait_serving(&service, status).await?;
        let uri = status.uri.ok_or_else(|| {
            LlamaCppError::Supervisor(format!(
                "Cloud Run reported no URL for service {}",
                record.service
            ))
        })?;
        self.say(&format!("service ready at {uri}; waiting for vLLM"));
        let probe = self
            .probe
            .clone()
            .unwrap_or_else(|| Arc::new(ReqwestTransport::new()));
        let key = validate_api_key_value(&startup.api_key)
            .map_err(|why| LlamaCppError::Supervisor(format!("the deployment's api_key {why}")))?;
        wait_ready(&probe, &uri, key, self.timing.health)
            .await
            .map_err(|e| err(&e))?;
        Ok(RuntimeHandle {
            id: record.service.clone(),
            deployment: plan.deployment.clone(),
            endpoint: uri,
            exposure: EndpointExposure::Public {
                gateway: GatewayAuth::None,
            },
        })
    }

    async fn status(&self, _handle: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError> {
        let record = self.launch.record().map_err(|e| err(&e))?;
        let status = self
            .lifecycle()
            .status(&record.service_record())
            .await
            .map_err(|e| err(&e))?;
        Ok(match status {
            None => RuntimeStatus::Stopped {
                detail: GONE.to_owned(),
            },
            Some(s) => match s.state {
                ReadyState::Ready => RuntimeStatus::Running {
                    detail: "Cloud Run service ready (scales to zero when idle)".to_owned(),
                },
                ReadyState::Pending => RuntimeStatus::Running {
                    detail: "Cloud Run service deploying".to_owned(),
                },
                ReadyState::Failed(why) => RuntimeStatus::Failed { detail: why },
            },
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

    async fn stop(&self, _handle: &RuntimeHandle) -> Result<(), LlamaCppError> {
        let record = self.launch.record().map_err(|e| err(&e))?;
        teardown(&self.client, &record, self.timing.poll)
            .await
            .map(|_| ())
            .map_err(|e| err(&e))
    }
}

/// Deletes the service, then the recorded secrets, each only when it carries
/// the record's owner marker, and confirms each is gone.
/// [`DeleteOutcome::AlreadyGone`] when there was no service. Safe to run
/// again.
///
/// # Errors
/// [`GcpError::NotOwned`] (nothing deleted) for a service or secret of that
/// name that is not ours, or an API error. The secrets are left in place when
/// the service could not be removed.
pub async fn teardown(
    client: &Arc<GcpClient>,
    record: &CloudRunRecord,
    timing: PollTiming,
) -> Result<DeleteOutcome, GcpError> {
    let outcome = CloudRunLifecycle::new(Arc::clone(client))
        .with_timing(timing)
        .delete(&record.service_record())
        .await?;
    for id in &record.secrets {
        delete_secret(client, &record.project, id, &record.owner).await?;
        if delete_secret(client, &record.project, id, &record.owner).await?
            != SecretDelete::AlreadyGone
        {
            return Err(GcpError::Conflict {
                message: format!("secret `{id}` still exists after it was deleted"),
            });
        }
    }
    Ok(outcome)
}

/// The service's status, `None` once it is gone.
///
/// # Errors
/// [`GcpError::NotOwned`] for a service of that name that is not ours, or an
/// API error.
pub async fn service_state(
    client: &Arc<GcpClient>,
    record: &CloudRunRecord,
) -> Result<Option<ServiceStatus>, GcpError> {
    CloudRunLifecycle::new(Arc::clone(client))
        .status(&record.service_record())
        .await
}

/// An amicu-labelled Cloud Run service found in a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedService {
    pub region: String,
    pub name: String,
    pub owner: String,
}

/// The services in `regions` of `project` that carry an owner label
/// (read-only).
///
/// # Errors
/// An API error.
pub async fn list_owned_services(
    client: &GcpClient,
    project: &str,
    regions: &[&str],
) -> Result<Vec<OwnedService>, GcpError> {
    validate_project_id(project)?;
    let mut out = Vec::new();
    for region in regions {
        validate_region(region)?;
        let mut token: Option<String> = None;
        for _ in 0..50 {
            let mut url = format!(
                "{RUN_BASE_URL}/projects/{project}/locations/{region}/services?pageSize=100"
            );
            if let Some(t) = &token {
                url = format!("{url}&pageToken={}", crate::gce::pct(t));
            }
            let page = client.request(HttpMethod::Get, &url, None).await?;
            for s in page["services"].as_array().into_iter().flatten() {
                let name = s["name"].as_str().and_then(|n| n.rsplit('/').next());
                let owner = s["labels"][OWNER_LABEL].as_str();
                if let (Some(name), Some(owner)) = (name, owner) {
                    out.push(OwnedService {
                        region: (*region).to_owned(),
                        name: name.to_owned(),
                        owner: owner.to_owned(),
                    });
                }
            }
            token = page["nextPageToken"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
            if token.is_none() {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> CloudRunLaunch {
        CloudRunLaunch {
            project: "my-proj".into(),
            region: "europe-west4".into(),
            project_id: "aaaa1111".into(),
            deployment_id: "chat".into(),
            gpu: "l4".into(),
            service_account: "run@my-proj.iam.gserviceaccount.com".into(),
            max_instances: 2,
            concurrency: Some(4),
            impersonate: None,
        }
    }

    fn startup(hf: bool) -> Startup {
        Startup {
            image: "vllm/vllm-openai:v0.29.0".into(),
            model: "Qwen/Qwen3-0.6B".into(),
            max_model_len: 4096,
            extra_args: vec!["--enforce-eager".into()],
            api_key: SecretValue::new("sk-test-key-0123456789".to_owned()),
            hf_token: hf.then(|| SecretValue::new("hf_x".to_owned())),
        }
    }

    #[test]
    fn the_record_names_the_service_and_both_secrets_from_amicu_ids() {
        let r = launch().record().unwrap();
        assert_eq!(r.service, service_name("aaaa1111", "chat"));
        assert!(
            r.service.starts_with("amicu-aaaa1111-chat-"),
            "{}",
            r.service
        );
        assert_eq!(r.owner, owner_marker(None, "aaaa1111", "chat"));
        assert_eq!(
            r.secrets,
            ["api-key", "hf-token"].map(|k| format!("{}-{k}", r.service))
        );
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<CloudRunRecord>(&json).unwrap(), r);
    }

    #[test]
    fn the_spec_uses_the_recorded_names_and_the_scaling_limits() {
        let l = launch();
        let r = l.record().unwrap();
        let spec = l.spec(&r, &startup(true)).unwrap();
        assert_eq!(spec.service_name(), r.service);
        assert_eq!(spec.api_key.secret, r.secrets[0]);
        assert_eq!(spec.hf_token.as_ref().unwrap().secret, r.secrets[1]);
        assert_eq!(spec.max_instances, 2);
        assert_eq!(spec.concurrency, 4);
        assert!(!spec.zonal_redundancy);
        let service = CloudRunLifecycle::new(Arc::new(GcpClient::new(Arc::new(NoTokens))))
            .prepare(&spec)
            .unwrap();
        assert_eq!(service, r.service_record());
        let body = spec.create_body(&crate::GcpCatalog::new()).unwrap();
        assert_eq!(body["labels"][OWNER_LABEL], r.owner);
        let spec = l.spec(&r, &startup(false)).unwrap();
        assert!(spec.hf_token.is_none());
    }

    struct NoTokens;

    #[async_trait]
    impl crate::TokenSource for NoTokens {
        async fn token(&self) -> Result<crate::AccessToken, GcpError> {
            Err(GcpError::MissingCredentials)
        }
    }
}
