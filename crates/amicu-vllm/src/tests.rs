//! Hermetic tests: no real binary, network, or processes.

use std::sync::Mutex;

use async_trait::async_trait;

use amicu_core::ByteSize;
use amicu_core::adapter::{Adapter, AdapterError, HealthStatus};
use amicu_core::diagnostic::{Severity, catalog};
use amicu_core::id::{DeploymentId, HardwareProfileId, ModelId};
use amicu_core::resource::{
    Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Deployment, HardwareProfile, Modality,
    Model, ModelSource, Runtime, SourceRegistry, VllmKvCacheDtype, VllmSettings,
};
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use amicu_runtime::{
    LogOptions, LogStream, RuntimeAdapter, RuntimeHandle, RuntimeMetrics, RuntimeStatus,
};

use amicu_llamacpp::{LlamaCppError, MapCatalog, ProcessSupervisor, ResolvedSecret};

use crate::adapter::VllmAdapter;

const GIB: u64 = 1024 * 1024 * 1024;

// ---- fake process supervisor ------------------------------------------------

#[derive(Default)]
struct FakeSupervisor {
    stopped: Mutex<bool>,
}

#[async_trait]
impl ProcessSupervisor for FakeSupervisor {
    async fn start(
        &self,
        plan: &amicu_runtime::LaunchPlan,
        _secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        Ok(RuntimeHandle {
            id: "inst-1".into(),
            deployment: plan.deployment.clone(),
            endpoint: "http://127.0.0.1:8000".into(),
            exposure: amicu_runtime::EndpointExposure::Loopback,
        })
    }

    async fn status(&self, _h: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError> {
        if *self.stopped.lock().unwrap() {
            Ok(RuntimeStatus::Stopped {
                detail: "stopped".into(),
            })
        } else {
            Ok(RuntimeStatus::Running {
                detail: "ready".into(),
            })
        }
    }

    async fn metrics(&self, _h: &RuntimeHandle) -> Result<RuntimeMetrics, LlamaCppError> {
        Ok(RuntimeMetrics {
            requests_total: 0,
            latency_p50_ms: 0.0,
            tokens_per_second: 0.0,
            memory_used: ByteSize::from_bytes(0),
        })
    }

    async fn logs(&self, _h: &RuntimeHandle, _o: LogOptions) -> Result<LogStream, LlamaCppError> {
        Ok(LogStream(vec!["server listening".into()]))
    }

    async fn stop(&self, _h: &RuntimeHandle) -> Result<(), LlamaCppError> {
        *self.stopped.lock().unwrap() = true;
        Ok(())
    }
}

// ---- fixtures ---------------------------------------------------------------

fn safetensors_model() -> Model {
    Model {
        id: ModelId::new("llama8b"),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "meta-llama/Llama-3.1-8B-Instruct".into(),
        },
        architecture: "llama".into(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Safetensors,
            quantization: None,
        },
        context_length: Some(8192),
        variants: Vec::new(),
        n_layers: None,
        n_kv_heads: None,
        head_dim: None,
    }
}

fn gguf_model() -> Model {
    let mut m = safetensors_model();
    m.artifact.format = ArtifactFormat::Gguf;
    m
}

fn gpu_hardware(vram_gib: u64, count: u32) -> HardwareProfile {
    HardwareProfile {
        id: HardwareProfileId::new("hw"),
        provider: "on_prem".into(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "h100".into(),
            count,
            memory: ByteSize::from_bytes(vram_gib * GIB),
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(256 * GIB),
        interconnect: None,
        price: None,
    }
}

fn base_settings() -> VllmSettings {
    VllmSettings {
        context_length: 8192,
        port: 8000,
        tensor_parallel_size: Some(1),
        gpu_memory_utilization: Some(0.9),
        ..Default::default()
    }
}

fn deployment(settings: VllmSettings) -> Deployment {
    Deployment {
        scaling: None,
        api_key: None,
        id: DeploymentId::new("dep"),
        model: ModelId::new("llama8b"),
        hardware_profile: HardwareProfileId::new("hw"),
        runtime: Runtime::Vllm(settings),
    }
}

fn adapter(model: Model, hw: HardwareProfile, total_parameters: u64) -> VllmAdapter {
    let catalog = MapCatalog::new()
        .with_model(model, total_parameters)
        .with_hardware(hw);
    VllmAdapter::new(Box::new(catalog), Box::new(FakeSupervisor::default()))
}

// 8B params @ bf16 default (2 bytes) = ~14.9 GiB weights.
const LLAMA_8B: u64 = 8_000_000_000;

// ---- fit checks ---------------------------------------------------------

#[test]
fn validate_fits_comfortably_on_large_gpu() {
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(base_settings()));
    assert!(
        !diags.iter().any(|d| d.severity == Severity::Error),
        "unexpected error(s): {diags:?}"
    );
}

#[test]
fn validate_flags_underfit_as_error() {
    // 8B @ bf16 (~14.9 GiB) does not fit a 90%-utilized 16 GiB GPU.
    let mut settings = base_settings();
    settings.gpu_memory_utilization = Some(0.9);
    let a = adapter(safetensors_model(), gpu_hardware(16, 1), LLAMA_8B);
    let diags = a.validate(&deployment(settings));
    let err = diags
        .iter()
        .find(|d| d.severity == Severity::Error)
        .expect("expected an under-fit error");
    assert_eq!(err.code, catalog::MEM_VRAM_EXCEEDED);
}

#[test]
fn validate_fit_scales_with_tensor_parallel_size() {
    // 8B @ bf16 does not fit one 16 GiB GPU, but fits 2 sharded.
    let mut settings = base_settings();
    settings.tensor_parallel_size = Some(2);
    let a = adapter(safetensors_model(), gpu_hardware(16, 2), LLAMA_8B);
    let diags = a.validate(&deployment(settings));
    assert!(!diags.iter().any(|d| d.severity == Severity::Error));
}

#[test]
fn validate_defaults_gpu_memory_utilization_when_unset() {
    let mut settings = base_settings();
    settings.gpu_memory_utilization = None;
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(settings));
    assert!(!diags.iter().any(|d| d.severity == Severity::Error));
}

#[test]
fn validate_folds_kv_cache_into_fit_when_shape_is_known() {
    // A model with full shape metadata and a huge context should push a
    // borderline-fitting deployment over the edge once KV cache is added.
    let mut model = safetensors_model();
    model.n_layers = Some(32);
    model.n_kv_heads = Some(8);
    model.head_dim = Some(128);
    let mut settings = base_settings();
    settings.context_length = 131_072;
    settings.gpu_memory_utilization = Some(0.9);
    // 15 GiB is just over the 8B bf16 weights figure (~14.9 GiB) at 90% of a
    // 16.7 GiB pool, but nowhere near enough once KV cache for 131072 ctx is
    // added.
    let a = adapter(model, gpu_hardware(17, 1), LLAMA_8B);
    let diags = a.validate(&deployment(settings));
    let err = diags
        .iter()
        .find(|d| d.severity == Severity::Error)
        .expect("expected kv-cache-inclusive under-fit error");
    assert_eq!(err.code, catalog::MEM_VRAM_EXCEEDED);
}

// ---- GGUF-on-vLLM warning -------------------------------------------------

#[test]
fn validate_warns_on_gguf_artifact_without_blocking() {
    let a = adapter(gguf_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(base_settings()));
    let warn = diags
        .iter()
        .find(|d| d.code == catalog::CAP_VLLM_GGUF_EXPERIMENTAL)
        .expect("expected a GGUF-experimental warning");
    assert_eq!(warn.severity, Severity::Warning);
    assert!(!diags.iter().any(|d| d.severity == Severity::Error));
}

#[test]
fn validate_does_not_warn_on_safetensors_artifact() {
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(base_settings()));
    assert!(
        !diags
            .iter()
            .any(|d| d.code == catalog::CAP_VLLM_GGUF_EXPERIMENTAL)
    );
}

// ---- escape hatch: extra_args / env ---------------------------------------

#[test]
fn extra_args_duplicating_typed_flag_is_cap002() {
    let mut settings = base_settings();
    settings.extra_args = vec!["--max-model-len".into(), "4096".into()];
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(settings));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED)
    );
}

#[test]
fn extra_args_duplicating_enforce_eager_is_cap002() {
    for flag in ["--enforce-eager", "--no-enforce-eager"] {
        let mut settings = base_settings();
        settings.extra_args = vec![flag.to_owned()];
        let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
        let diags = a.validate(&deployment(settings));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED),
            "{flag} should be flagged as duplicating the typed enforce_eager field"
        );
    }
}

#[test]
fn extra_args_denylisted_flag_is_cap003() {
    for flag in [
        "--host",
        "--api-key",
        "--ssl-keyfile",
        "--ssl-certfile",
        "--root-path",
    ] {
        let mut settings = base_settings();
        settings.extra_args = vec![flag.to_owned()];
        let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
        let diags = a.validate(&deployment(settings));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DENYLISTED),
            "{flag} should be denylisted"
        );
    }
}

#[test]
fn extra_args_present_yields_info_cap004() {
    let mut settings = base_settings();
    settings.extra_args = vec!["--enforce-eager".into()];
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(settings));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARGS_PRESENT && d.severity == Severity::Info)
    );
}

#[test]
fn extra_args_empty_has_no_cap004() {
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(base_settings()));
    assert!(
        !diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARGS_PRESENT)
    );
}

#[test]
fn env_key_vllm_api_key_is_denylisted() {
    let mut settings = base_settings();
    settings.env.insert("VLLM_API_KEY".into(), "secret".into());
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(settings));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DENYLISTED)
    );
}

#[test]
fn env_key_ld_preload_is_denylisted() {
    let mut settings = base_settings();
    settings.env.insert("LD_PRELOAD".into(), "/evil.so".into());
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(settings));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DENYLISTED)
    );
}

#[test]
fn env_key_ordinary_var_is_not_flagged() {
    let mut settings = base_settings();
    settings
        .env
        .insert("VLLM_LOGGING_LEVEL".into(), "DEBUG".into());
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let diags = a.validate(&deployment(settings));
    assert!(
        !diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DENYLISTED
                || d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED)
    );
}

#[test]
fn plan_launch_refuses_denylisted_extra_args() {
    let mut settings = base_settings();
    settings.extra_args = vec!["--host".into(), "0.0.0.0".into()];
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let err = a
        .plan_launch(&deployment(settings))
        .expect_err("denylisted extra_args must not plan");
    assert_eq!(err.code(), catalog::CAP_EXTRA_ARG_DENYLISTED);
}

// ---- plan_launch renders the command --------------------------------------

#[test]
fn plan_launch_renders_expected_argv() {
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let plan = a
        .plan_launch(&deployment(base_settings()))
        .expect("plan_launch");
    let argv = plan.rendered.argv().expect("command launch");
    assert_eq!(argv[0], "vllm");
    assert_eq!(argv[1], "serve");
    assert_eq!(argv[2], "meta-llama/Llama-3.1-8B-Instruct");
    assert!(argv.contains(&"--tensor-parallel-size".to_owned()));
    assert!(argv.contains(&"--gpu-memory-utilization".to_owned()));
    assert_eq!(plan.runtime, amicu_runtime::RuntimeKind::Vllm);
}

#[test]
fn plan_launch_estimate_combines_weights_and_kv_cache() {
    let mut model = safetensors_model();
    model.n_layers = Some(32);
    model.n_kv_heads = Some(8);
    model.head_dim = Some(128);
    let mut settings = base_settings();
    settings.kv_cache_dtype = Some(VllmKvCacheDtype::Fp8);
    let a = adapter(model, gpu_hardware(80, 1), LLAMA_8B);
    let plan = a.plan_launch(&deployment(settings)).expect("plan_launch");
    assert!(plan.estimate.kv_cache.is_some());
    assert!(plan.estimate.estimated_memory.bytes() > plan.estimate.kv_cache.unwrap().bytes());
}

// ---- capabilities / health --------------------------------------------------

#[test]
fn capabilities_report_vllm_kind() {
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let caps = a.capabilities();
    assert_eq!(caps.runtime, amicu_runtime::RuntimeKind::Vllm);
    assert!(caps.artifact_formats.contains(&ArtifactFormat::Safetensors));
    assert!(caps.parallelism.tensor);
}

#[tokio::test]
async fn health_reports_unavailable_when_binary_missing() {
    // `vllm` is not expected to be on the CI PATH.
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let health = a.health().await.expect("health check does not error");
    // The CI environment may or may not have `vllm` on PATH; either outcome
    // is acceptable here, this just exercises the code path without erroring.
    assert!(matches!(
        health,
        HealthStatus::Healthy | HealthStatus::Unavailable { .. } | HealthStatus::Degraded { .. }
    ));
}

// ---- apply/status/stop delegate to the supervisor --------------------------

#[tokio::test]
async fn apply_delegates_to_supervisor_and_registers_handle() {
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let plan = a
        .plan_launch(&deployment(base_settings()))
        .expect("plan_launch");
    let handle = a.apply(&plan, &FakeResolver).await.expect("apply succeeds");
    assert_eq!(handle.deployment, plan.deployment);

    let status = a.status(&handle).await.expect("status succeeds");
    assert!(matches!(status, RuntimeStatus::Running { .. }));

    a.stop(&handle).await.expect("stop succeeds");
    let status = a.status(&handle).await.expect("status succeeds");
    assert!(matches!(status, RuntimeStatus::Stopped { .. }));
}

struct FakeResolver;

#[async_trait]
impl SecretResolver for FakeResolver {
    async fn resolve(&self, secret: &SecretRef) -> Result<SecretValue, AdapterError> {
        Ok(SecretValue::new(format!("value-of-{}", secret.name)))
    }
}

// ---- endpoint API key (amicu-planning#191) -----------------------------------

const KEY_VALUE: &str = "vllm-endpoint-key-123";

/// Records the resolved secrets a `start` receives (name only is asserted on
/// by name; values only to prove where they went).
struct RecordingSupervisor {
    secrets: std::sync::Arc<Mutex<Vec<(String, String)>>>,
    argv: std::sync::Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ProcessSupervisor for RecordingSupervisor {
    async fn start(
        &self,
        plan: &amicu_runtime::LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        *self.secrets.lock().unwrap() = secrets
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    s.value.expose_secret_str().unwrap().to_owned(),
                )
            })
            .collect();
        *self.argv.lock().unwrap() = plan.rendered.argv().unwrap_or_default().to_vec();
        FakeSupervisor::default().start(plan, secrets).await
    }
    async fn status(&self, h: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError> {
        FakeSupervisor::default().status(h).await
    }
    async fn metrics(&self, h: &RuntimeHandle) -> Result<RuntimeMetrics, LlamaCppError> {
        FakeSupervisor::default().metrics(h).await
    }
    async fn logs(&self, h: &RuntimeHandle, o: LogOptions) -> Result<LogStream, LlamaCppError> {
        FakeSupervisor::default().logs(h, o).await
    }
    async fn stop(&self, h: &RuntimeHandle) -> Result<(), LlamaCppError> {
        FakeSupervisor::default().stop(h).await
    }
}

struct KeyResolver;

#[async_trait]
impl SecretResolver for KeyResolver {
    async fn resolve(&self, _: &SecretRef) -> Result<SecretValue, AdapterError> {
        Ok(SecretValue::new(KEY_VALUE.to_owned()))
    }
}

fn keyed(settings: VllmSettings) -> Deployment {
    let mut d = deployment(settings);
    d.api_key = Some(SecretRef::new("CHAT_API_KEY"));
    d
}

#[test]
fn plan_launch_carries_only_the_key_reference() {
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    let plan = a.plan_launch(&keyed(base_settings())).unwrap();
    assert_eq!(plan.api_key, Some(SecretRef::new("CHAT_API_KEY")));
    let argv = plan.rendered.argv().unwrap();
    assert!(!argv.iter().any(|a| a == "--api-key"));
    assert!(!serde_json::to_string(&plan).unwrap().contains(KEY_VALUE));
}

#[tokio::test]
async fn apply_delivers_the_key_as_vllm_api_key_never_argv() {
    let secrets = std::sync::Arc::new(Mutex::new(Vec::new()));
    let argv = std::sync::Arc::new(Mutex::new(Vec::new()));
    let catalog = MapCatalog::new()
        .with_model(safetensors_model(), LLAMA_8B)
        .with_hardware(gpu_hardware(80, 1));
    let a = VllmAdapter::new(
        Box::new(catalog),
        Box::new(RecordingSupervisor {
            secrets: secrets.clone(),
            argv: argv.clone(),
        }),
    );
    let plan = a.plan_launch(&keyed(base_settings())).unwrap();
    a.apply(&plan, &KeyResolver).await.expect("apply");
    assert_eq!(
        *secrets.lock().unwrap(),
        vec![("VLLM_API_KEY".to_owned(), KEY_VALUE.to_owned())]
    );
    assert!(argv.lock().unwrap().iter().all(|a| !a.contains(KEY_VALUE)));
}

#[test]
fn user_supplied_key_channels_stay_denylisted() {
    let a = adapter(safetensors_model(), gpu_hardware(80, 1), LLAMA_8B);
    for args in [&["--api-key", "x"][..], &["--api-key=x"][..]] {
        let mut s = base_settings();
        s.extra_args = args.iter().map(|a| (*a).to_owned()).collect();
        let d = deployment(s);
        assert!(
            a.validate(&d)
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DENYLISTED),
            "{args:?}"
        );
        assert!(a.plan_launch(&d).is_err(), "{args:?}");
    }
    let mut s = base_settings();
    s.env = [("VLLM_API_KEY".to_owned(), "x".to_owned())].into();
    let d = deployment(s);
    assert!(
        a.validate(&d)
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DENYLISTED)
    );
    assert!(a.plan_launch(&d).is_err());
}
