//! Hermetic tests: no real binary, network, or processes.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;

use amicu_core::ByteSize;
use amicu_core::adapter::{
    Adapter, AdapterError, AdapterErrorKind, AdapterId, DescribeCapabilities, HealthStatus,
};
use amicu_core::diagnostic::{Category, catalog};
use amicu_core::id::{DeploymentId, HardwareProfileId, ModelId};
use amicu_core::resource::{
    Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Deployment, GgufVariant,
    HardwareProfile, LlamaCppSettings, Modality, Model, ModelSource, Quantization, Runtime,
    SourceRegistry,
};
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use amicu_registry::{
    ArtifactRef, ArtifactSelector, DownloadResult, ModelMetadata, ModelQuery, ModelSourceAdapter,
    ModelSummary, SourceCapabilities, SourceError,
};
use amicu_runtime::{
    LogOptions, LogStream, Platform, RenderedLaunch, RuntimeAdapter, RuntimeHandle, RuntimeMetrics,
    RuntimeStatus,
};

use crate::binary::{BinaryConfig, CommandRunner, LlamaVersion, detect_binary};
use crate::download::download_gguf;
use crate::error::LlamaCppError;
use crate::supervisor::{ProcessSupervisor, ResolvedSecret};
use crate::{LlamaCppAdapter, MapCatalog};

const GIB: u64 = 1024 * 1024 * 1024;

// ---- fixtures ---------------------------------------------------------------

fn gguf_model(sizes: &[(Quantization, &str, u64)]) -> Model {
    Model {
        id: ModelId::new("tiny_llama"),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "org/tiny-llama".into(),
        },
        architecture: "llama".into(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Gguf,
            quantization: None,
        },
        context_length: Some(8192),
        variants: sizes
            .iter()
            .map(|(q, file, size)| GgufVariant {
                quantization: *q,
                file: (*file).into(),
                size: Some(ByteSize::from_bytes(*size)),
            })
            .collect(),
    }
}

fn safetensors_model() -> Model {
    let mut m = gguf_model(&[(Quantization::Q4KM, "m.Q4_K_M.gguf", 4 * GIB)]);
    m.artifact.format = ArtifactFormat::Safetensors;
    m.variants.clear();
    m
}

fn apple_silicon(mem_gib: u64) -> HardwareProfile {
    HardwareProfile {
        id: HardwareProfileId::new("m3_max"),
        provider: "local".into(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::AppleSilicon,
            model: "m3_max".into(),
            count: 1,
            memory: ByteSize::from_bytes(mem_gib * GIB),
        }),
        cpu: None,
        memory: ByteSize::from_bytes(mem_gib * GIB),
        interconnect: None,
        price: None,
    }
}

fn deployment() -> Deployment {
    Deployment {
        id: DeploymentId::new("support_chat_local"),
        model: ModelId::new("tiny_llama"),
        hardware_profile: HardwareProfileId::new("m3_max"),
        runtime: Runtime::LlamaCpp(LlamaCppSettings {
            context_length: 8192,
            port: 8080,
            n_gpu_layers: -1,
        }),
    }
}

/// Adapter over a catalog that fits (16 GiB Mac, ~4 GiB Q4_K_M model).
fn fitting_adapter() -> LlamaCppAdapter {
    let catalog = MapCatalog::new()
        .with_model(
            gguf_model(&[(Quantization::Q4KM, "tiny.Q4_K_M.gguf", 4 * GIB)]),
            7_000_000_000,
        )
        .with_hardware(apple_silicon(16));
    LlamaCppAdapter::new(Box::new(catalog), Box::new(FakeSupervisor::default()))
}

// ---- fake binary runner -----------------------------------------------------

struct FakeRunner {
    found: Option<PathBuf>,
    output: String,
}

impl FakeRunner {
    fn found(version_line: &str) -> Self {
        Self {
            found: Some(PathBuf::from("/usr/local/bin/llama-server")),
            output: version_line.to_string(),
        }
    }
    fn missing() -> Self {
        Self {
            found: None,
            output: String::new(),
        }
    }
}

impl CommandRunner for FakeRunner {
    fn which(&self, _name: &str) -> Option<PathBuf> {
        self.found.clone()
    }
    fn version_output(&self, _path: &Path) -> Result<String, LlamaCppError> {
        Ok(self.output.clone())
    }
}

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
            endpoint: "http://127.0.0.1:8080".into(),
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

// ---- fake secret resolver ---------------------------------------------------

struct FakeResolver;

#[async_trait]
impl SecretResolver for FakeResolver {
    async fn resolve(&self, secret: &SecretRef) -> Result<SecretValue, AdapterError> {
        Ok(SecretValue::new(format!("value-of-{}", secret.name)))
    }
}

// ---- fake model source (resume) ---------------------------------------------

/// A minimal source that writes a real file and models resume: a `flaky`
/// source drops the first attempt after a partial write; the retry resumes.
struct FakeSource {
    id: AdapterId,
    size: u64,
    flaky: bool,
}

impl FakeSource {
    fn new(size: u64, flaky: bool) -> Self {
        Self {
            id: AdapterId::new("hf"),
            size,
            flaky,
        }
    }
}

fn put_bytes(dest: &Path, offset: u64, count: u64) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(offset > 0)
        .write(true)
        .truncate(offset == 0)
        .open(dest)
        .unwrap();
    file.write_all(&vec![0xA5u8; usize::try_from(count).unwrap()])
        .unwrap();
}

#[async_trait]
impl Adapter for FakeSource {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }
    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(HealthStatus::Healthy)
    }
}

impl DescribeCapabilities for FakeSource {
    fn describe(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
}

#[async_trait]
impl ModelSourceAdapter for FakeSource {
    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            registry: SourceRegistry::HuggingFace,
            supports_search: false,
            supports_private: false,
            gguf: true,
        }
    }
    async fn search(&self, _q: &ModelQuery) -> Result<Vec<ModelSummary>, AdapterError> {
        Ok(vec![])
    }
    async fn metadata(&self, id: &ModelId) -> Result<ModelMetadata, AdapterError> {
        Ok(ModelMetadata {
            id: id.clone(),
            architecture: "llama".into(),
            total_parameters: 1_100_000_000,
            license: "apache-2.0".into(),
            modalities: vec![Modality::Text],
            quantizations: vec![Quantization::Q4KM],
            variants: vec![],
        })
    }
    async fn resolve_artifact(
        &self,
        id: &ModelId,
        _sel: &ArtifactSelector,
    ) -> Result<ArtifactRef, AdapterError> {
        Ok(ArtifactRef {
            model: id.clone(),
            uri: "mem://tiny/w.gguf".into(),
            format: ArtifactFormat::Gguf,
            quantization: Some(Quantization::Q4KM),
            size: ByteSize::from_bytes(self.size),
            checksum: "sha256:w".into(),
        })
    }
    async fn download(&self, a: &ArtifactRef, dest: &Path) -> Result<DownloadResult, AdapterError> {
        let total = a.size.bytes();
        let existing = fs::metadata(dest).map_or(0, |m| m.len());
        if self.flaky && existing == 0 && total > 1 {
            let partial = total / 2;
            put_bytes(dest, 0, partial);
            return Err(SourceError::Download("connection reset".into()).into_adapter_error());
        }
        let resumed = existing > 0;
        let remaining = total.saturating_sub(existing);
        if remaining > 0 {
            put_bytes(dest, existing, remaining);
        }
        Ok(DownloadResult {
            path: dest.to_path_buf(),
            bytes_downloaded: remaining,
            total_bytes: total,
            resumed,
        })
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("amicu-task-007-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---- capabilities -----------------------------------------------------------

#[test]
fn capabilities_reflect_llama_cpp() {
    let caps = fitting_adapter().capabilities();
    assert_eq!(caps.runtime, amicu_runtime::RuntimeKind::LlamaCpp);
    assert_eq!(caps.artifact_formats, vec![ArtifactFormat::Gguf]);
    assert!(caps.platforms.contains(&Platform::Cpu));
    assert!(caps.platforms.contains(&Platform::Metal));
    assert!(!caps.parallelism.tensor);
    assert_eq!(caps.parallelism.max_gpus, 1);
    assert!(caps.lora);
    assert!(caps.tool_calling);
    assert!(caps.quantizations.contains(&Quantization::Q4KM));
    // Discoverable as plain data.
    assert_eq!(fitting_adapter().describe()["runtime"], "llama_cpp");
}

// ---- validate ---------------------------------------------------------------

#[test]
fn validate_passes_when_model_fits() {
    let diags = fitting_adapter().validate(&deployment());
    assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
}

#[test]
fn validate_reports_memory_when_model_does_not_fit() {
    // A 40 GiB weight file on an 8 GiB Mac cannot fit.
    let catalog = MapCatalog::new()
        .with_model(
            gguf_model(&[(Quantization::Q4KM, "big.Q4_K_M.gguf", 40 * GIB)]),
            70_000_000_000,
        )
        .with_hardware(apple_silicon(8));
    let adapter = LlamaCppAdapter::new(Box::new(catalog), Box::new(FakeSupervisor::default()));

    let diags = adapter.validate(&deployment());
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code.category(), Category::Mem);
}

#[test]
fn validate_reports_cap_for_non_gguf_artifact() {
    let catalog = MapCatalog::new()
        .with_model(safetensors_model(), 7_000_000_000)
        .with_hardware(apple_silicon(16));
    let adapter = LlamaCppAdapter::new(Box::new(catalog), Box::new(FakeSupervisor::default()));

    let diags = adapter.validate(&deployment());
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code, catalog::CAP_RUNTIME_ARTIFACT_MISMATCH);
}

#[test]
fn validate_reports_ref_for_unknown_model() {
    let adapter = LlamaCppAdapter::new(
        Box::new(MapCatalog::new().with_hardware(apple_silicon(16))),
        Box::new(FakeSupervisor::default()),
    );
    let diags = adapter.validate(&deployment());
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code, catalog::REF_UNKNOWN_RESOURCE);
}

// ---- plan_launch ------------------------------------------------------------

#[test]
fn plan_launch_renders_visible_command() {
    let plan = fitting_adapter().plan_launch(&deployment()).expect("plan");
    assert_eq!(plan.runtime, amicu_runtime::RuntimeKind::LlamaCpp);

    let RenderedLaunch::Command { text } = &plan.rendered else {
        panic!("expected a command");
    };
    assert!(text.starts_with("llama-server -m "));
    assert!(text.contains("tiny.Q4_K_M.gguf"));
    assert!(text.contains("--port 8080"));
    assert!(text.contains("--ctx-size 8192"));
    assert!(text.contains("--n-gpu-layers -1"));
}

#[test]
fn plan_launch_command_is_secret_free_and_estimate_present() {
    let plan = fitting_adapter().plan_launch(&deployment()).expect("plan");
    assert!(plan.secret_refs.is_empty());
    assert!(!plan.rendered.as_text().to_lowercase().contains("secret"));
    // Weights + KV band is a positive estimate; one accelerator is occupied.
    assert!(plan.estimate.estimated_memory.bytes() > 4 * GIB);
    assert_eq!(plan.estimate.gpu_count, 1);
}

#[test]
fn plan_launch_errors_on_unknown_hardware() {
    let adapter = LlamaCppAdapter::new(
        Box::new(MapCatalog::new().with_model(
            gguf_model(&[(Quantization::Q4KM, "m.gguf", 4 * GIB)]),
            7_000_000_000,
        )),
        Box::new(FakeSupervisor::default()),
    );
    let err = adapter.plan_launch(&deployment()).expect_err("no hardware");
    assert_eq!(err.code(), catalog::REF_UNKNOWN_RESOURCE);
}

// ---- binary detection -------------------------------------------------------

#[test]
fn version_parses_build_number() {
    assert_eq!(
        LlamaVersion::parse("version: 3901 (a1b2c3d)\nbuilt with clang").unwrap(),
        LlamaVersion::new(3901)
    );
    assert_eq!(
        LlamaVersion::parse("llama-server version b4200").unwrap(),
        LlamaVersion::new(4200)
    );
    assert!(matches!(
        LlamaVersion::parse("no build here").unwrap_err(),
        LlamaCppError::UnparseableVersion(_)
    ));
}

#[test]
fn detect_binary_found_and_version_ok() {
    let runner = FakeRunner::found("version: 3901 (abc)");
    let cfg = BinaryConfig {
        minimum: LlamaVersion::new(3000),
        ..BinaryConfig::default()
    };
    let info = detect_binary(&runner, &cfg).expect("found");
    assert_eq!(info.version, LlamaVersion::new(3901));
    assert_eq!(info.path, PathBuf::from("/usr/local/bin/llama-server"));
}

#[test]
fn detect_binary_not_found() {
    let err = detect_binary(&FakeRunner::missing(), &BinaryConfig::default()).expect_err("missing");
    assert!(matches!(err, LlamaCppError::BinaryNotFound));
    assert_eq!(err.code(), catalog::PROVIDER_CAPACITY_UNAVAILABLE);
}

#[test]
fn detect_binary_version_too_old() {
    let runner = FakeRunner::found("version: 2000 (old)");
    let cfg = BinaryConfig {
        minimum: LlamaVersion::new(3000),
        ..BinaryConfig::default()
    };
    let err = detect_binary(&runner, &cfg).expect_err("too old");
    assert!(matches!(err, LlamaCppError::VersionTooOld { .. }));
    assert_eq!(err.code(), catalog::CAP_RUNTIME_ARTIFACT_MISMATCH);
}

#[tokio::test]
async fn health_reports_binary_state() {
    let adapter = fitting_adapter().with_runner(Box::new(FakeRunner::found("version: 3901 (a)")));
    assert!(adapter.health().await.unwrap().is_healthy());

    let missing = fitting_adapter().with_runner(Box::new(FakeRunner::missing()));
    assert!(matches!(
        missing.health().await.unwrap(),
        HealthStatus::Unavailable { .. }
    ));
}

// ---- download with resume ---------------------------------------------------

#[tokio::test]
async fn download_completes_in_one_pass() {
    let source = FakeSource::new(64, false);
    let tmp = TempDir::new("download");
    let dest = tmp.file("w.gguf");
    let result = download_gguf(&source, &ModelId::new("tiny_llama"), &selector(), &dest, 3)
        .await
        .expect("download");
    assert!(!result.resumed);
    assert_eq!(result.total_bytes, 64);
    assert_eq!(fs::metadata(&dest).unwrap().len(), 64);
}

#[tokio::test]
async fn download_resumes_partial_after_transient_failure() {
    let source = FakeSource::new(64, true);
    let tmp = TempDir::new("resume");
    let dest = tmp.file("w.gguf");

    let result = download_gguf(&source, &ModelId::new("tiny_llama"), &selector(), &dest, 3)
        .await
        .expect("resumed download");

    // The retry resumed from the 32-byte partial rather than restarting.
    assert!(result.resumed);
    assert_eq!(result.bytes_downloaded, 32);
    assert_eq!(result.total_bytes, 64);
    assert_eq!(fs::metadata(&dest).unwrap().len(), 64);
}

fn selector() -> ArtifactSelector {
    ArtifactSelector {
        format: ArtifactFormat::Gguf,
        quantization: Some(Quantization::Q4KM),
        max_size: None,
    }
}

// ---- RuntimeAdapter async path (via FakeSupervisor) -------------------------

#[tokio::test]
async fn apply_status_stop_round_trip() {
    let adapter = fitting_adapter();
    let plan = adapter.plan_launch(&deployment()).expect("plan");

    let handle = adapter.apply(&plan, &FakeResolver).await.expect("apply");
    assert_eq!(handle.deployment, DeploymentId::new("support_chat_local"));
    assert_eq!(handle.endpoint, "http://127.0.0.1:8080");

    assert!(adapter.status(&handle).await.unwrap().is_running());

    let logs = adapter.logs(&handle, LogOptions::default()).await.unwrap();
    assert_eq!(logs.lines().len(), 1);

    let _ = adapter.metrics(&handle).await.unwrap();

    adapter.stop(&handle).await.expect("stop");
    assert!(matches!(
        adapter.status(&handle).await.unwrap(),
        RuntimeStatus::Stopped { .. }
    ));
}
