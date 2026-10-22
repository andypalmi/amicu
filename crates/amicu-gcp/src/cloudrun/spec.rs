//! The service spec and the `services.create` request body.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use amicu_core::naming::project_app_name;

use crate::catalog::GcpCatalog;
use crate::error::GcpError;
use crate::labels::owner_labels;
use crate::validate::{validate_project_id, validate_region, validate_resource_name};

/// The port vLLM listens on and Cloud Run routes to.
pub const CONTAINER_PORT: u16 = 8000;

/// Default image: the vLLM OpenAI server, pinned like the Modal backend's.
pub const DEFAULT_VLLM_IMAGE: &str = "vllm/vllm-openai:v0.29.0";

/// Cloud Run service ids must be shorter than 50 characters
/// (`services.create`, `serviceId`).
const MAX_SERVICE_ID: usize = 49;
/// The longest request timeout Cloud Run accepts.
const MAX_TIMEOUT_SECS: u32 = 3600;
/// Highest per-instance request concurrency.
const MAX_CONCURRENCY: u32 = 1000;
/// Startup probes allow at most this period (`Probe.periodSeconds`).
const PROBE_PERIOD_SECS: u32 = 10;

/// The Cloud Run service id for a deployment: the shared naming scheme, cut to
/// fit Cloud Run's 49 character limit (the digest suffix is kept so names stay
/// unique).
#[must_use]
pub fn service_name(project_id: &str, deployment_id: &str) -> String {
    let full = project_app_name(project_id, deployment_id);
    if full.len() <= MAX_SERVICE_ID {
        return full;
    }
    // `project_app_name` ends in `-<12 hex digits>` and is ASCII.
    let (head, tail) = full.split_at(full.len() - 13);
    let head = head[..MAX_SERVICE_ID - 13].trim_end_matches('-');
    format!("{head}{tail}")
}

/// A Secret Manager secret version to expose as an environment variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    /// `NAME` (same project as the service) or `projects/<p>/secrets/<name>`.
    pub secret: String,
    /// `latest`, a version number or an alias. Pin a number for
    /// reproducible revisions.
    pub version: String,
}

impl SecretRef {
    /// A reference to `secret` at `version`.
    #[must_use]
    pub fn new(secret: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            version: version.into(),
        }
    }

    fn validate(&self, kind: &'static str) -> Result<(), GcpError> {
        let name_ok = |s: &str| {
            !s.is_empty()
                && s.len() <= 255
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        let secret_ok = match self.secret.split('/').collect::<Vec<_>>().as_slice() {
            [name] => name_ok(name),
            ["projects", p, "secrets", name] => validate_project_id(p).is_ok() && name_ok(name),
            _ => false,
        };
        if !secret_ok {
            return Err(invalid(
                kind,
                &self.secret,
                "expected `NAME` or `projects/<project>/secrets/<name>`",
            ));
        }
        if !name_ok(&self.version) {
            return Err(invalid(
                kind,
                &self.version,
                "version must be `latest`, a number or an alias",
            ));
        }
        Ok(())
    }

    fn env(&self, name: &str) -> Value {
        json!({
            "name": name,
            "valueSource": {"secretKeyRef": {"secret": self.secret, "version": self.version}},
        })
    }
}

/// Who may call the service.
///
/// `Service.ingress` is always `INGRESS_TRAFFIC_ALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceAccess {
    /// Sets `invokerIamDisabled: true` on this service, so anyone who can
    /// reach it passes Cloud Run's IAM check and vLLM's API key is the only
    /// authentication. It changes no IAM policy (no `allUsers` binding), and
    /// works under a "domain restricted sharing" org policy. This is an
    /// explicit opt-in: the endpoint is on the public internet.
    #[default]
    PublicApiKey,
    /// Leaves Cloud Run IAM in force (`roles/run.invoker` required). Callers
    /// must send a Google identity token in `Authorization`, which vLLM's
    /// `--api-key` also reads, so the two cannot be combined on one request:
    /// use this only behind a proxy that adds the identity token.
    IamProtected,
}

/// Everything needed to create one vLLM Cloud Run service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    pub project: String,
    pub region: String,
    /// Optional workspace id for the owner marker.
    pub workspace_id: Option<String>,
    pub deployment_id: String,
    /// `nvidia-l4`, `nvidia-rtx-pro-6000`, or any name or alias the catalog
    /// resolves (`l4`).
    pub gpu: String,
    pub image: String,
    /// The Hugging Face model id vLLM loads.
    pub model: String,
    /// The name clients pass as `model`.
    pub served_model_name: String,
    pub max_model_len: u32,
    /// Extra vLLM arguments, appended as given. Must not set `--api-key`
    /// (the key reaches vLLM as `VLLM_API_KEY`, never argv).
    pub extra_args: Vec<String>,
    /// `HF_TOKEN`, for gated models.
    pub hf_token: Option<SecretRef>,
    /// The key vLLM requires, delivered as `VLLM_API_KEY`.
    pub api_key: SecretRef,
    /// The runtime service account email. It needs
    /// `roles/secretmanager.secretAccessor` on the secrets above; amicu does
    /// not grant that.
    pub service_account: String,
    /// `RevisionScaling.maxInstanceCount`: each instance holds one GPU, so
    /// keep it within the region's GPU quota.
    pub max_instances: u32,
    /// `maxInstanceRequestConcurrency`.
    pub concurrency: u32,
    /// Per-request timeout (seconds, at most 3600); long, for streaming.
    pub request_timeout_secs: u32,
    /// How long the startup probe waits for the model to load (seconds).
    pub startup_budget_secs: u32,
    /// CPUs; `None` is the documented minimum for the GPU.
    pub cpu: Option<u32>,
    /// Memory in GiB; `None` is the documented minimum for the GPU.
    pub memory_gib: Option<u32>,
    /// GPU zonal redundancy; off unless asked (it needs more quota and costs
    /// more).
    pub zonal_redundancy: bool,
    pub access: ServiceAccess,
}

impl ServiceSpec {
    /// A spec with defaults: vLLM image, `max_instances = 1`, request
    /// concurrency 8, one hour request timeout, 30 minute startup budget,
    /// minimum CPU and memory, no zonal redundancy, public with API key.
    #[must_use]
    pub fn new(
        project: &str,
        region: &str,
        deployment_id: &str,
        gpu: &str,
        model: &str,
        api_key: SecretRef,
        service_account: &str,
    ) -> Self {
        Self {
            project: project.to_owned(),
            region: region.to_owned(),
            workspace_id: None,
            deployment_id: deployment_id.to_owned(),
            gpu: gpu.to_owned(),
            image: DEFAULT_VLLM_IMAGE.to_owned(),
            model: model.to_owned(),
            served_model_name: model.to_owned(),
            max_model_len: 8192,
            extra_args: Vec::new(),
            hf_token: None,
            api_key,
            service_account: service_account.to_owned(),
            max_instances: 1,
            concurrency: 8,
            request_timeout_secs: MAX_TIMEOUT_SECS,
            startup_budget_secs: 1800,
            cpu: None,
            memory_gib: None,
            zonal_redundancy: false,
            access: ServiceAccess::default(),
        }
    }

    /// The service id (see [`service_name`]).
    #[must_use]
    pub fn service_name(&self) -> String {
        service_name(&self.project, &self.deployment_id)
    }

    /// Checks the spec, including that the region offers the GPU on Cloud Run.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] naming the first bad field.
    pub fn validate(&self, catalog: &GcpCatalog) -> Result<(), GcpError> {
        validate_project_id(&self.project)?;
        validate_region(&self.region)?;
        validate_resource_name(&self.service_name())?;
        let gpu = self.gpu_type(*catalog)?;
        if !catalog
            .cloud_run_regions(gpu)
            .contains(&self.region.as_str())
        {
            return Err(invalid(
                "Cloud Run region",
                &self.region,
                "this GPU is not offered on Cloud Run in that region",
            ));
        }
        if catalog
            .cloud_run_offers()
            .iter()
            .any(|o| o.gpu == gpu && o.region == self.region && o.invitation_only)
        {
            return Err(invalid(
                "Cloud Run region",
                &self.region,
                "this GPU is by invitation only there; contact the Google account team",
            ));
        }
        if let Some((kind, v)) = [
            ("image", &self.image),
            ("model", &self.model),
            ("served model name", &self.served_model_name),
            ("service account", &self.service_account),
        ]
        .into_iter()
        .find(|(_, v)| v.is_empty() || v.chars().any(|c| c.is_control() || c == ' '))
        {
            return Err(invalid(kind, v, "must be non-empty without spaces"));
        }
        if !self.service_account.contains('@') {
            return Err(invalid(
                "service account",
                &self.service_account,
                "expected an email address",
            ));
        }
        if self.extra_args.iter().any(|a| {
            a == "--api-key" || a.starts_with("--api-key=") || a.starts_with("--api-key-file")
        }) {
            return Err(invalid(
                "extra argument",
                "--api-key",
                "the API key is delivered as VLLM_API_KEY, not argv",
            ));
        }
        self.api_key.validate("API key secret")?;
        if let Some(t) = &self.hf_token {
            t.validate("HF_TOKEN secret")?;
        }
        let min = catalog
            .cloud_run_gpu(gpu)
            .ok_or_else(|| invalid("GPU", gpu, "not a Cloud Run GPU"))?;
        if self.cpu.is_some_and(|c| c < min.min_vcpus)
            || self.memory_gib.is_some_and(|m| m < min.min_memory_gib)
        {
            return Err(invalid(
                "Cloud Run instance size",
                gpu,
                "below the documented minimum CPU/memory for this GPU",
            ));
        }
        let bounded = [
            ("max model length", self.max_model_len, 1, u32::MAX),
            ("max instances", self.max_instances, 1, 1000),
            ("concurrency", self.concurrency, 1, MAX_CONCURRENCY),
            (
                "request timeout",
                self.request_timeout_secs,
                1,
                MAX_TIMEOUT_SECS,
            ),
            (
                "startup budget",
                self.startup_budget_secs,
                PROBE_PERIOD_SECS,
                u32::MAX / 2,
            ),
        ];
        for (kind, v, lo, hi) in bounded {
            if v < lo || v > hi {
                return Err(invalid(kind, &v.to_string(), "out of range"));
            }
        }
        Ok(())
    }

    /// The catalog's canonical accelerator name, which is also
    /// `nodeSelector.accelerator`.
    fn gpu_type(&self, catalog: GcpCatalog) -> Result<&'static str, GcpError> {
        catalog
            .cloud_run_gpu(&self.gpu)
            .map(|g| g.gpu)
            .ok_or_else(|| {
                invalid(
                    "Cloud Run GPU",
                    &self.gpu,
                    "Cloud Run offers only nvidia-l4 and nvidia-rtx-pro-6000",
                )
            })
    }

    /// The JSON body for
    /// `POST {RUN_BASE_URL}/projects/<p>/locations/<r>/services?serviceId=<name>`.
    ///
    /// # Errors
    /// As [`validate`](Self::validate).
    pub fn create_body(&self, catalog: &GcpCatalog) -> Result<Value, GcpError> {
        self.validate(catalog)?;
        let gpu = self.gpu_type(*catalog)?;
        let sizing = catalog
            .cloud_run_gpu(gpu)
            .ok_or_else(|| invalid("GPU", gpu, "not a Cloud Run GPU"))?;
        let cpu = self.cpu.unwrap_or(sizing.min_vcpus);
        let mem = self.memory_gib.unwrap_or(sizing.min_memory_gib);

        let mut args: Vec<String> = vec![
            "--model".into(),
            self.model.clone(),
            "--served-model-name".into(),
            self.served_model_name.clone(),
            "--max-model-len".into(),
            self.max_model_len.to_string(),
            "--host".into(),
            "0.0.0.0".into(),
            "--port".into(),
            CONTAINER_PORT.to_string(),
        ];
        args.extend(self.extra_args.iter().cloned());

        let mut env = Vec::new();
        if let Some(t) = &self.hf_token {
            env.push(t.env("HF_TOKEN"));
        }
        env.push(self.api_key.env("VLLM_API_KEY"));

        let failure_threshold = self.startup_budget_secs.div_ceil(PROBE_PERIOD_SECS);
        let labels: BTreeMap<String, String> = owner_labels(
            self.workspace_id.as_deref(),
            &self.project,
            &self.deployment_id,
        );

        Ok(json!({
            "labels": labels,
            "ingress": "INGRESS_TRAFFIC_ALL",
            "invokerIamDisabled": self.access == ServiceAccess::PublicApiKey,
            "template": {
                "serviceAccount": self.service_account,
                "timeout": format!("{}s", self.request_timeout_secs),
                "maxInstanceRequestConcurrency": self.concurrency,
                "scaling": {
                    "minInstanceCount": 0,
                    "maxInstanceCount": self.max_instances,
                },
                "nodeSelector": {"accelerator": gpu},
                "gpuZonalRedundancyDisabled": !self.zonal_redundancy,
                "containers": [{
                    "image": self.image,
                    "args": args,
                    "env": env,
                    "ports": [{"name": "http1", "containerPort": CONTAINER_PORT}],
                    "resources": {
                        "limits": {
                            "cpu": cpu.to_string(),
                            "memory": format!("{mem}Gi"),
                            "nvidia.com/gpu": "1",
                        },
                        // false = CPU always allocated (instance-based billing),
                        // required for GPU.
                        "cpuIdle": false,
                    },
                    "startupProbe": {
                        "httpGet": {"path": "/health", "port": CONTAINER_PORT},
                        "initialDelaySeconds": 0,
                        "periodSeconds": PROBE_PERIOD_SECS,
                        "timeoutSeconds": 5,
                        "failureThreshold": failure_threshold,
                    },
                }],
            },
        }))
    }
}

fn invalid(kind: &'static str, value: &str, reason: &'static str) -> GcpError {
    GcpError::Invalid {
        kind,
        value: value.to_owned(),
        reason,
    }
}
