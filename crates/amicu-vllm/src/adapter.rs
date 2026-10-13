//! The vLLM [`RuntimeAdapter`] implementation.
//!
//! Mirrors `amicu_llamacpp::adapter`'s shape (amicu-planning#152) but reuses
//! that crate's [`Catalog`], [`MapCatalog`], [`ProcessSupervisor`],
//! [`ServiceSupervisor`], and `host_supervisor` directly rather than
//! re-deriving them: both are single-process HTTP servers launched the same
//! way (`ServiceSupervisor::start`/`endpoint_for` already matches on
//! `Runtime::Vllm` - see `amicu_llamacpp::supervisor`), so there is nothing
//! runtime-specific left in that layer to duplicate.

use amicu_core::ByteSize;
use amicu_core::adapter::{
    Adapter, AdapterError, AdapterErrorKind, AdapterId, DescribeCapabilities, HealthStatus,
};
use amicu_core::diagnostic::{Diagnostic, Severity, catalog};
use amicu_core::resource::{
    ArtifactFormat, Deployment, HardwareProfile, Modality, Quantization, Runtime, VllmSettings,
};
use amicu_core::secret::SecretResolver;
use amicu_llamacpp::{Catalog, CommandRunner, ProcessSupervisor, ResolvedModel};
use amicu_registry::{FitConfig, check_vllm_fit};
use amicu_runtime::{
    LaunchPlan, LogOptions, LogStream, Parallelism, Platform, RenderedLaunch, ResourceEstimate,
    RuntimeAdapter, RuntimeCapabilities, RuntimeHandle, RuntimeKind, RuntimeMetrics, RuntimeStatus,
};
use async_trait::async_trait;

use crate::command::{DENYLISTED_ENV_KEYS, DENYLISTED_FLAGS, is_typed_flag, render_command};
use crate::error::VllmError;

/// Quantizations the vLLM adapter reports as servable (safetensors dtypes and
/// integer quantizations vLLM natively supports). GGUF quantizations are
/// deliberately excluded from this list even though vLLM can load a
/// single-file GGUF checkpoint - that path is experimental (see
/// [`gguf_experimental_diagnostic`]), not the primary supported surface.
const SAFETENSORS_QUANTIZATIONS: &[Quantization] = &[
    Quantization::F32,
    Quantization::F16,
    Quantization::Bf16,
    Quantization::Fp8,
    Quantization::Int8,
    Quantization::Int4,
];

/// Default weight dtype assumed when a model declares no explicit
/// `artifact.quantization` - vLLM's own `--dtype auto` resolves to the
/// checkpoint's native dtype, which for the overwhelming majority of
/// HuggingFace text models is `bfloat16`. `bytes_per_parameter` for `Bf16`
/// and `F16` are identical (2 bytes), so this only matters for display, not
/// for the fit arithmetic.
const DEFAULT_QUANTIZATION: Quantization = Quantization::Bf16;

/// Default `--gpu-memory-utilization` vLLM itself applies when the field is
/// unset (matches vLLM's own CLI default).
const DEFAULT_GPU_MEMORY_UTILIZATION: f32 = 0.92;

/// Default `--tensor-parallel-size` vLLM itself applies when the field is
/// unset (matches vLLM's own CLI default: no sharding).
const DEFAULT_TENSOR_PARALLEL_SIZE: u32 = 1;

/// The vLLM runtime adapter: safetensors/GGUF planning and delegated process
/// supervision.
///
/// Planning (`capabilities`/`validate`/`plan_launch`) is pure and resolves the
/// deployment's model/hardware through a [`Catalog`]. Apply/runtime-time
/// methods delegate to a [`ProcessSupervisor`] (the same launchd/systemd
/// implementation llama.cpp uses).
pub struct VllmAdapter {
    id: AdapterId,
    catalog: Box<dyn Catalog>,
    supervisor: Box<dyn ProcessSupervisor>,
    fit: FitConfig,
}

impl VllmAdapter {
    /// Build an adapter over a catalog and process supervisor, with default
    /// fit tunables.
    #[must_use]
    pub fn new(catalog: Box<dyn Catalog>, supervisor: Box<dyn ProcessSupervisor>) -> Self {
        Self {
            id: AdapterId::new("vllm"),
            catalog,
            supervisor,
            fit: FitConfig::default(),
        }
    }

    /// Override the weight-fit tunables.
    #[must_use]
    pub fn with_fit_config(mut self, fit: FitConfig) -> Self {
        self.fit = fit;
        self
    }

    /// Borrow the vLLM settings from a deployment.
    fn settings(d: &Deployment) -> Result<&VllmSettings, VllmError> {
        match &d.runtime {
            Runtime::Vllm(s) => Ok(s),
            // Only reached on a mis-dispatched deployment: a runtime-family
            // mismatch, not an artifact-format one.
            Runtime::LlamaCpp(_) => Err(VllmError::WrongRuntime("llama_cpp")),
        }
    }

    /// Resolve model + hardware for a deployment, erroring on a missing reference.
    fn resolve(&self, d: &Deployment) -> Result<(ResolvedModel, HardwareProfile), VllmError> {
        let resolved = self
            .catalog
            .resolve_model(&d.model)
            .ok_or_else(|| VllmError::ModelNotFound(d.model.clone()))?;
        let hardware = self
            .catalog
            .resolve_hardware(&d.hardware_profile)
            .ok_or_else(|| VllmError::HardwareNotFound(d.hardware_profile.clone()))?;
        Ok((resolved, hardware))
    }
}

/// Convert a classified error into a diagnostic for the pure `validate` path.
fn diagnostic(error: &VllmError) -> Diagnostic {
    Diagnostic::new(error.code(), error.severity(), error.to_string())
}

/// `AMICU-CAP-006`: the model's artifact is GGUF, served by vLLM. Warning,
/// not a block - vLLM can load a single-file GGUF checkpoint, but upstream
/// documents the support as experimental, unlike llama.cpp where GGUF is the
/// native format.
fn gguf_experimental_diagnostic() -> Diagnostic {
    Diagnostic::new(
        catalog::CAP_VLLM_GGUF_EXPERIMENTAL,
        Severity::Warning,
        "model artifact is GGUF; vLLM's GGUF support is experimental".to_owned(),
    )
}

/// bytes-per-element for vLLM's `--kv-cache-dtype` value set. Unlike
/// llama.cpp's `KvCacheType` (which has a distinct block-quantized layout per
/// dtype), vLLM's fp8 KV-cache dtypes are plain 1-byte floats and `auto`
/// resolves to the model's own weight dtype - `bfloat16`/`float16` (2 bytes)
/// for the overwhelming majority of text models amicu targets, the same
/// assumption [`DEFAULT_QUANTIZATION`] makes for weights.
fn vllm_kv_bytes_per_element(dtype: amicu_core::resource::VllmKvCacheDtype) -> f64 {
    use amicu_core::resource::VllmKvCacheDtype as D;
    match dtype {
        D::Auto => 2.0,
        D::Fp8 | D::Fp8E5M2 | D::Fp8E4M3 => 1.0,
    }
}

/// Estimated total KV-cache memory for one vLLM deployment, when the model
/// declares full shape metadata (amicu-planning#152's fit-estimate formula:
/// "weights + kv cache only").
///
/// `ctx × kv_layers × n_kv_heads × head_dim × 2 × bytes_per_element(dtype)`,
/// where `kv_layers` is [`amicu_core::resource::Model::kv_layers`] (only the
/// full-attention layers for a hybrid-attention model): one K vector and one
/// V vector per attention head per KV-holding layer per cached token - vLLM applies a single `--kv-cache-dtype` to both K and V, unlike
/// llama.cpp's independently-settable `-ctk`/`-ctv`. `dtype` defaults to
/// [`amicu_core::resource::VllmKvCacheDtype::Auto`] when
/// `settings.kv_cache_dtype` is unset, matching vLLM's own CLI default.
fn kv_cache_params(
    model: &amicu_core::resource::Model,
    settings: &VllmSettings,
) -> Option<ByteSize> {
    let (Some(n_layers), Some(n_kv_heads), Some(head_dim)) =
        (model.kv_layers(), model.n_kv_heads, model.head_dim)
    else {
        return None;
    };
    let dtype = settings
        .kv_cache_dtype
        .unwrap_or(amicu_core::resource::VllmKvCacheDtype::Auto);
    let elements = f64::from(settings.context_length)
        * f64::from(n_layers)
        * f64::from(n_kv_heads)
        * f64::from(head_dim);
    let bytes = elements * 2.0 * vllm_kv_bytes_per_element(dtype);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    Some(ByteSize::from_bytes(
        bytes.round().clamp(0.0, u64::MAX as f64) as u64,
    ))
}

/// `AMICU-CAP-010`: the model's shape is unknown, so the KV cache is not in
/// the memory estimate. Warning, not a block - see
/// [`catalog::CAP_KV_CACHE_NOT_ESTIMATED`].
fn kv_cache_not_estimated_diagnostic(settings: &VllmSettings) -> Diagnostic {
    Diagnostic::new(
        catalog::CAP_KV_CACHE_NOT_ESTIMATED,
        Severity::Warning,
        format!(
            "kv cache: not estimated (model shape unknown; set n_layers, n_kv_heads, head_dim \
             on the model); the fit check covers weights only, and a {}-token context may need \
             far more",
            settings.context_length
        ),
    )
}

/// The leading flag token of one `extra_args` entry. See
/// `amicu_llamacpp::adapter::flag_token` - identical rule, vLLM has no
/// analogous short-alias forms to preserve.
fn flag_token(arg: &str) -> Option<&str> {
    if !arg.starts_with('-') || arg == "-" {
        return None;
    }
    Some(arg.split('=').next().unwrap_or(arg))
}

/// Diagnose one flag spelling from `extra_args`: `AMICU-CAP-002` if it
/// duplicates a typed field's flag, `AMICU-CAP-003` if it is denylisted,
/// `None` if clean.
fn flag_conflict(flag: &str) -> Option<VllmError> {
    if is_typed_flag(flag) {
        return Some(VllmError::ExtraArgDuplicatesTyped(flag.to_owned()));
    }
    if DENYLISTED_FLAGS.contains(&flag) {
        return Some(VllmError::ExtraArgDenylisted(flag.to_owned()));
    }
    None
}

/// Diagnose one settings `env` key: `AMICU-CAP-003` if it is denylisted.
/// Unlike llama.cpp's `LLAMA_ARG_*` convention, vLLM has no documented 1:1
/// env-to-flag mirror, so there is no further mapping to check here.
fn env_key_conflict(key: &str) -> Option<VllmError> {
    if DENYLISTED_ENV_KEYS.contains(&key) {
        return Some(VllmError::EnvKeyDenylisted(key.to_owned()));
    }
    None
}

/// The first conflict across `extra_args` and `env`, as an error - used by
/// `plan_launch` to refuse building an ambiguous or unsafe launch.
/// `extra_args` is checked first, in declaration order, then `env` in key
/// order.
fn first_conflict(settings: &VllmSettings) -> Result<(), VllmError> {
    for arg in &settings.extra_args {
        if let Some(flag) = flag_token(arg)
            && let Some(err) = flag_conflict(flag)
        {
            return Err(err);
        }
    }
    for key in settings.env.keys() {
        if let Some(err) = env_key_conflict(key) {
            return Err(err);
        }
    }
    Ok(())
}

/// Diagnose `extra_args` and `env` together for `validate`: every
/// duplicated/denylisted flag or env key (`AMICU-CAP-002`/`AMICU-CAP-003`),
/// plus one informational `AMICU-CAP-004` whenever `extra_args` is non-empty
/// at all.
fn extra_args_and_env_diagnostics(settings: &VllmSettings) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for arg in &settings.extra_args {
        if let Some(flag) = flag_token(arg)
            && let Some(err) = flag_conflict(flag)
        {
            out.push(diagnostic(&err));
        }
    }
    for key in settings.env.keys() {
        if let Some(err) = env_key_conflict(key) {
            out.push(diagnostic(&err));
        }
    }
    if !settings.extra_args.is_empty() {
        out.push(Diagnostic::new(
            catalog::CAP_EXTRA_ARGS_PRESENT,
            Severity::Info,
            "raw runtime flags present; estimate unverified".to_owned(),
        ));
    }
    out
}

#[async_trait]
impl Adapter for VllmAdapter {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(if amicu_llamacpp::HostRunner.which("vllm").is_some() {
            HealthStatus::Healthy
        } else {
            HealthStatus::Unavailable {
                detail: VllmError::BinaryNotFound.to_string(),
            }
        })
    }
}

impl DescribeCapabilities for VllmAdapter {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl RuntimeAdapter for VllmAdapter {
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            runtime: RuntimeKind::Vllm,
            artifact_formats: vec![ArtifactFormat::Safetensors, ArtifactFormat::Gguf],
            platforms: vec![Platform::Cuda, Platform::Rocm],
            parallelism: Parallelism {
                tensor: true,
                pipeline: false,
                max_gpus: 8,
            },
            quantizations: SAFETENSORS_QUANTIZATIONS.to_vec(),
            lora: true,
            tool_calling: true,
            multimodal: vec![Modality::Vision],
        }
    }

    fn validate(&self, d: &Deployment) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        let settings = match Self::settings(d) {
            Ok(s) => s,
            Err(e) => {
                out.push(diagnostic(&e));
                return out;
            }
        };
        out.extend(extra_args_and_env_diagnostics(settings));
        let (resolved, hardware) = match self.resolve(d) {
            Ok(pair) => pair,
            Err(e) => {
                out.push(diagnostic(&e));
                return out;
            }
        };
        let model = &resolved.model;
        if model.artifact.format == ArtifactFormat::Gguf {
            out.push(gguf_experimental_diagnostic());
        }
        let quantization = model.artifact.quantization.unwrap_or(DEFAULT_QUANTIZATION);
        let weights = amicu_registry::weights_memory(resolved.total_parameters, quantization);
        let kv_cache = kv_cache_params(model, settings);
        let report = check_vllm_fit(
            weights,
            &hardware,
            settings
                .gpu_memory_utilization
                .unwrap_or(DEFAULT_GPU_MEMORY_UTILIZATION),
            settings
                .tensor_parallel_size
                .unwrap_or(DEFAULT_TENSOR_PARALLEL_SIZE),
            kv_cache,
        );
        if let Some(diag) = report.diagnostic {
            out.push(diag);
        }
        if kv_cache.is_none() {
            out.push(kv_cache_not_estimated_diagnostic(settings));
        }
        out
    }

    fn plan_launch(&self, d: &Deployment) -> Result<LaunchPlan, AdapterError> {
        let settings = Self::settings(d).map_err(VllmError::into_adapter_error)?;
        first_conflict(settings).map_err(VllmError::into_adapter_error)?;
        let (resolved, hardware) = self.resolve(d).map_err(VllmError::into_adapter_error)?;
        let model = &resolved.model;

        let quantization = model.artifact.quantization.unwrap_or(DEFAULT_QUANTIZATION);
        let weights = amicu_registry::weights_memory(resolved.total_parameters, quantization);
        let kv_cache = kv_cache_params(model, settings);
        let gpu_memory_utilization = settings
            .gpu_memory_utilization
            .unwrap_or(DEFAULT_GPU_MEMORY_UTILIZATION);
        let tensor_parallel_size = settings
            .tensor_parallel_size
            .unwrap_or(DEFAULT_TENSOR_PARALLEL_SIZE);
        let report = check_vllm_fit(
            weights,
            &hardware,
            gpu_memory_utilization,
            tensor_parallel_size,
            kv_cache,
        );

        let (argv, text) = render_command(&model.source.reference, settings);

        let gpu_count = u32::from(hardware.accelerator.is_some()) * tensor_parallel_size.max(1);
        let estimated_memory =
            ByteSize::from_bytes(report.weights.bytes() + kv_cache.map_or(0, ByteSize::bytes));

        Ok(LaunchPlan {
            deployment: d.id.clone(),
            runtime: RuntimeKind::Vllm,
            rendered: RenderedLaunch::Command { argv, text },
            settings: d.runtime.clone(),
            secret_refs: Vec::new(),
            estimate: ResourceEstimate {
                estimated_memory,
                gpu_count,
                kv_cache,
            },
        })
    }

    async fn apply(
        &self,
        p: &LaunchPlan,
        s: &dyn SecretResolver,
    ) -> Result<RuntimeHandle, AdapterError> {
        // Resolve referenced secrets at apply time; values never touch the plan.
        let mut secrets = Vec::with_capacity(p.secret_refs.len());
        for reference in &p.secret_refs {
            let value = s.resolve(reference).await?;
            secrets.push(amicu_llamacpp::ResolvedSecret {
                name: reference.name.clone(),
                value,
            });
        }
        self.supervisor
            .start(p, &secrets)
            .await
            .map_err(|e| vllm_supervisor_error(&e))
    }

    async fn status(&self, h: &RuntimeHandle) -> Result<RuntimeStatus, AdapterError> {
        self.supervisor
            .status(h)
            .await
            .map_err(|e| vllm_supervisor_error(&e))
    }

    async fn metrics(&self, h: &RuntimeHandle) -> Result<RuntimeMetrics, AdapterError> {
        self.supervisor
            .metrics(h)
            .await
            .map_err(|e| vllm_supervisor_error(&e))
    }

    async fn logs(&self, h: &RuntimeHandle, o: LogOptions) -> Result<LogStream, AdapterError> {
        self.supervisor
            .logs(h, o)
            .await
            .map_err(|e| vllm_supervisor_error(&e))
    }

    async fn stop(&self, h: &RuntimeHandle) -> Result<(), AdapterError> {
        self.supervisor
            .stop(h)
            .await
            .map_err(|e| vllm_supervisor_error(&e))
    }
}

/// Fold a [`ProcessSupervisor`] failure (`amicu_llamacpp::LlamaCppError` -
/// the shared supervisor is generic over the runtime, not the error type)
/// into a [`VllmError::Supervisor`] so every error this adapter surfaces is
/// classified through [`VllmError`], not a borrowed llama.cpp code.
fn vllm_supervisor_error(e: &amicu_llamacpp::LlamaCppError) -> AdapterError {
    VllmError::Supervisor(e.to_string()).into_adapter_error()
}
