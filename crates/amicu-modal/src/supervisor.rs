//! Bridges vLLM's local-launch renderer onto Modal, so `amicu apply` can
//! start a `provider = "modal"` deployment through the same
//! `VllmAdapter`-shaped seam it uses for a local process, without
//! `amicu-vllm` (or `amicu-engine`, which does not depend on this crate at
//! all - see the packaging note on [`crate`]) ever knowing Modal exists.
//!
//! [`ModalSupervisor`] implements [`amicu_llamacpp::ProcessSupervisor`] - the
//! same trait [`amicu_llamacpp::host_supervisor`] returns for a local
//! launchd/systemd process, and the trait `amicu-vllm`'s `VllmAdapter` is
//! already generic over (`Box<dyn ProcessSupervisor>`). So `amicu-cli`'s
//! `apply` command selects a supervisor *per deployment* - this one when the
//! hardware profile has `provider = "modal"`, `host_supervisor()` otherwise -
//! and builds a `VllmAdapter` over whichever applies, rather than either
//! crate branching on provider internally.
//!
//! No bridging runtime is needed to implement the trait: `ProcessSupervisor`
//! is a genuine `#[async_trait]`, and `amicu-cli`'s `apply` command is itself
//! driven from an async fn (`block_on` at the top, not inside this call), so
//! [`ModalSupervisor`]'s methods call [`ModalClient`]'s async RPCs directly.
//!
//! Mapping a vLLM launch onto a Modal [`ServerSpec`] (see each item's own doc
//! comment for the reasoning): `argv` is `RenderedLaunch::Command`'s, with
//! `--host 127.0.0.1` rewritten to `0.0.0.0` ([`bind_all_interfaces`] - Modal
//! reaches the container over its internal network, never loopback); the
//! model reference must be a Hugging Face repo id, not a local path Modal's
//! container cannot see ([`reject_local_path_model`]); `gpu` is the hardware
//! profile's [`Accelerator`] rendered as Modal's `"<MODEL>:<count>"` string
//! ([`gpu_string`] - a **hand-maintained coupling**, not validated against
//! `amicu-providers::modal`'s GPU catalog, which is not on this crate's
//! dependency graph); `image` is [`DEFAULT_VLLM_IMAGE`], overridable via
//! [`VLLM_IMAGE_ENV_VAR`] (no HCL schema seam exists for it yet); `secret_env`
//! carries only resolved [`LaunchPlan::secret_refs`] (never a plain
//! `vllm { env = {...} }` entry - those become baked-in image env) plus the
//! [`HF_TOKEN_ENV_VAR`] fallback documented on that constant.

use std::collections::BTreeMap;
use std::time::Duration;

use amicu_core::resource::{Accelerator, AcceleratorKind, HardwareProfile, Runtime};
use amicu_llamacpp::{LlamaCppError, ProcessSupervisor, ResolvedSecret};
use amicu_runtime::{
    LaunchPlan, LogOptions, LogStream, RuntimeHandle, RuntimeMetrics, RuntimeStatus,
};
use async_trait::async_trait;

use crate::client::ModalClient;
use crate::credentials::Credentials;
use crate::deploy::ServerSpec;
use crate::error::ModalError;
use crate::image::ImageSpec;

/// The vLLM base image `ModalSupervisor::start` builds unless overridden by
/// [`VLLM_IMAGE_ENV_VAR`]. Must already contain Python and vLLM on `PATH`
/// (see `crate::image::ImageSpec::base_registry_image`'s doc comment) - the
/// official `vllm/vllm-openai` tags do.
pub const DEFAULT_VLLM_IMAGE: &str = "vllm/vllm-openai:v0.29.0";

/// Environment variable overriding [`DEFAULT_VLLM_IMAGE`]. There is no HCL
/// schema seam for this today (see the module docs), so this is the entire
/// override surface. Read fresh by [`ProcessSupervisor::start`] on every
/// call, not cached - the supervisor itself is stateless between calls, so
/// there is nothing to invalidate if the value changes between deploys.
pub const VLLM_IMAGE_ENV_VAR: &str = "AMICU_MODAL_VLLM_IMAGE";

/// Environment variable [`ModalSupervisor::start`] reads a Hugging Face
/// token from when the launch plan carries no resolved `HF_TOKEN` secret.
/// Read from the *operator's local* process environment - never from the
/// deployment's own `vllm { env = {...} }` block - and forwarded only
/// through Modal's Secret mechanism (never baked into the image). Like
/// [`VLLM_IMAGE_ENV_VAR`], this is read fresh on every `start()` call, not
/// cached.
pub const HF_TOKEN_ENV_VAR: &str = "HF_TOKEN";

/// Persistent Modal Volumes every Modal-deployed vLLM server mounts, as
/// `(volume name, container mount path)` pairs. Created if missing
/// (`crate::deploy`'s `VolumeGetOrCreate` call is always
/// `CreateIfMissing`), and shared across every amicu deployment on the same
/// Modal workspace - repeated deploys of the same or a different model reuse
/// the same warmed cache rather than re-downloading weights per app.
pub const VOLUMES: [(&str, &str); 2] = [
    ("amicu-hf-cache", "/root/.cache/huggingface"),
    ("amicu-vllm-cache", "/root/.cache/vllm"),
];

/// Idle seconds before a replica scales down to zero.
pub const SCALEDOWN_WINDOW_SECS: u32 = 900;
/// Seconds Modal waits for the vLLM process to accept connections on `port`
/// before declaring the deploy failed. vLLM's own startup (loading weights,
/// building the KV cache) is the dominant cost here, not container cold
/// start.
pub const STARTUP_TIMEOUT_SECS: u32 = 600;
/// Maximum lifetime of a single container.
pub const TIMEOUT_SECS: u32 = 3600;
/// Requests a single replica handles concurrently before Modal scales out.
pub const TARGET_CONCURRENCY: u32 = 32;
/// Replicas kept warm; `0` scales to zero between requests.
pub const MIN_CONTAINERS: u32 = 0;

/// Starts, inspects, and stops a Modal-deployed vLLM server behind the same
/// [`amicu_llamacpp::ProcessSupervisor`] contract the local launchd/systemd
/// supervisor implements. See the module docs for the full argv/GPU/secret
/// mapping.
///
/// One instance targets one deployment: `app_name` is fixed at construction,
/// and [`ModalSupervisor::with_accelerator`]/[`ModalSupervisor::with_region`]
/// attach that deployment's hardware profile before the first `start`.
/// `status`/`stop` need neither - they address an already-deployed app
/// purely through the [`RuntimeHandle`] `start` returned - so
/// [`ModalSupervisor::new`] alone is enough to build one for those calls.
pub struct ModalSupervisor {
    app_name: String,
    gpu: Option<String>,
    region: Option<String>,
}

impl ModalSupervisor {
    /// A supervisor for `app_name`, with no GPU/region constraint yet. Valid
    /// on its own for [`ProcessSupervisor::status`]/[`ProcessSupervisor::stop`];
    /// [`ProcessSupervisor::start`] additionally needs
    /// [`ModalSupervisor::with_accelerator`] (and, optionally,
    /// [`ModalSupervisor::with_region`]) called first.
    #[must_use]
    pub fn new(app_name: impl Into<String>) -> Self {
        Self {
            app_name: app_name.into(),
            gpu: None,
            region: None,
        }
    }

    /// Sets the GPU request from a hardware profile's accelerator (`None`
    /// for a CPU-only container). Validates eagerly - before any RPC - that
    /// the accelerator is a GPU: Modal has no TPU or Apple Silicon offering,
    /// so [`AcceleratorKind::Tpu`]/[`AcceleratorKind::AppleSilicon`] are
    /// rejected here rather than surfacing as an opaque `FunctionCreate`
    /// failure later.
    ///
    /// # Errors
    /// Returns [`LlamaCppError::Supervisor`] if `accelerator` is present but
    /// is not [`AcceleratorKind::Gpu`].
    pub fn with_accelerator(
        mut self,
        accelerator: Option<&Accelerator>,
    ) -> Result<Self, LlamaCppError> {
        self.gpu = accelerator.map(gpu_string).transpose()?;
        Ok(self)
    }

    /// Sets the region constraint (a hardware profile's `region.code`, if
    /// any).
    #[must_use]
    pub fn with_region(mut self, region: Option<String>) -> Self {
        self.region = region;
        self
    }

    /// Convenience: builds a supervisor for `app_name` fully configured from
    /// a resolved [`HardwareProfile`] in one call.
    ///
    /// # Errors
    /// See [`ModalSupervisor::with_accelerator`].
    pub fn for_hardware(
        app_name: impl Into<String>,
        hardware: &HardwareProfile,
    ) -> Result<Self, LlamaCppError> {
        let region = hardware.region.as_ref().map(|r| r.code.clone());
        let supervisor = Self::new(app_name).with_accelerator(hardware.accelerator.as_ref())?;
        Ok(supervisor.with_region(region))
    }
}

/// Deployment-name-safe app name: `amicu-<sanitized deployment id>`.
///
/// Lower-cases `deployment_id`, replaces every byte that is not
/// `[a-z0-9-]` with `-`, collapses repeats, and trims leading/trailing
/// dashes - conservative enough to satisfy any reasonable "deployment name"
/// constraint without needing Modal's exact validation regex (not published
/// as of this writing).
#[must_use]
pub fn app_name_for(deployment_id: &str) -> String {
    let mut out = String::with_capacity(deployment_id.len() + 6);
    out.push_str("amicu-");
    let mut last_was_dash = true; // swallow a leading separator too
    for byte in deployment_id.to_ascii_lowercase().bytes() {
        let ch = if byte.is_ascii_alphanumeric() {
            byte as char
        } else {
            '-'
        };
        if ch == '-' {
            if !last_was_dash {
                out.push('-');
            }
            last_was_dash = true;
        } else {
            out.push(ch);
            last_was_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Renders an [`Accelerator`] as Modal's `gpu=` string (`"H100"`, `"H100:2"`).
fn gpu_string(accelerator: &Accelerator) -> Result<String, LlamaCppError> {
    if accelerator.kind != AcceleratorKind::Gpu {
        return Err(LlamaCppError::Supervisor(format!(
            "Modal has no {:?} offering; only GPU accelerators can be deployed there",
            accelerator.kind
        )));
    }
    let model = accelerator.model.to_uppercase();
    Ok(if accelerator.count <= 1 {
        model
    } else {
        format!("{model}:{}", accelerator.count)
    })
}

/// Whether `model_reference` looks like a local filesystem path rather than
/// a Hugging Face repo id (`"org/model"`). Deliberately conservative - it
/// only catches the unambiguous cases (a leading `/`, `./`, `../`, or `~`) -
/// because false positives here would block a legitimate deploy, whereas a
/// false negative just reaches Modal's own "no such repo" error instead of
/// amicu's clearer one.
#[must_use]
fn looks_like_local_path(model_reference: &str) -> bool {
    model_reference.starts_with('/')
        || model_reference.starts_with("./")
        || model_reference.starts_with("../")
        || model_reference.starts_with('~')
}

/// The rendered model reference from a vLLM `argv` (`["vllm", "serve",
/// "<model>", ...]`), if the shape matches.
fn model_reference(argv: &[String]) -> Option<&str> {
    (argv.first().map(String::as_str) == Some("vllm")
        && argv.get(1).map(String::as_str) == Some("serve"))
    .then(|| argv.get(2).map(String::as_str))
    .flatten()
}

/// Refuses a launch whose model reference is a local path: Modal's
/// container has no access to the operator's filesystem, so vLLM there must
/// be given a Hugging Face repo id.
fn reject_local_path_model(argv: &[String]) -> Result<(), LlamaCppError> {
    match model_reference(argv) {
        Some(reference) if looks_like_local_path(reference) => {
            Err(LlamaCppError::Supervisor(format!(
                "Modal deployments need a Hugging Face model id (e.g. \
                 'meta-llama/Llama-3.1-8B-Instruct'), not a local path; got '{reference}' - \
                 Modal's container cannot see this machine's filesystem"
            )))
        }
        _ => Ok(()),
    }
}

/// Rewrites `--host 127.0.0.1` (what `amicu_vllm::command::render_command`
/// always renders) to `--host 0.0.0.0`. See the module docs for why this
/// happens here rather than in the vLLM renderer.
fn bind_all_interfaces(argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();
    if let Some(pos) = out.iter().position(|a| a == "--host") {
        if let Some(value) = out.get_mut(pos + 1) {
            "0.0.0.0".clone_into(value);
        } else {
            out.push("0.0.0.0".to_owned());
        }
    } else {
        out.push("--host".to_owned());
        out.push("0.0.0.0".to_owned());
    }
    out
}

/// Folds a [`ModalError`] into the [`LlamaCppError`] the `ProcessSupervisor`
/// trait requires. `ModalError`'s `Display` never contains a token or secret
/// value (see `crate::credentials`/`crate::error`'s doc comments), so this is
/// always safe to surface.
fn modal_err(error: &ModalError) -> LlamaCppError {
    LlamaCppError::Supervisor(error.to_string())
}

impl ModalSupervisor {
    /// Connects an authenticated [`ModalClient`], reading
    /// `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` fresh each call - this
    /// supervisor is stateless between calls, matching
    /// `amicu_llamacpp::ServiceSupervisor`'s own statelessness.
    async fn connect(&self) -> Result<ModalClient, LlamaCppError> {
        let credentials = Credentials::from_env().map_err(|e| {
            LlamaCppError::Supervisor(format!(
                "{e}; set MODAL_TOKEN_ID and MODAL_TOKEN_SECRET before deploying to Modal"
            ))
        })?;
        ModalClient::connect(credentials)
            .await
            .map_err(|e| modal_err(&e))
    }

    /// Builds the [`ServerSpec`] for `plan`, forwarding `secrets` (plus the
    /// [`HF_TOKEN_ENV_VAR`] fallback) as Modal Secret env, never image env.
    ///
    /// # Errors
    /// - [`LlamaCppError::Supervisor`] if `plan` is not a vLLM launch, is not
    ///   a [`amicu_runtime::RenderedLaunch::Command`], names a local-path
    ///   model (see [`reject_local_path_model`]), or has a settings `env` key
    ///   that collides with a resolved secret name (ambiguous - which one
    ///   should win is not guessable, same rule
    ///   `amicu_llamacpp::ServiceSupervisor::start` applies to the local
    ///   path).
    ///
    /// `base_registry_image` and `hf_token_fallback` are read from the
    /// process environment by [`ProcessSupervisor::start`] ([`VLLM_IMAGE_ENV_VAR`]/
    /// [`HF_TOKEN_ENV_VAR`]) and passed in explicitly here rather than read
    /// directly - this keeps the mapping itself a pure function of its
    /// arguments, exercised by the unit tests below with no process-global
    /// state to set up or restore.
    fn build_spec(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
        base_registry_image: &str,
        hf_token_fallback: Option<&str>,
    ) -> Result<ServerSpec, LlamaCppError> {
        let settings = match &plan.settings {
            Runtime::Vllm(s) => s,
            Runtime::LlamaCpp(_) => {
                return Err(LlamaCppError::Supervisor(
                    "the Modal supervisor serves vLLM launches only".to_owned(),
                ));
            }
        };
        let argv = plan.rendered.argv().ok_or_else(|| {
            LlamaCppError::Supervisor("Modal supervisor expects a command launch".to_owned())
        })?;
        reject_local_path_model(argv)?;
        let argv = bind_all_interfaces(argv);

        if let Some(s) = secrets.iter().find(|s| settings.env.contains_key(&s.name)) {
            return Err(LlamaCppError::Supervisor(format!(
                "env var '{}' is set both by the deployment's `env` block and a resolved secret",
                s.name
            )));
        }

        let image_env: BTreeMap<String, String> = settings
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let mut secret_env = BTreeMap::new();
        for secret in secrets {
            let value = secret.value.expose_secret_str().map_err(|_| {
                LlamaCppError::Supervisor(format!("secret '{}' is not valid utf-8", secret.name))
            })?;
            secret_env.insert(secret.name.clone(), value.to_owned());
        }
        // Documented fallback (see the module docs and `HF_TOKEN_ENV_VAR`):
        // read from the operator's own environment, never the deployment's.
        if !secret_env.contains_key(HF_TOKEN_ENV_VAR)
            && !image_env.contains_key(HF_TOKEN_ENV_VAR)
            && let Some(token) = hf_token_fallback
        {
            secret_env.insert(HF_TOKEN_ENV_VAR.to_owned(), token.to_owned());
        }

        Ok(ServerSpec {
            app_name: self.app_name.clone(),
            gpu: self.gpu.clone(),
            region: self.region.clone(),
            port: settings.port,
            argv,
            image: ImageSpec {
                base_registry_image: base_registry_image.to_owned(),
                pip_packages: Vec::new(),
                env: image_env,
            },
            volumes: VOLUMES
                .iter()
                .map(|(name, path)| ((*name).to_owned(), (*path).to_owned()))
                .collect(),
            secret_env,
            scaledown_window_secs: SCALEDOWN_WINDOW_SECS,
            startup_timeout_secs: STARTUP_TIMEOUT_SECS,
            timeout_secs: TIMEOUT_SECS,
            target_concurrency: TARGET_CONCURRENCY,
            min_containers: MIN_CONTAINERS,
            max_containers: None,
        })
    }
}

/// `GET <endpoint>/health` with a short timeout, mirroring
/// `amicu_llamacpp::health::HttpHealthProbe` but over TLS (Modal's endpoint
/// is always HTTPS) - see the module docs for why that probe cannot be reused
/// as-is. A `2xx` is healthy; anything else (including a network error or
/// timeout) is not. Returns the status code when a response arrived (so a
/// proxy `401` can be told apart from "not up yet"); proxy auth headers are
/// attached when `MODAL_PROXY_TOKEN_*` are set.
async fn health_status(endpoint: &str) -> Option<u16> {
    let url = format!("{}/health", endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = crate::proxy_auth::apply(client.get(url))
        .send()
        .await
        .ok()?;
    Some(resp.status().as_u16())
}

#[async_trait]
impl ProcessSupervisor for ModalSupervisor {
    async fn start(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        let base_registry_image =
            std::env::var(VLLM_IMAGE_ENV_VAR).unwrap_or_else(|_| DEFAULT_VLLM_IMAGE.to_owned());
        let hf_token_fallback = std::env::var(HF_TOKEN_ENV_VAR).ok();
        let spec = self.build_spec(
            plan,
            secrets,
            &base_registry_image,
            hf_token_fallback.as_deref(),
        )?;
        let mut client = self.connect().await?;
        // Build log lines carry no secret (see `crate::deploy`'s doc
        // comments); discarding them here (rather than printing) matches
        // `apply`'s existing quiet-by-default behaviour for the local
        // supervisor, which reports only the final handle.
        let mut discard_log_line = |_line: &str| {};
        let deployment = client
            .deploy_server(&spec, &mut discard_log_line)
            .await
            .map_err(|e| modal_err(&e))?;
        Ok(RuntimeHandle {
            id: deployment.app_id,
            deployment: plan.deployment.clone(),
            endpoint: deployment.url,
        })
    }

    async fn status(&self, handle: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError> {
        let code = health_status(&handle.endpoint).await;
        Ok(if matches!(code, Some(c) if (200..300).contains(&c)) {
            RuntimeStatus::Running {
                detail: "healthy".to_owned(),
            }
        } else if let Some(hint) =
            code.and_then(|c| crate::proxy_auth::hint_for_status(c, crate::proxy_auth::env_state()))
        {
            RuntimeStatus::Failed {
                detail: format!("auth required: {hint}"),
            }
        } else {
            RuntimeStatus::Failed {
                detail: "endpoint not ready".to_owned(),
            }
        })
    }

    async fn metrics(&self, _handle: &RuntimeHandle) -> Result<RuntimeMetrics, LlamaCppError> {
        // Same placeholder zero-metrics `amicu_llamacpp::ServiceSupervisor`
        // returns: real metrics need Modal's own function-call telemetry,
        // out of scope here.
        Ok(RuntimeMetrics {
            requests_total: 0,
            latency_p50_ms: 0.0,
            tokens_per_second: 0.0,
            memory_used: amicu_core::ByteSize::from_bytes(0),
        })
    }

    async fn logs(
        &self,
        _handle: &RuntimeHandle,
        _options: LogOptions,
    ) -> Result<LogStream, LlamaCppError> {
        // `AppGetLogs` plumbing is a separate PR (amicu-planning#158 PR 4,
        // `crates/amicu-modal/src/ops.rs`) landing on its own branch; wiring
        // it through this trait is a follow-up once it merges.
        Err(LlamaCppError::Supervisor(
            "log retrieval for Modal deployments is not yet implemented (amicu-planning#158)"
                .to_owned(),
        ))
    }

    async fn stop(&self, handle: &RuntimeHandle) -> Result<(), LlamaCppError> {
        let mut client = self.connect().await?;
        client.stop_app(&handle.id).await.map_err(|e| modal_err(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::ByteSize;
    use amicu_core::id::DeploymentId;
    use amicu_core::resource::VllmSettings;
    use amicu_core::secret::SecretValue;
    use amicu_runtime::{RenderedLaunch, ResourceEstimate};
    use std::collections::BTreeMap;

    fn base_argv() -> Vec<String> {
        vec![
            "vllm".to_owned(),
            "serve".to_owned(),
            "meta-llama/Llama-3.1-8B-Instruct".to_owned(),
            "--port".to_owned(),
            "8000".to_owned(),
            "--host".to_owned(),
            "127.0.0.1".to_owned(),
            "--max-model-len".to_owned(),
            "8192".to_owned(),
        ]
    }

    fn plan_with(argv: Vec<String>, settings: VllmSettings) -> LaunchPlan {
        let text = argv.join(" ");
        LaunchPlan {
            deployment: DeploymentId::from("dep"),
            runtime: amicu_runtime::RuntimeKind::Vllm,
            rendered: RenderedLaunch::Command { argv, text },
            settings: Runtime::Vllm(settings),
            secret_refs: Vec::new(),
            estimate: ResourceEstimate {
                estimated_memory: ByteSize::from_bytes(0),
                gpu_count: 1,
                kv_cache: None,
            },
        }
    }

    fn base_settings() -> VllmSettings {
        VllmSettings {
            context_length: 8192,
            port: 8000,
            ..Default::default()
        }
    }

    // --- app_name_for -------------------------------------------------

    #[test]
    fn app_name_sanitizes_and_prefixes() {
        assert_eq!(app_name_for("support-chat"), "amicu-support-chat");
        assert_eq!(app_name_for("Support_Chat.01"), "amicu-support-chat-01");
        assert_eq!(app_name_for("  weird//id--"), "amicu-weird-id");
    }

    // --- gpu_string -----------------------------------------------------

    fn accelerator(kind: AcceleratorKind, model: &str, count: u32) -> Accelerator {
        Accelerator {
            kind,
            model: model.to_owned(),
            count,
            memory: ByteSize::from_bytes(0),
            memory_bandwidth_gbps: None,
        }
    }

    #[test]
    fn gpu_string_upper_cases_and_omits_count_of_one() {
        let acc = accelerator(AcceleratorKind::Gpu, "h100", 1);
        assert_eq!(gpu_string(&acc).unwrap(), "H100");
    }

    #[test]
    fn gpu_string_includes_count_above_one() {
        let acc = accelerator(AcceleratorKind::Gpu, "h100", 2);
        assert_eq!(gpu_string(&acc).unwrap(), "H100:2");
    }

    #[test]
    fn gpu_string_rejects_non_gpu_accelerators() {
        for kind in [AcceleratorKind::Tpu, AcceleratorKind::AppleSilicon] {
            let acc = accelerator(kind, "whatever", 1);
            assert!(gpu_string(&acc).is_err());
        }
    }

    // --- with_accelerator / for_hardware --------------------------------

    #[test]
    fn with_accelerator_none_leaves_gpu_unset() {
        let sup = ModalSupervisor::new("amicu-x")
            .with_accelerator(None)
            .unwrap();
        assert!(sup.gpu.is_none());
    }

    #[test]
    fn for_hardware_rejects_tpu() {
        let hardware = HardwareProfile {
            id: "hw".into(),
            provider: "modal".into(),
            region: None,
            accelerator: Some(accelerator(AcceleratorKind::Tpu, "v5e", 1)),
            cpu: None,
            memory: ByteSize::from_bytes(0),
            interconnect: None,
            price: None,
        };
        assert!(ModalSupervisor::for_hardware("amicu-x", &hardware).is_err());
    }

    // --- bind_all_interfaces --------------------------------------------

    #[test]
    fn bind_all_interfaces_rewrites_loopback_host() {
        let argv = bind_all_interfaces(&base_argv());
        let idx = argv.iter().position(|a| a == "--host").unwrap();
        assert_eq!(argv[idx + 1], "0.0.0.0");
    }

    #[test]
    fn bind_all_interfaces_appends_when_host_flag_absent() {
        let argv: Vec<String> = vec!["vllm".to_owned(), "serve".to_owned(), "m".to_owned()];
        let out = bind_all_interfaces(&argv);
        assert_eq!(out.last(), Some(&"0.0.0.0".to_owned()));
        assert_eq!(out[out.len() - 2], "--host");
    }

    // --- reject_local_path_model -----------------------------------------

    #[test]
    fn local_paths_are_rejected() {
        for bad in ["/models/llama", "./llama", "../llama", "~/llama"] {
            let mut argv = base_argv();
            argv[2] = bad.to_owned();
            assert!(reject_local_path_model(&argv).is_err(), "{bad}");
        }
    }

    #[test]
    fn hf_repo_ids_are_accepted() {
        assert!(reject_local_path_model(&base_argv()).is_ok());
    }

    // --- build_spec -------------------------------------------------------

    /// `build_spec` never reads the process environment itself (see its doc
    /// comment) - these two stand in for what `start` would read from
    /// [`VLLM_IMAGE_ENV_VAR`]/[`HF_TOKEN_ENV_VAR`], so tests never need
    /// `unsafe` env mutation (forbidden workspace-wide) to exercise either
    /// fallback.
    fn no_image_override() -> &'static str {
        DEFAULT_VLLM_IMAGE
    }
    fn no_hf_token_fallback() -> Option<&'static str> {
        None
    }

    #[test]
    fn build_spec_binds_all_interfaces_and_uses_default_image() {
        let sup = ModalSupervisor::new("amicu-dep");
        let plan = plan_with(base_argv(), base_settings());
        let spec = sup
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap();
        let idx = spec.argv.iter().position(|a| a == "--host").unwrap();
        assert_eq!(spec.argv[idx + 1], "0.0.0.0");
        assert_eq!(spec.image.base_registry_image, DEFAULT_VLLM_IMAGE);
        assert_eq!(spec.volumes.len(), VOLUMES.len());
    }

    #[test]
    fn build_spec_rejects_a_local_path_model_before_any_rpc() {
        let sup = ModalSupervisor::new("amicu-dep");
        let mut argv = base_argv();
        argv[2] = "/models/llama".to_owned();
        let plan = plan_with(argv, base_settings());
        let err = sup
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap_err();
        assert!(err.to_string().contains("Hugging Face"));
    }

    #[test]
    fn build_spec_rejects_a_non_vllm_plan() {
        let sup = ModalSupervisor::new("amicu-dep");
        let mut plan = plan_with(base_argv(), base_settings());
        plan.settings = Runtime::LlamaCpp(amicu_core::resource::LlamaCppSettings::default());
        assert!(
            sup.build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
                .is_err()
        );
    }

    #[test]
    fn build_spec_forwards_resolved_secrets_as_secret_env_never_image_env() {
        let sup = ModalSupervisor::new("amicu-dep");
        let plan = plan_with(base_argv(), base_settings());
        let secrets = [ResolvedSecret {
            name: "HF_TOKEN".to_owned(),
            value: SecretValue::new("hf_super_secret".to_owned()),
        }];
        let spec = sup
            .build_spec(&plan, &secrets, no_image_override(), no_hf_token_fallback())
            .unwrap();
        assert_eq!(
            spec.secret_env.get("HF_TOKEN").map(String::as_str),
            Some("hf_super_secret")
        );
        assert!(!spec.image.env.contains_key("HF_TOKEN"));
    }

    #[test]
    fn build_spec_plain_env_settings_become_image_env_not_secret_env() {
        let sup = ModalSupervisor::new("amicu-dep");
        let mut settings = base_settings();
        settings.env = BTreeMap::from([("VLLM_LOGGING_LEVEL".to_owned(), "DEBUG".to_owned())]);
        let plan = plan_with(base_argv(), settings);
        let spec = sup
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap();

        assert_eq!(
            spec.image.env.get("VLLM_LOGGING_LEVEL").map(String::as_str),
            Some("DEBUG")
        );
        assert!(spec.secret_env.is_empty());
    }

    #[test]
    fn build_spec_rejects_env_key_colliding_with_a_resolved_secret() {
        let sup = ModalSupervisor::new("amicu-dep");
        let mut settings = base_settings();
        settings.env = BTreeMap::from([("HF_TOKEN".to_owned(), "not-a-secret".to_owned())]);
        let plan = plan_with(base_argv(), settings);
        let secrets = [ResolvedSecret {
            name: "HF_TOKEN".to_owned(),
            value: SecretValue::new("real-secret".to_owned()),
        }];
        assert!(
            sup.build_spec(&plan, &secrets, no_image_override(), no_hf_token_fallback())
                .is_err()
        );
    }

    #[test]
    fn build_spec_honours_the_image_override_argument() {
        let sup = ModalSupervisor::new("amicu-dep");
        let plan = plan_with(base_argv(), base_settings());
        let spec = sup
            .build_spec(
                &plan,
                &[],
                "vllm/vllm-openai:v0.30.0",
                no_hf_token_fallback(),
            )
            .unwrap();
        assert_eq!(spec.image.base_registry_image, "vllm/vllm-openai:v0.30.0");
    }

    #[test]
    fn build_spec_uses_the_hf_token_fallback_only_when_not_already_set() {
        let sup = ModalSupervisor::new("amicu-dep");
        let plan = plan_with(base_argv(), base_settings());
        let spec = sup
            .build_spec(&plan, &[], no_image_override(), Some("hf_fallback_token"))
            .unwrap();
        assert_eq!(
            spec.secret_env.get("HF_TOKEN").map(String::as_str),
            Some("hf_fallback_token")
        );

        // A resolved secret named HF_TOKEN wins over the fallback.
        let secrets = [ResolvedSecret {
            name: "HF_TOKEN".to_owned(),
            value: SecretValue::new("real-secret".to_owned()),
        }];
        let spec = sup
            .build_spec(
                &plan,
                &secrets,
                no_image_override(),
                Some("hf_fallback_token"),
            )
            .unwrap();
        assert_eq!(
            spec.secret_env.get("HF_TOKEN").map(String::as_str),
            Some("real-secret")
        );
    }
}
