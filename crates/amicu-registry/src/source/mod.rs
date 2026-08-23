//! The model-source adapter family: discovery and artifact fetch.
//!
//! A [`ModelSourceAdapter`] wraps a model source such as Hugging Face, Ollama,
//! or a URL mirror. It extends the common [`Adapter`] contract with:
//!
//! - [`capabilities`](ModelSourceAdapter::capabilities) - a plain-data
//!   descriptor consumed without side effects.
//! - [`search`](ModelSourceAdapter::search),
//!   [`metadata`](ModelSourceAdapter::metadata) and
//!   [`resolve_artifact`](ModelSourceAdapter::resolve_artifact) - **planning-time**
//!   reads that never touch the destination filesystem. They are `async` only
//!   because a real source performs network I/O.
//! - [`download`](ModelSourceAdapter::download) - the single **apply-time**
//!   method. It writes bytes to `dest` and supports resume/retry: a transfer
//!   that stops partway leaves a partial file, and a retry resumes from that
//!   offset rather than starting over (see [`DownloadResult`]).
//!
//! Missing models or artifacts surface as `AMICU-CAP-*`; a weight-fit failure
//! against resolved metadata surfaces as `AMICU-MEM-*` (see [`SourceError`]).

mod error;

pub use error::SourceError;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use amicu_core::adapter::{Adapter, AdapterError, DescribeCapabilities};
use amicu_core::id::ModelId;
use amicu_core::resource::{ArtifactFormat, GgufVariant, Modality, Quantization, SourceRegistry};
use amicu_core::units::ByteSize;

/// Plain-data descriptor of what a source can do, consumed without side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCapabilities {
    /// Which backing registry this source talks to.
    pub registry: SourceRegistry,
    /// Whether the source supports free-text/faceted search.
    pub supports_search: bool,
    /// Whether the source can serve private/gated models.
    pub supports_private: bool,
    /// Whether the source exposes GGUF quantization variants.
    pub gguf: bool,
}

/// Filters for a planning-time [`search`](ModelSourceAdapter::search).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelQuery {
    /// Free-text query, matched against model id and architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Restrict to models supporting this modality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modality: Option<Modality>,
    /// Restrict to models offering this artifact format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<ArtifactFormat>,
}

/// Short model record returned by [`search`](ModelSourceAdapter::search).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSummary {
    /// Stable model id within the source.
    pub id: ModelId,
    /// Architecture family, e.g. `"llama"`.
    pub architecture: String,
    /// Modalities the model supports.
    pub modalities: Vec<Modality>,
    /// Primary artifact format on offer.
    pub format: ArtifactFormat,
}

/// Full metadata for a model, resolved by [`metadata`](ModelSourceAdapter::metadata).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// Stable model id within the source.
    pub id: ModelId,
    /// Architecture family, e.g. `"llama"`.
    pub architecture: String,
    /// Total parameter count.
    pub total_parameters: u64,
    /// SPDX-style license identifier, e.g. `"apache-2.0"`.
    pub license: String,
    /// Modalities the model supports.
    pub modalities: Vec<Modality>,
    /// Quantizations available across the artifacts.
    pub quantizations: Vec<Quantization>,
    /// Concrete GGUF variants, reusing the canonical [`GgufVariant`].
    pub variants: Vec<GgufVariant>,
}

/// Chooses a concrete artifact from a model's available variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactSelector {
    /// Desired weight format.
    pub format: ArtifactFormat,
    /// Desired quantization; `None` accepts the first available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<Quantization>,
    /// Weight-fit budget: resolving an artifact larger than this raises
    /// `AMICU-MEM-*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<ByteSize>,
}

/// A resolved, fetchable artifact produced by
/// [`resolve_artifact`](ModelSourceAdapter::resolve_artifact).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// The model this artifact belongs to.
    pub model: ModelId,
    /// Opaque locator (URL or registry-specific reference) to fetch from.
    pub uri: String,
    /// The artifact's format.
    pub format: ArtifactFormat,
    /// The artifact's quantization, if applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<Quantization>,
    /// Total artifact size.
    pub size: ByteSize,
    /// Content checksum, e.g. `"sha256:..."`, for integrity verification.
    pub checksum: String,
}

/// Outcome of an apply-time [`download`](ModelSourceAdapter::download).
///
/// `resumed` and `bytes_downloaded` make resume/retry observable: after a
/// partial transfer, a retry reports `resumed == true` and only the bytes
/// fetched on that attempt, while `total_bytes` is the final artifact size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadResult {
    /// Final path the artifact was written to.
    pub path: PathBuf,
    /// Bytes fetched on this call (excludes bytes already on disk).
    pub bytes_downloaded: u64,
    /// Final size of the completed artifact.
    pub total_bytes: u64,
    /// Whether this call resumed from a pre-existing partial file.
    pub resumed: bool,
}

/// A source of models and their weight artifacts.
///
/// Extends [`Adapter`] with discovery and fetch. `search`, `metadata` and
/// `resolve_artifact` run at planning time; `download` runs at apply time and
/// supports resume/retry.
#[async_trait]
pub trait ModelSourceAdapter: Adapter + DescribeCapabilities {
    /// This source's plain-data capability descriptor.
    fn capabilities(&self) -> SourceCapabilities;

    /// Planning-time: find models matching `q`.
    async fn search(&self, q: &ModelQuery) -> Result<Vec<ModelSummary>, AdapterError>;

    /// Planning-time: full metadata for a model - parameters, quantizations,
    /// GGUF variants and license.
    async fn metadata(&self, id: &ModelId) -> Result<ModelMetadata, AdapterError>;

    /// Planning-time: resolve a concrete artifact from a selector. Raises
    /// `AMICU-CAP-*` when nothing matches and `AMICU-MEM-*` on a weight-fit
    /// failure against the selector's budget.
    async fn resolve_artifact(
        &self,
        id: &ModelId,
        sel: &ArtifactSelector,
    ) -> Result<ArtifactRef, AdapterError>;

    /// Apply-time: download an artifact to `dest`, resuming from any partial
    /// file already present and retrying transient failures.
    async fn download(&self, a: &ArtifactRef, dest: &Path) -> Result<DownloadResult, AdapterError>;
}

#[cfg(test)]
mod tests;
