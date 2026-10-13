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
    SourceRegistry, SpeculativeSettings,
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
                sha256: None,
            })
            .collect(),
        n_layers: None,
        n_kv_heads: None,
        head_dim: None,
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
            memory_bandwidth_gbps: None,
        }),
        cpu: None,
        memory: ByteSize::from_bytes(mem_gib * GIB),
        interconnect: None,
        price: None,
    }
}

fn deployment() -> Deployment {
    Deployment {
        scaling: None,
        id: DeploymentId::new("support_chat_local"),
        model: ModelId::new("tiny_llama"),
        hardware_profile: HardwareProfileId::new("m3_max"),
        runtime: Runtime::LlamaCpp(LlamaCppSettings {
            context_length: 8192,
            port: 8080,
            n_gpu_layers: -1,
            ..Default::default()
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
    let diags = fitting_adapter().validate(&deployment_with_extra_args(&["--verbose"]));
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

// H1 regression: `-m`/`--model` was missing from the typed-flag table, so
// `extra_args = ["--model", "/tmp/evil.gguf"]` only raised the informational
// CAP-004 while llama-server's last-flag-wins argv parsing would silently
// load an unresolved model instead of the one amicu selected.
#[test]
fn validate_reports_cap002_for_model_flag_duplicate() {
    let diags =
        fitting_adapter().validate(&deployment_with_extra_args(&["--model", "/tmp/evil.gguf"]));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-002 for --model, got {diags:?}"
    );
}

// M1 regression: `flag_token` only recognized `--` (long) flags, so the
// single-dash short aliases llama-server also accepts (`-c`, `-ngl`, `-m`)
// bypassed CAP-002 entirely.
#[test]
fn validate_reports_cap002_for_short_flag_aliases() {
    for short in [
        vec!["-c".to_owned(), "999".to_owned()],
        vec!["-ngl".to_owned(), "0".to_owned()],
        vec!["-m".to_owned(), "x".to_owned()],
    ] {
        let args: Vec<&str> = short.iter().map(String::as_str).collect();
        let diags = fitting_adapter().validate(&deployment_with_extra_args(&args));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-002 for {args:?}, got {diags:?}"
        );
    }
}

// ---- env key diagnostics (amicu-planning#150 review hardening: H2) ---------
//
// llama-server reads `LLAMA_ARG_<X>` environment variables as equivalent to
// `--<x>` on the CLI, so a settings `env` entry is a second channel to
// exactly the conflicts `extra_args` already guards against.

#[test]
fn validate_reports_cap003_for_llama_arg_host_env_key() {
    let diags = fitting_adapter().validate(&deployment_with_env(&[("LLAMA_ARG_HOST", "0.0.0.0")]));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DENYLISTED
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-003 for LLAMA_ARG_HOST, got {diags:?}"
    );
}

#[test]
fn validate_reports_cap002_for_llama_arg_ctx_size_env_key() {
    let diags = fitting_adapter().validate(&deployment_with_env(&[("LLAMA_ARG_CTX_SIZE", "1")]));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-002 for LLAMA_ARG_CTX_SIZE, got {diags:?}"
    );
}

// ---- amicu-planning#151: sampling/KV-cache/batching/flash-attn typed-flag
// diagnostics -----------------------------------------------------------

#[test]
fn validate_reports_cap002_for_temp_flag_duplicate_in_extra_args() {
    let diags = fitting_adapter().validate(&deployment_with_extra_args(&["--temp", "0.9"]));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-002 for --temp, got {diags:?}"
    );
}

#[test]
fn validate_reports_cap002_for_llama_arg_temp_env_key() {
    let diags = fitting_adapter().validate(&deployment_with_env(&[("LLAMA_ARG_TEMP", "0.9")]));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-002 for LLAMA_ARG_TEMP, got {diags:?}"
    );
}

#[test]
fn validate_reports_cap002_for_new_typed_flags_and_short_aliases() {
    for args in [
        vec!["--top-p".to_owned(), "0.9".to_owned()],
        vec!["--top-k".to_owned(), "40".to_owned()],
        vec!["--min-p".to_owned(), "0.05".to_owned()],
        vec!["--repeat-penalty".to_owned(), "1.1".to_owned()],
        vec!["--cache-type-k".to_owned(), "q8_0".to_owned()],
        vec!["-ctk".to_owned(), "q8_0".to_owned()],
        vec!["--cache-type-v".to_owned(), "q8_0".to_owned()],
        vec!["-ctv".to_owned(), "q8_0".to_owned()],
        vec!["--batch-size".to_owned(), "2048".to_owned()],
        vec!["-b".to_owned(), "2048".to_owned()],
        vec!["--ubatch-size".to_owned(), "512".to_owned()],
        vec!["-ub".to_owned(), "512".to_owned()],
        vec!["--parallel".to_owned(), "4".to_owned()],
        vec!["-np".to_owned(), "4".to_owned()],
        vec!["--flash-attn".to_owned(), "on".to_owned()],
        vec!["-fa".to_owned(), "on".to_owned()],
    ] {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let diags = fitting_adapter().validate(&deployment_with_extra_args(&refs));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-002 for {args:?}, got {diags:?}"
        );
    }
}

// ---- amicu-planning#151 PR B: chat template / jinja / reasoning-format
// typed-flag diagnostics ---------------------------------------------------

#[test]
fn validate_reports_cap002_for_chat_template_jinja_reasoning_format_flags() {
    for args in [
        vec!["--chat-template".to_owned(), "{{ messages }}".to_owned()],
        vec![
            "--chat-template-file".to_owned(),
            "/models/chat.jinja".to_owned(),
        ],
        vec!["--jinja".to_owned()],
        vec!["--no-jinja".to_owned()],
        vec!["--reasoning-format".to_owned(), "auto".to_owned()],
    ] {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let diags = fitting_adapter().validate(&deployment_with_extra_args(&refs));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-002 for {args:?}, got {diags:?}"
        );
    }
}

#[test]
fn validate_reports_cap002_for_llama_arg_chat_template_jinja_reasoning_format_env_keys() {
    // No ENV_FLAG_OVERRIDES entry is needed for the first three - the
    // mechanical LLAMA_ARG_<X> -> --<x> rule already matches their real
    // flags. LLAMA_ARG_THINK is the exception: it is the real env var
    // `--reasoning-format`'s `add_opt` registers via `set_env` in
    // `common/arg.cpp` (named after the internal `params.think` field, not
    // the flag), so it needs the explicit override in
    // `command::ENV_FLAG_OVERRIDES` - there is no `LLAMA_ARG_REASONING_FORMAT`.
    for key in [
        "LLAMA_ARG_CHAT_TEMPLATE",
        "LLAMA_ARG_CHAT_TEMPLATE_FILE",
        "LLAMA_ARG_JINJA",
        "LLAMA_ARG_THINK",
    ] {
        let diags = fitting_adapter().validate(&deployment_with_env(&[(key, "x")]));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-002 for {key}, got {diags:?}"
        );
    }
}

// Env vars whose LLAMA_ARG_ mirror does not mechanically match its flag
// (`--batch-size`, `--ubatch-size`, `--parallel`, `--reasoning-format`) -
// see `command::ENV_FLAG_OVERRIDES`.
#[test]
fn validate_reports_cap002_for_irregular_env_flag_overrides() {
    for key in [
        "LLAMA_ARG_BATCH",
        "LLAMA_ARG_UBATCH",
        "LLAMA_ARG_N_PARALLEL",
        "LLAMA_ARG_THINK",
    ] {
        let diags = fitting_adapter().validate(&deployment_with_env(&[(key, "4")]));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-002 for {key}, got {diags:?}"
        );
    }
}

#[test]
fn validate_reports_no_diagnostic_for_ordinary_env_key() {
    let diags =
        fitting_adapter().validate(&deployment_with_env(&[("GGML_CUDA_DISABLE_GRAPHS", "1")]));
    assert!(
        diags.is_empty(),
        "GGML_CUDA_DISABLE_GRAPHS is not LLAMA_ARG_*-prefixed and isn't denylisted, \
         expected no diagnostics, got {diags:?}"
    );
}

#[test]
fn validate_reports_cap003_for_denylisted_env_key() {
    for key in [
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "LD_LIBRARY_PATH",
    ] {
        let diags = fitting_adapter().validate(&deployment_with_env(&[(key, "/tmp/evil.so")]));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DENYLISTED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-003 for {key}, got {diags:?}"
        );
    }
}

// ---- amicu-planning#151 PR D: multimodal typed-flag diagnostics ----------

#[test]
fn validate_reports_cap002_for_multimodal_flags() {
    for args in [
        vec!["--mmproj".to_owned(), "/m.gguf".to_owned()],
        vec!["-mm".to_owned(), "/m.gguf".to_owned()],
        vec!["--mmproj-offload".to_owned()],
        vec!["--no-mmproj-offload".to_owned()],
    ] {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let diags = fitting_adapter().validate(&deployment_with_extra_args(&refs));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-002 for {args:?}, got {diags:?}"
        );
    }
}

#[test]
fn validate_reports_cap002_for_llama_arg_mmproj_env_keys() {
    // No ENV_FLAG_OVERRIDES entry is needed for either key - `common/arg.cpp`
    // sets each env var after its own long flag spelling, so the mechanical
    // LLAMA_ARG_<X> -> --<x> rule already matches (amicu-planning#151 PR D).
    for key in ["LLAMA_ARG_MMPROJ", "LLAMA_ARG_MMPROJ_OFFLOAD"] {
        let diags = fitting_adapter().validate(&deployment_with_env(&[(key, "x")]));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-002 for {key}, got {diags:?}"
        );
    }
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
fn render_command_emits_new_typed_flags_when_set() {
    let settings = LlamaCppSettings {
        context_length: 32768,
        port: 8000,
        n_gpu_layers: -1,
        temperature: Some(0.6),
        top_p: Some(0.9),
        top_k: Some(40),
        min_p: Some(0.05),
        repeat_penalty: Some(1.1),
        cache_type_k: Some(amicu_core::resource::KvCacheType::Q8_0),
        cache_type_v: Some(amicu_core::resource::KvCacheType::F16),
        batch_size: Some(2048),
        ubatch_size: Some(512),
        parallel_slots: Some(4),
        flash_attention: Some(amicu_core::resource::FlashAttention::Auto),
        ..Default::default()
    };
    let (argv, text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);

    let pairs = |flag: &str| -> Option<String> {
        argv.iter()
            .position(|a| a == flag)
            .map(|i| argv[i + 1].clone())
    };
    assert_eq!(pairs("--temp").as_deref(), Some("0.6"));
    assert_eq!(pairs("--top-p").as_deref(), Some("0.9"));
    assert_eq!(pairs("--top-k").as_deref(), Some("40"));
    assert_eq!(pairs("--min-p").as_deref(), Some("0.05"));
    assert_eq!(pairs("--repeat-penalty").as_deref(), Some("1.1"));
    assert_eq!(pairs("--cache-type-k").as_deref(), Some("q8_0"));
    assert_eq!(pairs("--cache-type-v").as_deref(), Some("f16"));
    assert_eq!(pairs("--batch-size").as_deref(), Some("2048"));
    assert_eq!(pairs("--ubatch-size").as_deref(), Some("512"));
    assert_eq!(pairs("--parallel").as_deref(), Some("4"));
    assert_eq!(pairs("--flash-attn").as_deref(), Some("auto"));
    // Typed flags precede extra_args (empty here), in the documented order.
    assert!(text.contains("--temp 0.6"));
    assert!(text.contains("--flash-attn auto"));
}

#[test]
fn render_command_omits_unset_new_fields() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    for flag in [
        "--temp",
        "--top-p",
        "--top-k",
        "--min-p",
        "--repeat-penalty",
        "--cache-type-k",
        "--cache-type-v",
        "--batch-size",
        "--ubatch-size",
        "--parallel",
        "--flash-attn",
    ] {
        assert!(
            !argv.contains(&flag.to_owned()),
            "unexpected {flag} in {argv:?}"
        );
    }
}

#[test]
fn render_command_formats_f32_as_plain_decimal_not_scientific() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        temperature: Some(0.6),
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    let i = argv.iter().position(|a| a == "--temp").unwrap();
    assert_eq!(argv[i + 1], "0.6");
}

#[test]
fn render_command_appends_extra_args_after_typed_flags() {
    // `--mmproj` used to be the canonical "still untyped" example here
    // (amicu-planning#151 PR D types it) - `--lora` remains untyped, so it
    // still demonstrates plain argv-order pass-through.
    let settings = {
        let Runtime::LlamaCpp(s) = deployment_with_extra_args(&["--lora", "/m.gguf"]).runtime
        else {
            unreachable!()
        };
        s
    };
    let (argv, text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    // Typed flags first, then extra_args verbatim, in declaration order.
    let typed_end = argv
        .iter()
        .position(|a| a == "-1")
        .expect("n_gpu_layers value present");
    // `--metrics` (always rendered) sits between the typed flags and extra_args.
    assert_eq!(&argv[typed_end + 1..], ["--metrics", "--lora", "/m.gguf"]);
    assert!(text.ends_with("--metrics --lora /m.gguf"));
}

#[test]
fn render_command_always_enables_the_metrics_endpoint_once() {
    let settings = |extra: &[&str]| {
        let Runtime::LlamaCpp(s) = deployment_with_extra_args(extra).runtime else {
            unreachable!()
        };
        s
    };
    let count = |extra: &[&str]| {
        let (argv, _) = render_command(Path::new("/models/tiny.gguf"), None, &settings(extra));
        argv.iter().filter(|a| *a == "--metrics").count()
    };
    assert_eq!(count(&[]), 1);
    assert_eq!(count(&["--metrics"]), 1, "no duplicate when user passes it");
}

// ---- amicu-planning#151 PR B: chat template / jinja / reasoning-format
// renderer ---------------------------------------------------------------

#[test]
fn render_command_emits_chat_template_jinja_reasoning_format_when_set() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        chat_template: Some("{{ messages }}".to_owned()),
        jinja: Some(true),
        reasoning_format: Some(amicu_core::resource::ReasoningFormat::Deepseek),
        ..Default::default()
    };
    let (argv, text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);

    let pairs = |flag: &str| -> Option<String> {
        argv.iter()
            .position(|a| a == flag)
            .map(|i| argv[i + 1].clone())
    };
    assert_eq!(pairs("--chat-template").as_deref(), Some("{{ messages }}"));
    assert!(argv.contains(&"--jinja".to_owned()));
    assert_eq!(pairs("--reasoning-format").as_deref(), Some("deepseek"));
    assert!(text.contains("--reasoning-format deepseek"));
}

#[test]
fn render_command_emits_chat_template_file_when_set() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        chat_template_file: Some("/models/chat.jinja".to_owned()),
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    let i = argv
        .iter()
        .position(|a| a == "--chat-template-file")
        .expect("--chat-template-file present");
    assert_eq!(argv[i + 1], "/models/chat.jinja");
    assert!(!argv.contains(&"--chat-template".to_owned()));
}

#[test]
fn render_command_omits_both_jinja_flags_when_unset() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        jinja: None,
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    assert!(!argv.contains(&"--jinja".to_owned()));
    assert!(!argv.contains(&"--no-jinja".to_owned()));
}

// llama-server DOES have `--no-jinja` (paired with `--jinja` in one
// `add_opt`, `common/arg.cpp`) - an explicit `jinja = false` renders it,
// rather than rendering nothing, so it overrides an inherited
// `LLAMA_ARG_JINJA=1` from the environment instead of silently deferring
// to it.
#[test]
fn render_command_emits_no_jinja_when_explicitly_false() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        jinja: Some(false),
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    assert!(argv.contains(&"--no-jinja".to_owned()));
    assert!(!argv.contains(&"--jinja".to_owned()));
}

// `common/chat.cpp`'s `common_reasoning_format_name` renders
// `COMMON_REASONING_FORMAT_DEEPSEEK_LEGACY` as the hyphenated
// `"deepseek-legacy"`, not the underscored HCL/schema spelling
// `deepseek_legacy` - llama-server rejects the underscored form with
// "Unknown reasoning format".
#[test]
fn render_command_emits_hyphenated_deepseek_legacy_reasoning_format() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        reasoning_format: Some(amicu_core::resource::ReasoningFormat::DeepseekLegacy),
        ..Default::default()
    };
    let (argv, text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    let i = argv
        .iter()
        .position(|a| a == "--reasoning-format")
        .expect("--reasoning-format present");
    assert_eq!(argv[i + 1], "deepseek-legacy");
    assert!(text.contains("--reasoning-format deepseek-legacy"));
}

#[test]
fn render_command_renders_neither_chat_template_flag_when_both_are_set() {
    // AMICU-CAP-005: an ambiguous pair - render_command itself refuses to
    // guess which one wins, independent of the adapter-level diagnostic.
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        chat_template: Some("{{ messages }}".to_owned()),
        chat_template_file: Some("/models/chat.jinja".to_owned()),
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    assert!(!argv.contains(&"--chat-template".to_owned()));
    assert!(!argv.contains(&"--chat-template-file".to_owned()));
}

#[test]
fn render_command_omits_unset_chat_template_jinja_reasoning_format_fields() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    for flag in [
        "--chat-template",
        "--chat-template-file",
        "--jinja",
        "--no-jinja",
        "--reasoning-format",
    ] {
        assert!(
            !argv.contains(&flag.to_owned()),
            "unexpected {flag} in {argv:?}"
        );
    }
}

// ---- amicu-planning#151 PR D: multimodal projector renderer --------------

#[test]
fn render_command_emits_multimodal_projector_and_offload_when_set() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        multimodal: Some(amicu_core::resource::MultimodalSettings {
            projector: Some("/models/mmproj-f16.gguf".to_owned()),
            offload: Some(true),
        }),
        ..Default::default()
    };
    let (argv, text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    let i = argv
        .iter()
        .position(|a| a == "--mmproj")
        .expect("--mmproj present");
    assert_eq!(argv[i + 1], "/models/mmproj-f16.gguf");
    assert!(argv.contains(&"--mmproj-offload".to_owned()));
    assert!(!argv.contains(&"--no-mmproj-offload".to_owned()));
    assert!(text.contains("--mmproj /models/mmproj-f16.gguf"));
    assert!(text.contains("--mmproj-offload"));
}

#[test]
fn render_command_emits_no_mmproj_offload_when_explicitly_false() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        multimodal: Some(amicu_core::resource::MultimodalSettings {
            projector: None,
            offload: Some(false),
        }),
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    assert!(!argv.contains(&"--mmproj".to_owned()));
    assert!(argv.contains(&"--no-mmproj-offload".to_owned()));
    assert!(!argv.contains(&"--mmproj-offload".to_owned()));
}

#[test]
fn render_command_omits_multimodal_flags_when_unset() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    for flag in ["--mmproj", "-mm", "--mmproj-offload", "--no-mmproj-offload"] {
        assert!(
            !argv.contains(&flag.to_owned()),
            "unexpected {flag} in {argv:?}"
        );
    }
}

// ---- amicu-planning#151 PR B: AMICU-CAP-005 chat template conflict --------

#[test]
fn validate_reports_cap005_when_both_chat_template_fields_are_set() {
    let mut d = deployment();
    let Runtime::LlamaCpp(s) = &mut d.runtime else {
        unreachable!("deployment() always builds a llama_cpp runtime");
    };
    s.chat_template = Some("{{ messages }}".to_owned());
    s.chat_template_file = Some("/models/chat.jinja".to_owned());

    let diags = fitting_adapter().validate(&d);
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_CHAT_TEMPLATE_CONFLICT
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-005, got {diags:?}"
    );
}

#[test]
fn plan_launch_errors_on_cap005_chat_template_conflict() {
    let mut d = deployment();
    let Runtime::LlamaCpp(s) = &mut d.runtime else {
        unreachable!("deployment() always builds a llama_cpp runtime");
    };
    s.chat_template = Some("{{ messages }}".to_owned());
    s.chat_template_file = Some("/models/chat.jinja".to_owned());

    let err = fitting_adapter()
        .plan_launch(&d)
        .expect_err("both chat_template fields set must be rejected");
    assert_eq!(err.code(), catalog::CAP_CHAT_TEMPLATE_CONFLICT);
}

#[test]
fn plan_launch_succeeds_with_only_chat_template_file_set() {
    let mut d = deployment();
    let Runtime::LlamaCpp(s) = &mut d.runtime else {
        unreachable!("deployment() always builds a llama_cpp runtime");
    };
    s.chat_template_file = Some("/models/chat.jinja".to_owned());

    let plan = fitting_adapter()
        .plan_launch(&d)
        .expect("a single chat-template field must not block plan_launch");
    let RenderedLaunch::Command { argv, .. } = &plan.rendered else {
        panic!("expected a command");
    };
    assert!(argv.contains(&"--chat-template-file".to_owned()));
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
        .plan_launch(&deployment_with_extra_args(&["--verbose"]))
        .expect("non-conflicting extra_args must not block plan_launch");
    let RenderedLaunch::Command { argv, .. } = &plan.rendered else {
        panic!("expected a command");
    };
    assert!(argv.contains(&"--verbose".to_owned()));
}

#[test]
fn plan_launch_errors_on_model_flag_duplicate() {
    let err = fitting_adapter()
        .plan_launch(&deployment_with_extra_args(&["--model", "/tmp/evil.gguf"]))
        .expect_err("--model duplicate must be rejected");
    assert_eq!(err.code(), catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED);
}

#[test]
fn plan_launch_errors_on_short_flag_alias_duplicate() {
    let err = fitting_adapter()
        .plan_launch(&deployment_with_extra_args(&["-c", "1"]))
        .expect_err("-c duplicate must be rejected");
    assert_eq!(err.code(), catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED);
}

#[test]
fn plan_launch_errors_on_llama_arg_env_key_mirroring_denylisted_flag() {
    let err = fitting_adapter()
        .plan_launch(&deployment_with_env(&[("LLAMA_ARG_HOST", "0.0.0.0")]))
        .expect_err("LLAMA_ARG_HOST must be rejected");
    assert_eq!(err.code(), catalog::CAP_EXTRA_ARG_DENYLISTED);
}

#[test]
fn plan_launch_errors_on_llama_arg_env_key_mirroring_typed_flag() {
    let err = fitting_adapter()
        .plan_launch(&deployment_with_env(&[("LLAMA_ARG_CTX_SIZE", "1")]))
        .expect_err("LLAMA_ARG_CTX_SIZE must be rejected");
    assert_eq!(err.code(), catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED);
}

#[test]
fn plan_launch_errors_on_denylisted_env_key() {
    let err = fitting_adapter()
        .plan_launch(&deployment_with_env(&[("LD_PRELOAD", "/tmp/evil.so")]))
        .expect_err("LD_PRELOAD must be rejected");
    assert_eq!(err.code(), catalog::CAP_EXTRA_ARG_DENYLISTED);
}

#[test]
fn plan_launch_succeeds_with_ordinary_env_key() {
    fitting_adapter()
        .plan_launch(&deployment_with_env(&[("GGML_CUDA_DISABLE_GRAPHS", "1")]))
        .expect("an ordinary tuning env var must not block plan_launch");
}

#[test]
fn plan_launch_command_is_secret_free_and_estimate_present() {
    let plan = fitting_adapter().plan_launch(&deployment()).expect("plan");
    assert_eq!(plan.secret_refs, Vec::new());
    assert!(!plan.rendered.as_text().to_lowercase().contains("secret"));
    // Weights + KV band is a positive estimate; one accelerator is occupied.
    assert!(plan.estimate.estimated_memory.bytes() > 4 * GIB);
    assert_eq!(plan.estimate.gpu_count, 1);
}

// ---- amicu-planning#151: KV-cache estimator wiring --------------------------

#[test]
fn plan_launch_kv_cache_is_none_when_model_has_no_shape_metadata() {
    // deployment()/fitting_adapter()'s model declares no n_layers/n_kv_heads/
    // head_dim - the flat overhead band remains the only estimate, unchanged.
    let plan = fitting_adapter().plan_launch(&deployment()).expect("plan");
    assert_eq!(plan.estimate.kv_cache, None);
}

#[test]
fn validate_and_plan_launch_compute_kv_cache_when_model_shape_and_cache_type_are_set() {
    let mut model = gguf_model(&[(Quantization::Q4KM, "tiny.Q4_K_M.gguf", 4 * GIB)]);
    model.n_layers = Some(32);
    model.n_kv_heads = Some(8);
    model.head_dim = Some(128);
    let catalog = MapCatalog::new()
        .with_model(model, 7_000_000_000)
        .with_hardware(apple_silicon(16));
    let adapter = LlamaCppAdapter::new(Box::new(catalog), Box::new(FakeSupervisor::default()));

    let mut d = deployment();
    let Runtime::LlamaCpp(s) = &mut d.runtime else {
        unreachable!("deployment() always builds a llama_cpp runtime");
    };
    s.context_length = 32768;
    s.cache_type_k = Some(amicu_core::resource::KvCacheType::Q8_0);
    s.cache_type_v = Some(amicu_core::resource::KvCacheType::Q8_0);

    // validate() folds the same estimate into its own fit check without
    // erroring (a fitting model/hardware pair stays diagnostic-free).
    assert_eq!(adapter.validate(&d), Vec::new());

    let plan = adapter.plan_launch(&d).expect("plan");
    // 32768 * 32 * 8 * 128 * (34/32 + 34/32) = 2_281_701_376 bytes exactly.
    assert_eq!(
        plan.estimate.kv_cache,
        Some(amicu_core::ByteSize::from_bytes(2_281_701_376))
    );
    assert!(plan.estimate.estimated_memory.bytes() >= 2_281_701_376);
}

#[test]
fn plan_launch_kv_cache_is_none_when_shape_present_but_no_cache_type_set() {
    // Shape metadata alone (with no cache_type_k/v) must not trigger the
    // estimator - "at least one cache type" is required too.
    let mut model = gguf_model(&[(Quantization::Q4KM, "tiny.Q4_K_M.gguf", 4 * GIB)]);
    model.n_layers = Some(32);
    model.n_kv_heads = Some(8);
    model.head_dim = Some(128);
    let catalog = MapCatalog::new()
        .with_model(model, 7_000_000_000)
        .with_hardware(apple_silicon(16));
    let adapter = LlamaCppAdapter::new(Box::new(catalog), Box::new(FakeSupervisor::default()));

    let plan = adapter.plan_launch(&deployment()).expect("plan");
    assert_eq!(plan.estimate.kv_cache, None);
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
    let result = download_gguf(
        &source,
        &ModelId::new("tiny_llama"),
        &selector(),
        &dest,
        3,
        None,
    )
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

    let result = download_gguf(
        &source,
        &ModelId::new("tiny_llama"),
        &selector(),
        &dest,
        3,
        None,
    )
    .await
    .expect("resumed download");

    // The retry resumed from the 32-byte partial rather than restarting.
    assert!(result.resumed);
    assert_eq!(result.bytes_downloaded, 32);
    assert_eq!(result.total_bytes, 64);
    assert_eq!(fs::metadata(&dest).unwrap().len(), 64);
}

// ---- sha256 verification (amicu-planning#151) -------------------------------
//
// `FakeSource::resolve_artifact` always returns `checksum: "sha256:w".into()`
// (see the fixture above).

#[tokio::test]
async fn download_succeeds_when_expected_sha256_matches() {
    let source = FakeSource::new(64, false);
    let tmp = TempDir::new("sha256-match");
    let dest = tmp.file("w.gguf");
    let result = download_gguf(
        &source,
        &ModelId::new("tiny_llama"),
        &selector(),
        &dest,
        3,
        Some("w"),
    )
    .await
    .expect("matching checksum must not block download");
    assert_eq!(result.total_bytes, 64);
}

#[tokio::test]
async fn download_matches_expected_sha256_case_insensitively() {
    let source = FakeSource::new(64, false);
    let tmp = TempDir::new("sha256-case");
    let dest = tmp.file("w.gguf");
    download_gguf(
        &source,
        &ModelId::new("tiny_llama"),
        &selector(),
        &dest,
        3,
        Some("W"),
    )
    .await
    .expect("checksum comparison must be case-insensitive");
}

#[tokio::test]
async fn download_fails_fast_on_expected_sha256_mismatch_without_writing_bytes() {
    let source = FakeSource::new(64, false);
    let tmp = TempDir::new("sha256-mismatch");
    let dest = tmp.file("w.gguf");
    let err = download_gguf(
        &source,
        &ModelId::new("tiny_llama"),
        &selector(),
        &dest,
        3,
        Some("deadbeef"),
    )
    .await
    .expect_err("mismatched checksum must be rejected before download");
    assert!(matches!(err, LlamaCppError::ChecksumMismatch { .. }));
    assert_eq!(err.code(), catalog::CAP_RUNTIME_ARTIFACT_MISMATCH);
    // Fail-fast: no bytes were ever written for a checksum known to be wrong.
    assert!(!dest.exists());
}

#[tokio::test]
async fn download_skips_verification_when_expected_sha256_is_absent() {
    // None is today's unchanged behavior: no verification at all.
    let source = FakeSource::new(64, false);
    let tmp = TempDir::new("sha256-absent");
    let dest = tmp.file("w.gguf");
    download_gguf(
        &source,
        &ModelId::new("tiny_llama"),
        &selector(),
        &dest,
        3,
        None,
    )
    .await
    .expect("absent expected_sha256 must not verify anything");
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
            ..Default::default()
        }),
        secret_refs: vec![],
        estimate: ResourceEstimate {
            estimated_memory: ByteSize::from_bytes(0),
            gpu_count: 0,
            kv_cache: None,
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
fn systemd_start_passes_env_via_0600_environment_file_not_argv() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("systemd-env");
    let exec = std::sync::Arc::new(FakeExec::new(true, ""));
    let mut spec = service_spec();
    spec.env = vec![("api_key".into(), "s3\"cr\\et".into())];
    let manager = SystemdManager::new(exec.clone(), &dir.0);
    manager.start(&spec).expect("start");
    let call = exec.last();
    assert_eq!(call[0], "systemd-run");
    assert!(call.contains(&"--user".to_owned()));
    assert!(call.contains(&"--unit=io.amicu.llama.support_chat_local".to_owned()));
    // The secret never reaches argv.
    assert!(
        !call
            .iter()
            .any(|a| a.contains("s3") || a.contains("--setenv"))
    );
    let file = &dir.0.join("io.amicu.llama.support_chat_local.env");
    assert!(call.contains(&format!("--property=EnvironmentFile={}", file.display())));
    assert_eq!(
        std::fs::read_to_string(file.as_path()).unwrap(),
        "api_key=\"s3\\\"cr\\\\et\"\n"
    );
    assert_eq!(
        std::fs::metadata(file.as_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    // The argv follows the options, program first.
    let prog = call
        .iter()
        .position(|a| a == "llama-server")
        .expect("program");
    assert!(call[prog..].contains(&"/models/tiny.gguf".to_owned()));
    // Stop removes the env file.
    manager
        .stop("io.amicu.llama.support_chat_local")
        .expect("stop");
    assert!(!file.exists());
}

#[test]
fn systemd_start_failure_removes_the_env_file() {
    let dir = TempDir::new("systemd-env");
    let mut spec = service_spec();
    spec.env = vec![("api_key".into(), "v".into())];
    let manager = SystemdManager::new(FakeExec::new(false, ""), &dir.0);
    assert!(manager.start(&spec).is_err());
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn systemd_stop_removes_the_env_file_even_when_stop_fails() {
    let dir = TempDir::new("systemd-stop");
    let mut spec = service_spec();
    spec.env = vec![("api_key".into(), "v".into())];
    SystemdManager::new(FakeExec::new(true, ""), &dir.0)
        .start(&spec)
        .expect("start");
    let manager = SystemdManager::new(FakeExec::new(false, ""), &dir.0);
    assert!(manager.stop(&spec.label).is_err());
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn systemd_env_file_replaces_preexisting_files_and_symlinks() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("systemd-replace");
    let mut spec = service_spec();
    spec.env = vec![("k".into(), "line1\nline2".into())];
    let file = dir.0.join(format!("{}.env", spec.label));
    // A pre-existing world-readable file is not reused.
    std::fs::write(&file, "old").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let manager = SystemdManager::new(FakeExec::new(true, ""), &dir.0);
    manager.start(&spec).expect("start");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // A planted symlink is replaced, not followed.
    let target = dir.file("target");
    std::fs::write(&target, "untouched").unwrap();
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&target, &file).unwrap();
    manager.start(&spec).expect("start");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    // Multi-line values are written raw inside the quotes.
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "k=\"line1\nline2\"\n"
    );
}

#[test]
fn systemd_refuses_to_write_secrets_without_an_absolute_dir() {
    let mut spec = service_spec();
    spec.env = vec![("k".into(), "v".into())];
    let err = SystemdManager::new(FakeExec::new(true, ""), "")
        .start(&spec)
        .expect_err("no dir");
    assert!(err.to_string().contains("HOME"));
}

#[test]
fn systemd_state_maps_is_active_output() {
    assert_eq!(
        SystemdManager::new(FakeExec::new(true, "active\n"), "/unused-env-dir")
            .state("x")
            .unwrap(),
        ServiceState::Active
    );
    assert_eq!(
        SystemdManager::new(FakeExec::new(false, "failed\n"), "/unused-env-dir")
            .state("x")
            .unwrap(),
        ServiceState::Failed
    );
    assert_eq!(
        SystemdManager::new(FakeExec::new(false, "inactive\n"), "/unused-env-dir")
            .state("x")
            .unwrap(),
        ServiceState::Inactive
    );
}

#[test]
fn systemd_logs_use_journalctl_with_tail() {
    let exec = std::sync::Arc::new(FakeExec::new(true, "line1\nline2\n"));
    let lines = SystemdManager::new(exec.clone(), "/unused-env-dir")
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

// ---- amicu-planning#151 PR C: speculative decoding --------------------------

/// [`gguf_model`] under a caller-chosen id, so one catalog can hold both a
/// primary and a draft model.
fn gguf_model_named(id: &str, sizes: &[(Quantization, &str, u64)]) -> Model {
    let mut m = gguf_model(sizes);
    m.id = ModelId::new(id);
    m
}

/// A draft-model reference with no draft parameters set.
fn draft_spec(model: &str) -> SpeculativeSettings {
    SpeculativeSettings {
        model: ModelId::new(model),
        n_max: None,
        n_min: None,
        p_min: None,
        n_gpu_layers: None,
    }
}

/// `deployment()` carrying `spec` as its `speculative {}` block.
fn speculative_deployment(spec: SpeculativeSettings) -> Deployment {
    let mut d = deployment();
    let Runtime::LlamaCpp(s) = &mut d.runtime else {
        unreachable!("deployment() always builds a llama_cpp runtime");
    };
    s.speculative = Some(spec);
    d
}

/// [`fitting_adapter`] plus a `tiny_draft` model of `draft_bytes`.
fn adapter_with_draft(draft_bytes: u64) -> LlamaCppAdapter {
    let catalog = MapCatalog::new()
        .with_model(
            gguf_model(&[(Quantization::Q4KM, "tiny.Q4_K_M.gguf", 4 * GIB)]),
            7_000_000_000,
        )
        .with_model(
            gguf_model_named(
                "tiny_draft",
                &[(Quantization::Q4KM, "draft.Q4_K_M.gguf", draft_bytes)],
            ),
            1_000_000_000,
        )
        .with_hardware(apple_silicon(16));
    LlamaCppAdapter::new(Box::new(catalog), Box::new(FakeSupervisor::default()))
}

/// A catalog whose draft model comfortably fits alongside the primary one.
fn speculative_adapter() -> LlamaCppAdapter {
    adapter_with_draft(GIB)
}

// -- renderer --

#[test]
fn render_command_emits_speculative_flags_when_set() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        speculative: Some(SpeculativeSettings {
            model: ModelId::new("tiny_draft"),
            n_max: Some(8),
            n_min: Some(2),
            p_min: Some(0.75),
            n_gpu_layers: Some(99),
        }),
        ..Default::default()
    };
    let (argv, text) = render_command(
        Path::new("/models/tiny.gguf"),
        Some(Path::new("/models/draft.gguf")),
        &settings,
    );
    let i = argv
        .iter()
        .position(|a| a == "--model-draft")
        .expect("--model-draft present");
    assert_eq!(
        &argv[i..i + 10],
        [
            "--model-draft",
            "/models/draft.gguf",
            "--spec-draft-n-max",
            "8",
            "--spec-draft-n-min",
            "2",
            "--draft-p-min",
            "0.75",
            "--n-gpu-layers-draft",
            "99",
        ]
    );
    assert!(text.contains("--model-draft /models/draft.gguf"));
    assert!(text.contains("--spec-draft-n-max 8"));
}

// `--draft`/`--draft-n`/`--draft-max` and `--draft-min`/`--draft-n-min` were
// *removed* upstream (`common/arg.cpp`'s "removed params" section aborts on
// them), so the renderer must emit the `--spec-draft-n-*` spellings instead -
// and `--spec-type` is never emitted at all, since llama.cpp infers the
// speculative type from the draft GGUF's own metadata when a draft path is
// set and no type is requested.
#[test]
fn render_command_never_emits_removed_or_redundant_speculative_spellings() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        speculative: Some(SpeculativeSettings {
            model: ModelId::new("tiny_draft"),
            n_max: Some(8),
            n_min: Some(2),
            p_min: Some(0.75),
            n_gpu_layers: Some(99),
        }),
        ..Default::default()
    };
    let (argv, _text) = render_command(
        Path::new("/models/tiny.gguf"),
        Some(Path::new("/models/draft.gguf")),
        &settings,
    );
    for flag in [
        "--draft",
        "--draft-n",
        "--draft-max",
        "--draft-min",
        "--draft-n-min",
        "--spec-draft-model",
        "-md",
        "--spec-type",
    ] {
        assert!(
            !argv.contains(&flag.to_owned()),
            "unexpected {flag} in {argv:?}"
        );
    }
}

#[test]
fn render_command_emits_only_model_draft_when_no_draft_params_are_set() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        speculative: Some(draft_spec("tiny_draft")),
        ..Default::default()
    };
    let (argv, _text) = render_command(
        Path::new("/models/tiny.gguf"),
        Some(Path::new("/models/draft.gguf")),
        &settings,
    );
    assert!(argv.contains(&"--model-draft".to_owned()));
    for flag in [
        "--spec-draft-n-max",
        "--spec-draft-n-min",
        "--draft-p-min",
        "--n-gpu-layers-draft",
    ] {
        assert!(
            !argv.contains(&flag.to_owned()),
            "unexpected {flag} in {argv:?}"
        );
    }
}

#[test]
fn render_command_omits_all_speculative_flags_when_block_is_absent() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    for flag in [
        "--model-draft",
        "--spec-draft-n-max",
        "--spec-draft-n-min",
        "--draft-p-min",
        "--n-gpu-layers-draft",
    ] {
        assert!(
            !argv.contains(&flag.to_owned()),
            "unexpected {flag} in {argv:?}"
        );
    }
}

// The draft path and the block travel together: neither half alone renders
// anything, so a caller that resolved no draft never emits orphaned draft
// parameters llama-server would apply to nothing.
#[test]
fn render_command_omits_draft_params_without_a_resolved_draft_path() {
    let settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        speculative: Some(SpeculativeSettings {
            model: ModelId::new("tiny_draft"),
            n_max: Some(8),
            ..draft_spec("tiny_draft")
        }),
        ..Default::default()
    };
    let (argv, _text) = render_command(Path::new("/models/tiny.gguf"), None, &settings);
    assert!(!argv.contains(&"--model-draft".to_owned()));
    assert!(!argv.contains(&"--spec-draft-n-max".to_owned()));
}

#[test]
fn render_command_appends_extra_args_after_speculative_flags() {
    let mut settings = LlamaCppSettings {
        context_length: 8192,
        port: 8000,
        n_gpu_layers: -1,
        speculative: Some(draft_spec("tiny_draft")),
        ..Default::default()
    };
    settings.extra_args = vec!["--verbose".to_owned()];
    let (argv, _text) = render_command(
        Path::new("/models/tiny.gguf"),
        Some(Path::new("/models/draft.gguf")),
        &settings,
    );
    assert_eq!(argv.last().map(String::as_str), Some("--verbose"));
}

// -- AMICU-CAP-002: extra_args / env duplicating the new flags --

#[test]
fn validate_reports_cap002_for_every_speculative_flag_spelling() {
    for args in [
        vec!["--model-draft", "/tmp/d.gguf"],
        vec!["--spec-draft-model", "/tmp/d.gguf"],
        vec!["-md", "/tmp/d.gguf"],
        vec!["--spec-draft-n-max", "8"],
        vec!["--draft-max", "8"],
        vec!["--draft-n", "8"],
        vec!["--draft", "8"],
        vec!["--spec-draft-n-min", "2"],
        vec!["--draft-min", "2"],
        vec!["--draft-n-min", "2"],
        vec!["--draft-p-min", "0.5"],
        vec!["--spec-draft-p-min", "0.5"],
        vec!["--n-gpu-layers-draft", "10"],
        vec!["--spec-draft-ngl", "10"],
        vec!["--gpu-layers-draft", "10"],
        vec!["-ngld", "10"],
    ] {
        let diags = fitting_adapter().validate(&deployment_with_extra_args(&args));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-002 for {args:?}, got {diags:?}"
        );
    }
}

#[test]
fn validate_reports_cap002_for_speculative_flag_equals_form() {
    let diags = fitting_adapter().validate(&deployment_with_extra_args(&["--model-draft=/tmp/d"]));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED),
        "expected AMICU-CAP-002, got {diags:?}"
    );
}

// Every speculative env var llama-server registers derives, under the
// mechanical `LLAMA_ARG_<X>` -> `--<x>` rule, a spelling already listed in
// `command::TYPED_FLAGS` - so none of them needs an `ENV_FLAG_OVERRIDES`
// entry, and all of them must still be caught.
#[test]
fn validate_reports_cap002_for_speculative_llama_arg_env_keys() {
    for key in [
        "LLAMA_ARG_SPEC_DRAFT_MODEL",
        "LLAMA_ARG_SPEC_DRAFT_N_MAX",
        "LLAMA_ARG_SPEC_DRAFT_N_MIN",
        "LLAMA_ARG_SPEC_DRAFT_P_MIN",
        "LLAMA_ARG_N_GPU_LAYERS_DRAFT",
        "LLAMA_ARG_DRAFT_MAX",
        "LLAMA_ARG_DRAFT_MIN",
    ] {
        let diags = fitting_adapter().validate(&deployment_with_env(&[(key, "1")]));
        assert!(
            diags
                .iter()
                .any(|d| d.code == catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
                    && d.severity == amicu_core::diagnostic::Severity::Error),
            "expected AMICU-CAP-002 for {key}, got {diags:?}"
        );
    }
}

#[test]
fn plan_launch_errors_on_speculative_flag_duplicate_in_extra_args() {
    let err = fitting_adapter()
        .plan_launch(&deployment_with_extra_args(&["--model-draft", "/tmp/d"]))
        .expect_err("--model-draft must be rejected");
    assert_eq!(err.code(), catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED);
}

// -- AMICU-CAP-007: draft model is the primary model --

#[test]
fn validate_reports_cap007_when_draft_model_is_the_primary_model() {
    let d = speculative_deployment(draft_spec("tiny_llama"));
    let diags = speculative_adapter().validate(&d);
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_SPECULATIVE_DRAFT_IS_PRIMARY
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-CAP-007, got {diags:?}"
    );
}

#[test]
fn plan_launch_errors_on_cap007_draft_model_is_the_primary_model() {
    let d = speculative_deployment(draft_spec("tiny_llama"));
    let err = speculative_adapter()
        .plan_launch(&d)
        .expect_err("a self-referential draft model must be rejected");
    assert_eq!(err.code(), catalog::CAP_SPECULATIVE_DRAFT_IS_PRIMARY);
}

// -- missing draft reference: the same AMICU-REF-001 class the primary
// `model` reference raises --

#[test]
fn validate_reports_ref001_for_an_unresolved_draft_model_reference() {
    let d = speculative_deployment(draft_spec("no_such_draft"));
    let diags = speculative_adapter().validate(&d);
    assert!(
        diags.iter().any(|d| d.code == catalog::REF_UNKNOWN_RESOURCE
            && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected AMICU-REF-001, got {diags:?}"
    );
}

#[test]
fn plan_launch_errors_on_an_unresolved_draft_model_reference() {
    let d = speculative_deployment(draft_spec("no_such_draft"));
    let err = speculative_adapter()
        .plan_launch(&d)
        .expect_err("an unresolved draft reference must be rejected");
    assert_eq!(err.code(), catalog::REF_UNKNOWN_RESOURCE);
}

// -- resolution / rendering through the adapter --

#[test]
fn validate_is_clean_for_a_resolvable_draft_model() {
    let d = speculative_deployment(draft_spec("tiny_draft"));
    let diags = speculative_adapter().validate(&d);
    assert!(diags.is_empty(), "expected no diagnostics, got {diags:?}");
}

#[test]
fn plan_launch_renders_the_resolved_draft_gguf_path() {
    let d = speculative_deployment(SpeculativeSettings {
        n_max: Some(5),
        ..draft_spec("tiny_draft")
    });
    let plan = speculative_adapter().plan_launch(&d).expect("plan");
    let RenderedLaunch::Command { argv, .. } = &plan.rendered else {
        panic!("expected a command launch");
    };
    let i = argv
        .iter()
        .position(|a| a == "--model-draft")
        .expect("--model-draft present");
    // The draft GGUF is staged under its own model id, exactly like the
    // primary model's - never the HCL reference.
    assert!(
        argv[i + 1].ends_with("tiny_draft/draft.Q4_K_M.gguf"),
        "unexpected draft path {}",
        argv[i + 1]
    );
    assert_ne!(argv[i + 1], "tiny_draft");
    assert_eq!(argv[i + 2], "--spec-draft-n-max");
    assert_eq!(argv[i + 3], "5");
}

// -- fit: draft weights count toward device memory --

#[test]
fn plan_launch_adds_draft_weights_to_the_memory_estimate() {
    let without = fitting_adapter().plan_launch(&deployment()).expect("plan");
    let with = speculative_adapter()
        .plan_launch(&speculative_deployment(draft_spec("tiny_draft")))
        .expect("plan");
    assert!(
        with.estimate.estimated_memory.bytes() > without.estimate.estimated_memory.bytes(),
        "draft weights must raise the estimate: {} vs {}",
        with.estimate.estimated_memory,
        without.estimate.estimated_memory
    );
}

// The point of the fit change: an otherwise-fitting deployment stops fitting
// once the draft model's weights are counted against the same pool.
#[test]
fn draft_model_weights_can_push_an_otherwise_fitting_config_over_the_limit() {
    // 16 GiB unified, 2 GiB reserve -> 14 GiB available. A 4 GiB primary fits
    // alone; a 4 GiB primary plus an 11 GiB draft does not.
    let d = speculative_deployment(draft_spec("tiny_draft"));
    assert!(
        adapter_with_draft(GIB).validate(&d).is_empty(),
        "a 4 GiB + 1 GiB pair must still fit"
    );
    let diags = adapter_with_draft(11 * GIB).validate(&d);
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::MEM_ARTIFACT_OVERSIZE
                && d.severity == amicu_core::diagnostic::Severity::Error),
        "expected a memory diagnostic once draft weights are counted, got {diags:?}"
    );
}

#[test]
fn a_non_gguf_draft_model_is_a_capability_error() {
    let mut draft = safetensors_model();
    draft.id = ModelId::new("tiny_draft");
    let catalog = MapCatalog::new()
        .with_model(
            gguf_model(&[(Quantization::Q4KM, "tiny.Q4_K_M.gguf", 4 * GIB)]),
            7_000_000_000,
        )
        .with_model(draft, 1_000_000_000)
        .with_hardware(apple_silicon(16));
    let adapter = LlamaCppAdapter::new(Box::new(catalog), Box::new(FakeSupervisor::default()));
    let diags = adapter.validate(&speculative_deployment(draft_spec("tiny_draft")));
    assert!(
        diags
            .iter()
            .any(|d| d.code == catalog::CAP_RUNTIME_ARTIFACT_MISMATCH),
        "expected AMICU-CAP-001 for a safetensors draft, got {diags:?}"
    );
}

// ---- real child process (ignored by default) --------------------------------

#[test]
#[ignore = "spawns a real child process"]
fn host_exec_runs_a_real_process() {
    let out = HostExec.exec("echo", &["amicu"]).expect("echo runs");
    assert!(out.success);
    assert!(out.stdout.contains("amicu"));
}
