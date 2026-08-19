//! The llama.cpp adapter error enum, classified into the uniform `AdapterError`.

use amicu_core::ByteSize;
use amicu_core::adapter::AdapterErrorKind;
use amicu_core::diagnostic::{DiagnosticCode, Severity, catalog};
use amicu_core::id::{HardwareProfileId, ModelId};
use amicu_core::resource::ArtifactFormat;

use crate::binary::LlamaVersion;

/// Failures the llama.cpp adapter raises, each mapped to a stable
/// `AMICU-<CATEGORY>-<NUMBER>` code via [`AdapterErrorKind`].
#[derive(Debug, thiserror::Error)]
pub enum LlamaCppError {
    /// The deployment references a model absent from the catalog. Maps to
    /// `AMICU-REF-001`.
    #[error("model '{0}' was not found in the catalog")]
    ModelNotFound(ModelId),

    /// The deployment references a hardware profile absent from the catalog.
    /// Maps to `AMICU-REF-001`.
    #[error("hardware profile '{0}' was not found in the catalog")]
    HardwareNotFound(HardwareProfileId),

    /// The model's artifact is not GGUF. Maps to `AMICU-CAP-001`.
    #[error("llama.cpp requires a GGUF artifact, not {0:?}")]
    IncompatibleArtifact(ArtifactFormat),

    /// The model declares no GGUF variant to serve. Maps to `AMICU-CAP-001`.
    #[error("model '{0}' declares no GGUF variant")]
    NoVariant(ModelId),

    /// The model does not fit in available device memory. Maps to
    /// `AMICU-MEM-001`.
    #[error("model needs {required} but only {available} is available")]
    InsufficientMemory {
        required: ByteSize,
        available: ByteSize,
    },

    /// The `llama-server` binary could not be located. Maps to
    /// `AMICU-PROVIDER-001`.
    #[error("llama-server binary not found (searched PATH and configured path)")]
    BinaryNotFound,

    /// The detected `llama-server` is older than the required minimum. Maps to
    /// `AMICU-CAP-001`.
    #[error("llama-server {found} is older than the required minimum {minimum}")]
    VersionTooOld {
        found: LlamaVersion,
        minimum: LlamaVersion,
    },

    /// `llama-server --version` output could not be parsed. Maps to
    /// `AMICU-PROVIDER-001`.
    #[error("could not parse llama-server version from {0:?}")]
    UnparseableVersion(String),

    /// A probe command failed to execute. Transient; maps to
    /// `AMICU-PROVIDER-001`.
    #[error("probe failed: {0}")]
    Probe(String),

    /// Fetching the GGUF artifact failed. Transient; maps to
    /// `AMICU-PROVIDER-002`.
    #[error("download failed: {0}")]
    DownloadFailed(String),

    /// The backend could not start the instance. Transient; maps to
    /// `AMICU-PROVIDER-001`.
    #[error("runtime backend failed: {0}")]
    Supervisor(String),
}

impl AdapterErrorKind for LlamaCppError {
    fn code(&self) -> DiagnosticCode {
        match self {
            LlamaCppError::ModelNotFound(_) | LlamaCppError::HardwareNotFound(_) => {
                catalog::REF_UNKNOWN_RESOURCE
            }
            LlamaCppError::IncompatibleArtifact(_)
            | LlamaCppError::NoVariant(_)
            | LlamaCppError::VersionTooOld { .. } => catalog::CAP_RUNTIME_ARTIFACT_MISMATCH,
            LlamaCppError::InsufficientMemory { .. } => catalog::MEM_VRAM_EXCEEDED,
            LlamaCppError::BinaryNotFound
            | LlamaCppError::UnparseableVersion(_)
            | LlamaCppError::Probe(_)
            | LlamaCppError::Supervisor(_) => catalog::PROVIDER_CAPACITY_UNAVAILABLE,
            LlamaCppError::DownloadFailed(_) => catalog::PROVIDER_DOWNLOAD_FAILED,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn retryable(&self) -> bool {
        matches!(
            self,
            LlamaCppError::Probe(_)
                | LlamaCppError::DownloadFailed(_)
                | LlamaCppError::Supervisor(_)
        )
    }
}
