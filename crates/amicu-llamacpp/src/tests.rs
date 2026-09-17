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
    LaunchPlan, LogOptions, LogStream, Platform, RenderedLaunch, ResourceEstimate, RuntimeAdapter,
    RuntimeHandle, RuntimeKind, RuntimeMetrics, RuntimeStatus,
};

use crate::binary::{BinaryConfig, CommandRunner, LlamaVersion, detect_binary};
use crate::command::render_command;
use crate::download::download_gguf;
use crate::error::LlamaCppError;
use crate::health::HealthProbe;
use crate::service::{
    CommandExec, ExecOutput, HostExec, LaunchdManager, ServiceManager, ServiceSpec, ServiceState,
    SystemdManager,
};
use crate::supervisor::{ProcessSupervisor, ResolvedSecret, ServiceSupervisor};
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
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
    }
}

/// `deployment()` with `extra_args` set to `args` (amicu-planning#150).
fn deployment_with_extra_args(args: &[&str]) -> Deployment {
    let mut d = deployment();
    let Runtime::LlamaCpp(s) = &mut d.runtime else {
        unreachable!("deployment() always builds a llama_cpp runtime");
    };
    s.extra_args = args.iter().map(|a| (*a).to_owned()).collect();
    d
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

// ---- extra_args diagnostics (amicu-planning#150) ----------------------------

#[test]
fn validate_reports_cap004_when_extra_args_present() {
    let diags = fitting_adapter().validate(&deployment_with_extra_args(&["--jinja"]));
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code, catalog::CAP_EXTRA_ARGS_PRESENT);
    assert_eq!(diags[0].severity, amicu_core::diagnostic::Severity::Info);
}

#[test]
fn validate_reports_no_cap004_when_extra_args_empty() {
    let diags = fitting_adapter().validate(&deployment());
    assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
}

#[test]
fn validate_reports_cap002_for_duplicate_flag_space_form() {
    // `--ctx-size 1`: flag and value as two separate extra_args entries.
    let diags = fitting_adapter().validate(&deployment_with_extra_args(&["--ctx-size", "1"]));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-002, got {diags:?}"
    );
}

#[test]
fn validate_reports_cap002_for_duplicate_flag_equals_form() {
    // `--ctx-size=1`: flag and value in one extra_args entry.
    let diags = fitting_adapter().validate(&deployment_with_extra_args(&["--ctx-size=1"]));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-002, got {diags:?}"
    );
}

#[test]
fn validate_reports_cap003_for_denylisted_flag() {
    let diags =
        fitting_adapter().validate(&deployment_with_extra_args(&["--api-key", "sk-whatever"]));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DENYLISTED
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-003, got {diags:?}"
    );
}

// ---- plan_launch ------------------------------------------------------------

#[test]
fn plan_launch_renders_visible_command() {
    let plan = fitting_adapter().plan_launch(&deployment()).expect("plan");
    assert_eq!(plan.runtime, amicu_runtime::RuntimeKind::LlamaCpp);

    let RenderedLaunch::Command { argv, text } = &plan.rendered else {
        panic!("expected a command");
    };
    assert!(text.starts_with("llama-server -m "));
    assert!(text.contains("tiny.Q4_K_M.gguf"));
    assert!(text.contains("--port 8080"));
    assert!(text.contains("--ctx-size 8192"));
    assert!(text.contains("--n-gpu-layers -1"));
    // argv is the structured source of truth, not a shell-split of `text`.
    assert_eq!(argv[0], "llama-server");
    assert_eq!(argv[1], "-m");
    assert!(argv[2].ends_with("tiny.Q4_K_M.gguf"));
    assert!(argv.contains(&"--port".to_owned()) && argv.contains(&"8080".to_owned()));
}

#[test]
fn render_command_appends_extra_args_after_typed_flags() {
    let settings = {
        let Runtime::LlamaCpp(s) = deployment_with_extra_args(&["--mmproj", "/m.gguf"]).runtime
        else {
            unreachable!()
        };
        s
    };
    let (argv, text) = render_command(Path::new("/models/tiny.gguf"), &settings);
    // Typed flags first, then extra_args verbatim, in declaration order.
    let typed_end = argv
        .iter()
        .position(|a| a == "-1")
        .expect("n_gpu_layers value present");
    assert_eq!(&argv[typed_end + 1..], ["--mmproj", "/m.gguf"]);
    assert!(text.ends_with("--mmproj /m.gguf"));
}

#[test]
fn plan_launch_errors_on_cap002_duplicate_flag() {
    let err = fitting_adapter()
        .plan_launch(&deployment_with_extra_args(&["--port=9999"]))
        .expect_err("duplicate typed flag must be rejected");
    assert_eq!(err.code(), catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED);
}

#[test]
fn plan_launch_errors_on_cap003_denylisted_flag() {
    let err = fitting_adapter()
        .plan_launch(&deployment_with_extra_args(&["--host", "0.0.0.0"]))
        .expect_err("denylisted flag must be rejected");
    assert_eq!(err.code(), catalog::CAP_EXTRA_ARG_DENYLISTED);
}

#[test]
fn plan_launch_succeeds_with_only_informational_extra_args() {
    let plan = fitting_adapter()
        .plan_launch(&deployment_with_extra_args(&["--jinja"]))
        .expect("non-conflicting extra_args must not block plan_launch");
    let RenderedLaunch::Command { argv, .. } = &plan.rendered else {
        panic!("expected a command");
    };
    assert!(argv.contains(&"--jinja".to_owned()));
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

// ---- ServiceSupervisor over fake ports --------------------------------------

/// A `ServiceManager` returning canned state and recording start/stop.
struct FakeManager {
    state: ServiceState,
    start_ok: bool,
    logs: Vec<String>,
    started: Mutex<Option<ServiceSpec>>,
    stopped: Mutex<bool>,
}

impl FakeManager {
    fn new(state: ServiceState) -> Self {
        Self {
            state,
            start_ok: true,
            logs: vec!["server listening".into()],
            started: Mutex::new(None),
            stopped: Mutex::new(false),
        }
    }
    fn failing_start() -> Self {
        Self {
            start_ok: false,
            ..Self::new(ServiceState::Inactive)
        }
    }
}

impl ServiceManager for FakeManager {
    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError> {
        if !self.start_ok {
            return Err(LlamaCppError::Supervisor("launchctl load failed".into()));
        }
        *self.started.lock().unwrap() = Some(spec.clone());
        Ok(())
    }
    fn state(&self, _label: &str) -> Result<ServiceState, LlamaCppError> {
        Ok(self.state)
    }
    fn stop(&self, _label: &str) -> Result<(), LlamaCppError> {
        *self.stopped.lock().unwrap() = true;
        Ok(())
    }
    fn logs(&self, _label: &str, options: LogOptions) -> Result<Vec<String>, LlamaCppError> {
        let mut lines = self.logs.clone();
        if let Some(n) = options.tail {
            let keep = (n as usize).min(lines.len());
            lines = lines.split_off(lines.len() - keep);
        }
        Ok(lines)
    }
}

struct FakeProbe {
    ready: bool,
}

#[async_trait]
impl HealthProbe for FakeProbe {
    async fn is_ready(&self, _endpoint: &str) -> bool {
        self.ready
    }
}

fn command_plan() -> LaunchPlan {
    fitting_adapter().plan_launch(&deployment()).expect("plan")
}

fn manifest_plan() -> LaunchPlan {
    LaunchPlan {
        deployment: DeploymentId::new("support_chat_local"),
        runtime: RuntimeKind::LlamaCpp,
        rendered: RenderedLaunch::Manifest {
            text: "manifest".into(),
        },
        settings: Runtime::LlamaCpp(LlamaCppSettings {
            context_length: 8192,
            port: 8080,
            n_gpu_layers: -1,
            extra_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        }),
        secret_refs: vec![],
        estimate: ResourceEstimate {
            estimated_memory: ByteSize::from_bytes(0),
            gpu_count: 0,
        },
    }
}

#[tokio::test]
async fn supervisor_start_yields_running_healthy() {
    let sup = ServiceSupervisor::new(
        FakeManager::new(ServiceState::Active),
        FakeProbe { ready: true },
    );
    let handle = sup.start(&command_plan(), &[]).await.expect("start");
    assert_eq!(handle.id, "io.amicu.llama.support_chat_local");
    assert_eq!(handle.endpoint, "http://127.0.0.1:8080");

    let status = sup.status(&handle).await.expect("status");
    assert!(status.is_running());
    let RuntimeStatus::Running { detail } = status else {
        panic!("expected running");
    };
    assert_eq!(detail, "healthy");
}

#[tokio::test]
async fn supervisor_status_degraded_when_endpoint_not_ready() {
    let sup = ServiceSupervisor::new(
        FakeManager::new(ServiceState::Active),
        FakeProbe { ready: false },
    );
    let handle = sup.start(&command_plan(), &[]).await.expect("start");
    let RuntimeStatus::Running { detail } = sup.status(&handle).await.unwrap() else {
        panic!("expected running (degraded)");
    };
    assert!(detail.starts_with("degraded"), "detail was {detail:?}");
}

#[tokio::test]
async fn supervisor_status_failed_when_service_crashed() {
    let sup = ServiceSupervisor::new(
        FakeManager::new(ServiceState::Failed),
        FakeProbe { ready: true },
    );
    let handle = sup.start(&command_plan(), &[]).await.expect("start");
    assert!(matches!(
        sup.status(&handle).await.unwrap(),
        RuntimeStatus::Failed { .. }
    ));
}

#[tokio::test]
async fn supervisor_status_stopped_when_inactive() {
    let sup = ServiceSupervisor::new(
        FakeManager::new(ServiceState::Inactive),
        FakeProbe { ready: false },
    );
    let handle = sup.start(&command_plan(), &[]).await.expect("start");
    assert!(matches!(
        sup.status(&handle).await.unwrap(),
        RuntimeStatus::Stopped { .. }
    ));
}

#[tokio::test]
async fn supervisor_logs_and_stop_and_metrics() {
    let manager = std::sync::Arc::new(FakeManager::new(ServiceState::Active));
    let sup = ServiceSupervisor::new(SharedManager(manager.clone()), FakeProbe { ready: true });
    let handle = sup.start(&command_plan(), &[]).await.expect("start");

    let logs = sup
        .logs(&handle, LogOptions::default())
        .await
        .expect("logs");
    assert_eq!(logs.lines(), ["server listening".to_owned()]);

    let _ = sup.metrics(&handle).await.expect("metrics");

    sup.stop(&handle).await.expect("stop");
    assert!(*manager.stopped.lock().unwrap());
}

/// Shares one `FakeManager` so a test can inspect it after moving it into the
/// supervisor.
struct SharedManager(std::sync::Arc<FakeManager>);

impl ServiceManager for SharedManager {
    fn start(&self, spec: &ServiceSpec) -> Result<(), LlamaCppError> {
        self.0.start(spec)
    }
    fn state(&self, label: &str) -> Result<ServiceState, LlamaCppError> {
        self.0.state(label)
    }
    fn stop(&self, label: &str) -> Result<(), LlamaCppError> {
        self.0.stop(label)
    }
    fn logs(&self, label: &str, options: LogOptions) -> Result<Vec<String>, LlamaCppError> {
        self.0.logs(label, options)
    }
}

#[tokio::test]
async fn supervisor_start_failure_is_typed_error() {
    let sup = ServiceSupervisor::new(FakeManager::failing_start(), FakeProbe { ready: true });
    let err = sup
        .start(&command_plan(), &[])
        .await
        .expect_err("start fails");
    assert!(matches!(err, LlamaCppError::Supervisor(_)));
    assert_eq!(err.code(), catalog::PROVIDER_CAPACITY_UNAVAILABLE);
    assert!(err.retryable());
}

#[tokio::test]
async fn supervisor_rejects_manifest_launch() {
    let sup = ServiceSupervisor::new(
        FakeManager::new(ServiceState::Active),
        FakeProbe { ready: true },
    );
    let err = sup.start(&manifest_plan(), &[]).await.expect_err("no argv");
    assert!(matches!(err, LlamaCppError::Supervisor(_)));
}

#[tokio::test]
async fn supervisor_injects_secrets_into_env_not_argv() {
    let manager = std::sync::Arc::new(FakeManager::new(ServiceState::Active));
    let sup = ServiceSupervisor::new(SharedManager(manager.clone()), FakeProbe { ready: true });
    let secret = ResolvedSecret {
        name: "api_key".into(),
        value: SecretValue::new("s3cr3t"),
    };
    sup.start(&command_plan(), std::slice::from_ref(&secret))
        .await
        .expect("start");

    let spec = manager.started.lock().unwrap().clone().expect("started");
    assert_eq!(spec.env, vec![("api_key".to_owned(), "s3cr3t".to_owned())]);
    // The value is in env, never in argv.
    assert!(spec.argv.iter().all(|a| !a.contains("s3cr3t")));
}

// ---- env merge / collision (amicu-planning#150) ------------------------------

fn deployment_with_env(pairs: &[(&str, &str)]) -> Deployment {
    let mut d = deployment();
    let Runtime::LlamaCpp(s) = &mut d.runtime else {
        unreachable!("deployment() always builds a llama_cpp runtime");
    };
    s.env = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    d
}

fn plan_with_env(pairs: &[(&str, &str)]) -> LaunchPlan {
    fitting_adapter()
        .plan_launch(&deployment_with_env(pairs))
        .expect("plan")
}

#[tokio::test]
async fn supervisor_merges_settings_env_before_secrets() {
    let manager = std::sync::Arc::new(FakeManager::new(ServiceState::Active));
    let sup = ServiceSupervisor::new(SharedManager(manager.clone()), FakeProbe { ready: true });
    let plan = plan_with_env(&[("GGML_CUDA_DISABLE_GRAPHS", "1")]);
    let secret = ResolvedSecret {
        name: "api_key".into(),
        value: SecretValue::new("s3cr3t"),
    };
    sup.start(&plan, std::slice::from_ref(&secret))
        .await
        .expect("start");

    let spec = manager.started.lock().unwrap().clone().expect("started");
    // settings.env first, then resolved secrets.
    assert_eq!(
        spec.env,
        vec![
            ("GGML_CUDA_DISABLE_GRAPHS".to_owned(), "1".to_owned()),
            ("api_key".to_owned(), "s3cr3t".to_owned()),
        ]
    );
}

#[tokio::test]
async fn supervisor_hard_errors_on_settings_env_secret_collision() {
    // `secret_refs` is always empty for llama.cpp today (no HCL path
    // produces one yet), so this constructs a `ResolvedSecret` directly to
    // cover the collision path regardless.
    let manager = std::sync::Arc::new(FakeManager::new(ServiceState::Active));
    let sup = ServiceSupervisor::new(SharedManager(manager.clone()), FakeProbe { ready: true });
    let plan = plan_with_env(&[("HF_TOKEN", "not-a-secret-value")]);
    let secret = ResolvedSecret {
        name: "HF_TOKEN".into(),
        value: SecretValue::new("real-secret"),
    };
    let err = sup
        .start(&plan, std::slice::from_ref(&secret))
        .await
        .expect_err("colliding env key must hard-error");
    assert!(matches!(err, LlamaCppError::Supervisor(_)));
    assert!(err.to_string().contains("HF_TOKEN"));
    // Never even attempted to start the service.
    assert!(manager.started.lock().unwrap().is_none());
}

// ---- launchd / systemd managers over a fake CommandExec ---------------------

struct FakeExec {
    out: ExecOutput,
    calls: Mutex<Vec<Vec<String>>>,
}

impl FakeExec {
    fn new(success: bool, stdout: &str) -> Self {
        Self {
            out: ExecOutput {
                success,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            },
            calls: Mutex::new(Vec::new()),
        }
    }
    fn last(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

impl CommandExec for FakeExec {
    fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, LlamaCppError> {
        let mut call = vec![program.to_owned()];
        call.extend(args.iter().map(|a| (*a).to_owned()));
        self.calls.lock().unwrap().push(call);
        Ok(self.out.clone())
    }
}

/// Lets a test share one `FakeExec` with a manager and still inspect it.
impl<T: CommandExec> CommandExec for std::sync::Arc<T> {
    fn exec(&self, program: &str, args: &[&str]) -> Result<ExecOutput, LlamaCppError> {
        (**self).exec(program, args)
    }
}

fn service_spec() -> ServiceSpec {
    ServiceSpec {
        label: "io.amicu.llama.support_chat_local".into(),
        argv: vec![
            "llama-server".into(),
            "-m".into(),
            "/models/tiny.gguf".into(),
            "--port".into(),
            "8080".into(),
        ],
        env: vec![],
    }
}

#[test]
fn launchd_start_writes_plist_and_loads() {
    let tmp = TempDir::new("launchd");
    let agents = tmp.file("agents");
    let logs = tmp.file("logs");
    let manager = LaunchdManager::new(FakeExec::new(true, ""), &agents, &logs);
    manager.start(&service_spec()).expect("start");

    let plist = agents.join("io.amicu.llama.support_chat_local.plist");
    let body = fs::read_to_string(&plist).expect("plist written");
    assert!(body.contains("<string>llama-server</string>"));
    assert!(body.contains("<string>/models/tiny.gguf</string>"));
    assert!(body.contains("<key>RunAtLoad</key>"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&plist).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "plist must be owner-only");
    }
}

#[test]
fn launchd_stop_removes_plist() {
    let tmp = TempDir::new("launchd-stop");
    let agents = tmp.file("agents");
    let logs = tmp.file("logs");
    let manager = LaunchdManager::new(FakeExec::new(true, ""), &agents, &logs);
    let spec = service_spec();
    manager.start(&spec).expect("start");

    let plist = agents.join("io.amicu.llama.support_chat_local.plist");
    assert!(plist.exists(), "plist should exist after start");

    manager.stop(&spec.label).expect("stop");
    assert!(!plist.exists(), "plist must be removed after stop");

    // A second stop (plist already gone) must not fail.
    manager.stop(&spec.label).expect("stop is idempotent");
}

#[test]
fn launchd_state_maps_pid_exit_and_missing() {
    let active = LaunchdManager::new(FakeExec::new(true, "{\n\t\"PID\" = 1234;\n};"), "/a", "/l");
    assert_eq!(active.state("x").unwrap(), ServiceState::Active);

    let failed = LaunchdManager::new(
        FakeExec::new(true, "{\n\t\"LastExitStatus\" = 78;\n};"),
        "/a",
        "/l",
    );
    assert_eq!(failed.state("x").unwrap(), ServiceState::Failed);

    let gone = LaunchdManager::new(FakeExec::new(false, ""), "/a", "/l");
    assert_eq!(gone.state("x").unwrap(), ServiceState::Inactive);
}

#[test]
fn systemd_start_builds_transient_unit_with_setenv() {
    let exec = std::sync::Arc::new(FakeExec::new(true, ""));
    let mut spec = service_spec();
    spec.env = vec![("api_key".into(), "v".into())];
    SystemdManager::new(exec.clone())
        .start(&spec)
        .expect("start");
    let call = exec.last();
    assert_eq!(call[0], "systemd-run");
    assert!(call.contains(&"--user".to_owned()));
    assert!(call.contains(&"--unit=io.amicu.llama.support_chat_local".to_owned()));
    assert!(call.contains(&"--setenv=api_key=v".to_owned()));
    // The argv follows the options, program first.
    let prog = call
        .iter()
        .position(|a| a == "llama-server")
        .expect("program");
    assert!(call[prog..].contains(&"/models/tiny.gguf".to_owned()));
}

#[test]
fn systemd_state_maps_is_active_output() {
    assert_eq!(
        SystemdManager::new(FakeExec::new(true, "active\n"))
            .state("x")
            .unwrap(),
        ServiceState::Active
    );
    assert_eq!(
        SystemdManager::new(FakeExec::new(false, "failed\n"))
            .state("x")
            .unwrap(),
        ServiceState::Failed
    );
    assert_eq!(
        SystemdManager::new(FakeExec::new(false, "inactive\n"))
            .state("x")
            .unwrap(),
        ServiceState::Inactive
    );
}

#[test]
fn systemd_logs_use_journalctl_with_tail() {
    let exec = std::sync::Arc::new(FakeExec::new(true, "line1\nline2\n"));
    let lines = SystemdManager::new(exec.clone())
        .logs(
            "io.amicu.llama.support_chat_local",
            LogOptions {
                follow: false,
                tail: Some(5),
            },
        )
        .expect("logs");
    assert_eq!(lines, vec!["line1".to_owned(), "line2".to_owned()]);
    let call = exec.last();
    assert_eq!(call[0], "journalctl");
    assert!(call.contains(&"--lines=5".to_owned()));
}

// ---- real child process (ignored by default) --------------------------------

#[test]
#[ignore = "spawns a real child process"]
fn host_exec_runs_a_real_process() {
    let out = HostExec.exec("echo", &["amicu"]).expect("echo runs");
    assert!(out.success);
    assert!(out.stdout.contains("amicu"));
}
