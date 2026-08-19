//! Model/hardware metadata and source sync.
//!
//! This crate defines the [`ModelSourceAdapter`] family: the contract a model
//! source (Hugging Face, Ollama, a URL mirror, ...) implements so Amicu can
//! discover models and fetch their weight artifacts. It builds on the common
//! adapter contract in [`amicu_core::adapter`].

pub mod source;

pub use source::{
    ArtifactRef, ArtifactSelector, DownloadResult, ModelMetadata, ModelQuery, ModelSourceAdapter,
    ModelSummary, SourceCapabilities, SourceError,
};
