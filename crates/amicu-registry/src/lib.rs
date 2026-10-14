//! Model/hardware metadata and source sync.
//!
//! This crate defines the [`ModelSourceAdapter`] family: the contract a model
//! source (Hugging Face, Ollama, a URL mirror, ...) implements so Amicu can
//! discover models and fetch their weight artifacts. It builds on the common
//! adapter contract in [`amicu_core::adapter`].

pub mod fit;
pub mod gguf;
pub mod source;

pub use fit::{
    FitConfig, FitReport, KvCacheParams, MemoryPool, OverheadRange, bytes_per_parameter, check_fit,
    check_variant_fit, check_variant_fit_with_draft, check_variant_fit_with_kv_cache,
    check_vllm_fit, estimated_peak, kv_cache_bytes, variant_weights, weights_memory,
};
pub use gguf::{GgufError, GgufResolver};
pub use source::{
    ArtifactRef, ArtifactSelector, DownloadResult, ModelMetadata, ModelQuery, ModelSourceAdapter,
    ModelSummary, SourceCapabilities, SourceError,
};
