//! In-memory fake source and its tests, exercising the full contract.
//!
//! `FakeSource` serves a small catalog with no network. Its `download` writes a
//! real file to `dest` and models resume/retry: a `flaky` source drops the
//! first attempt after a partial write, and the retry resumes from that file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use amicu_core::adapter::{
    Adapter, AdapterError, AdapterErrorKind, AdapterId, DescribeCapabilities, HealthStatus,
    Registry,
};
use amicu_core::diagnostic::{Category, Severity, catalog};
use amicu_core::id::ModelId;
use amicu_core::resource::{ArtifactFormat, GgufVariant, Modality, Quantization, SourceRegistry};
use amicu_core::units::ByteSize;

use super::{
    ArtifactRef, ArtifactSelector, DownloadResult, ModelMetadata, ModelQuery, ModelSourceAdapter,
    ModelSummary, SourceCapabilities, SourceError,
};

struct FakeSource {
    id: AdapterId,
    catalog: Vec<ModelMetadata>,
    /// When set, `download` fails the first attempt after a partial write.
    flaky: bool,
}

impl FakeSource {
    fn new(id: &str) -> Self {
        Self {
            id: AdapterId::new(id),
            catalog: vec![tiny_model()],
            flaky: false,
        }
    }

    fn flaky(mut self) -> Self {
        self.flaky = true;
        self
    }

    fn lookup(&self, id: &ModelId) -> Result<&ModelMetadata, AdapterError> {
        self.catalog
            .iter()
            .find(|m| &m.id == id)
            .ok_or_else(|| SourceError::ModelNotFound(id.clone()).into_adapter_error())
    }
}

/// A 64-byte GGUF model with two quantization variants.
fn tiny_model() -> ModelMetadata {
    ModelMetadata {
        id: ModelId::new("tiny_llama"),
        architecture: "llama".into(),
        total_parameters: 1_100_000_000,
        license: "apache-2.0".into(),
        modalities: vec![Modality::Text],
        quantizations: vec![Quantization::Q4KM, Quantization::Q8_0],
        variants: vec![
            GgufVariant {
                quantization: Quantization::Q4KM,
                file: "tiny-llama.Q4_K_M.gguf".into(),
                size: Some(ByteSize::from_bytes(64)),
            },
            GgufVariant {
                quantization: Quantization::Q8_0,
                file: "tiny-llama.Q8_0.gguf".into(),
                size: Some(ByteSize::from_bytes(120)),
            },
        ],
    }
}

/// Write `count` bytes to `dest`, appending when a partial file exists.
fn put_bytes(dest: &Path, offset: u64, count: u64) -> Result<(), AdapterError> {
    let io = |e: std::io::Error| SourceError::Download(e.to_string()).into_adapter_error();
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(offset > 0)
        .write(true)
        .truncate(offset == 0)
        .open(dest)
        .map_err(io)?;
    let chunk = vec![0xA5u8; usize::try_from(count).expect("tiny fake artifact")];
    file.write_all(&chunk).map_err(io)
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
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl ModelSourceAdapter for FakeSource {
    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            registry: SourceRegistry::HuggingFace,
            supports_search: true,
            supports_private: false,
            gguf: true,
        }
    }

    async fn search(&self, q: &ModelQuery) -> Result<Vec<ModelSummary>, AdapterError> {
        let matches = self
            .catalog
            .iter()
            .filter(|m| {
                q.text.as_ref().is_none_or(|t| {
                    m.id.as_str().contains(t.as_str()) || m.architecture.contains(t.as_str())
                }) && q.modality.is_none_or(|md| m.modalities.contains(&md))
                    && q.format.is_none_or(|f| f == ArtifactFormat::Gguf)
            })
            .map(|m| ModelSummary {
                id: m.id.clone(),
                architecture: m.architecture.clone(),
                modalities: m.modalities.clone(),
                format: ArtifactFormat::Gguf,
            })
            .collect();
        Ok(matches)
    }

    async fn metadata(&self, id: &ModelId) -> Result<ModelMetadata, AdapterError> {
        self.lookup(id).cloned()
    }

    async fn resolve_artifact(
        &self,
        id: &ModelId,
        sel: &ArtifactSelector,
    ) -> Result<ArtifactRef, AdapterError> {
        let meta = self.lookup(id)?;
        let unavailable = || SourceError::ArtifactUnavailable { model: id.clone() };

        if sel.format != ArtifactFormat::Gguf {
            return Err(unavailable().into_adapter_error());
        }
        let variant = meta
            .variants
            .iter()
            .find(|v| sel.quantization.is_none_or(|q| v.quantization == q))
            .ok_or_else(|| unavailable().into_adapter_error())?;

        let size = variant.size.unwrap_or(ByteSize::from_bytes(0));
        if let Some(budget) = sel.max_size
            && size > budget
        {
            return Err(SourceError::WeightFit {
                model: id.clone(),
                required: size,
                budget,
            }
            .into_adapter_error());
        }

        Ok(ArtifactRef {
            model: id.clone(),
            uri: format!("mem://{}/{}", id.as_str(), variant.file),
            format: ArtifactFormat::Gguf,
            quantization: Some(variant.quantization),
            size,
            checksum: format!("sha256:{}", variant.file),
        })
    }

    async fn download(&self, a: &ArtifactRef, dest: &Path) -> Result<DownloadResult, AdapterError> {
        let total = a.size.bytes();
        let existing = fs::metadata(dest).map_or(0, |m| m.len());

        // Flaky sources drop the connection on the first attempt after a
        // partial write, forcing a retry that resumes from the partial file.
        if self.flaky && existing == 0 && total > 1 {
            let partial = total / 2;
            put_bytes(dest, 0, partial)?;
            return Err(
                SourceError::Download(format!("connection reset after {partial} bytes"))
                    .into_adapter_error(),
            );
        }

        let resumed = existing > 0;
        let remaining = total.saturating_sub(existing);
        if remaining > 0 {
            put_bytes(dest, existing, remaining)?;
        }
        Ok(DownloadResult {
            path: dest.to_path_buf(),
            bytes_downloaded: remaining,
            total_bytes: total,
            resumed,
        })
    }
}

/// A unique temp dir under `std::env::temp_dir()`, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("amicu-task-053-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
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

fn registry_with(adapters: Vec<Arc<dyn ModelSourceAdapter>>) -> Registry<dyn ModelSourceAdapter> {
    let mut registry = Registry::new();
    for adapter in adapters {
        registry.register(adapter);
    }
    registry
}

#[test]
fn capabilities_are_plain_data() {
    let source = FakeSource::new("hf");
    let caps = source.capabilities();
    assert!(caps.supports_search);
    assert!(caps.gguf);
    assert!(!caps.supports_private);

    // And they serialize for discovery.
    let described = source.describe();
    assert_eq!(described["registry"], "hugging_face");
    assert_eq!(described["supports_search"], true);
}

#[tokio::test]
async fn search_returns_summaries() {
    let source = FakeSource::new("hf");

    let all = source.search(&ModelQuery::default()).await.expect("search");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, ModelId::new("tiny_llama"));

    let by_text = source
        .search(&ModelQuery {
            text: Some("llama".into()),
            ..ModelQuery::default()
        })
        .await
        .expect("search");
    assert_eq!(by_text.len(), 1);

    let miss = source
        .search(&ModelQuery {
            modality: Some(Modality::Audio),
            ..ModelQuery::default()
        })
        .await
        .expect("search");
    assert!(miss.is_empty());
}

#[tokio::test]
async fn metadata_for_known_id() {
    let source = FakeSource::new("hf");
    let meta = source
        .metadata(&ModelId::new("tiny_llama"))
        .await
        .expect("metadata");
    assert_eq!(meta.architecture, "llama");
    assert_eq!(meta.license, "apache-2.0");
    assert_eq!(meta.quantizations.len(), 2);
    assert_eq!(meta.variants.len(), 2);
}

#[tokio::test]
async fn resolve_artifact_selects_quantization() {
    let source = FakeSource::new("hf");
    let artifact = source
        .resolve_artifact(
            &ModelId::new("tiny_llama"),
            &ArtifactSelector {
                format: ArtifactFormat::Gguf,
                quantization: Some(Quantization::Q8_0),
                max_size: None,
            },
        )
        .await
        .expect("resolve");
    assert_eq!(artifact.quantization, Some(Quantization::Q8_0));
    assert_eq!(artifact.size, ByteSize::from_bytes(120));
    assert!(artifact.uri.starts_with("mem://tiny_llama/"));
}

#[tokio::test]
async fn download_writes_file_and_reports_result() {
    let source = FakeSource::new("hf");
    let tmp = TempDir::new("download");
    let dest = tmp.file("tiny-llama.Q4_K_M.gguf");

    let artifact = source
        .resolve_artifact(
            &ModelId::new("tiny_llama"),
            &ArtifactSelector {
                format: ArtifactFormat::Gguf,
                quantization: Some(Quantization::Q4KM),
                max_size: None,
            },
        )
        .await
        .expect("resolve");

    let result = source.download(&artifact, &dest).await.expect("download");
    assert!(!result.resumed);
    assert_eq!(result.bytes_downloaded, 64);
    assert_eq!(result.total_bytes, 64);
    assert_eq!(fs::metadata(&dest).unwrap().len(), 64);
}

#[tokio::test]
async fn download_resumes_after_a_transient_failure() {
    let source = FakeSource::new("hf").flaky();
    let tmp = TempDir::new("resume");
    let dest = tmp.file("tiny-llama.Q4_K_M.gguf");

    let artifact = source
        .resolve_artifact(
            &ModelId::new("tiny_llama"),
            &ArtifactSelector {
                format: ArtifactFormat::Gguf,
                quantization: Some(Quantization::Q4KM),
                max_size: None,
            },
        )
        .await
        .expect("resolve");

    // First attempt: partial write, then a retryable failure.
    let err = source
        .download(&artifact, &dest)
        .await
        .expect_err("flaky first attempt");
    assert!(err.is_retryable());
    assert_eq!(err.code(), catalog::PROVIDER_DOWNLOAD_FAILED);
    assert_eq!(fs::metadata(&dest).unwrap().len(), 32);

    // Retry: resumes from the 32-byte partial file and completes.
    let result = source.download(&artifact, &dest).await.expect("resumed");
    assert!(result.resumed);
    assert_eq!(result.bytes_downloaded, 32);
    assert_eq!(result.total_bytes, 64);
    assert_eq!(fs::metadata(&dest).unwrap().len(), 64);
}

#[tokio::test]
async fn unknown_id_yields_cap_error() {
    let source = FakeSource::new("hf");
    let missing = ModelId::new("nope");

    let meta_err = source
        .metadata(&missing)
        .await
        .expect_err("unknown metadata");
    assert_eq!(meta_err.code(), catalog::CAP_RUNTIME_ARTIFACT_MISMATCH);
    assert_eq!(meta_err.code().category(), Category::Cap);
    assert!(!meta_err.is_retryable());

    let resolve_err = source
        .resolve_artifact(
            &missing,
            &ArtifactSelector {
                format: ArtifactFormat::Gguf,
                quantization: None,
                max_size: None,
            },
        )
        .await
        .expect_err("unknown resolve");
    assert_eq!(resolve_err.code().category(), Category::Cap);
}

#[tokio::test]
async fn missing_artifact_variant_yields_cap_error() {
    let source = FakeSource::new("hf");
    let err = source
        .resolve_artifact(
            &ModelId::new("tiny_llama"),
            &ArtifactSelector {
                format: ArtifactFormat::Safetensors,
                quantization: None,
                max_size: None,
            },
        )
        .await
        .expect_err("no safetensors artifact");
    assert_eq!(err.code(), catalog::CAP_RUNTIME_ARTIFACT_MISMATCH);
}

#[tokio::test]
async fn weight_fit_failure_yields_mem_error() {
    let source = FakeSource::new("hf");
    let err = source
        .resolve_artifact(
            &ModelId::new("tiny_llama"),
            &ArtifactSelector {
                format: ArtifactFormat::Gguf,
                quantization: Some(Quantization::Q8_0),
                max_size: Some(ByteSize::from_bytes(64)),
            },
        )
        .await
        .expect_err("120 bytes over a 64-byte budget");
    assert_eq!(err.code(), catalog::MEM_ARTIFACT_OVERSIZE);
    assert_eq!(err.code().category(), Category::Mem);
    assert_eq!(err.severity(), Severity::Error);
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn registry_registers_and_discovers() {
    let registry = registry_with(vec![
        Arc::new(FakeSource::new("hf")),
        Arc::new(FakeSource::new("ollama")),
    ]);
    assert_eq!(registry.len(), 2);

    let hf = registry.get(&AdapterId::new("hf")).expect("registered");
    assert_eq!(hf.id(), AdapterId::new("hf"));
    assert!(hf.capabilities().gguf);

    let discovered = registry.discover();
    assert_eq!(discovered.len(), 2);
    assert!(discovered.iter().all(|d| d.capabilities["gguf"] == true));
}
