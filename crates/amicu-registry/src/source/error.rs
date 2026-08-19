//! Error classification for the model-source adapter family.

use amicu_core::adapter::AdapterErrorKind;
use amicu_core::diagnostic::{DiagnosticCode, Severity, catalog};
use amicu_core::id::ModelId;
use amicu_core::units::ByteSize;

/// Adapter-specific errors raised by a [`ModelSourceAdapter`](super::ModelSourceAdapter).
///
/// Folded into the uniform [`AdapterError`](amicu_core::adapter::AdapterError)
/// at the dyn boundary via [`AdapterErrorKind`].
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// No such model in the source catalog.
    #[error("model '{0}' was not found in the source catalog")]
    ModelNotFound(ModelId),

    /// No artifact matches the requested format/quantization selector.
    #[error("no artifact matches the selector for model '{model}'")]
    ArtifactUnavailable {
        /// The model whose artifact could not be resolved.
        model: ModelId,
    },

    /// The resolved artifact is larger than the caller's weight-fit budget.
    #[error("artifact for '{model}' needs {required} but only {budget} is available")]
    WeightFit {
        /// The model whose weights do not fit.
        model: ModelId,
        /// Size of the resolved artifact.
        required: ByteSize,
        /// Budget the caller allowed.
        budget: ByteSize,
    },

    /// A transient transfer failure; the caller should retry and resume.
    #[error("download failed: {0}")]
    Download(String),
}

impl AdapterErrorKind for SourceError {
    fn code(&self) -> DiagnosticCode {
        match self {
            // A missing model or artifact is a capability mismatch.
            SourceError::ModelNotFound(_) | SourceError::ArtifactUnavailable { .. } => {
                catalog::CAP_RUNTIME_ARTIFACT_MISMATCH
            }
            // An oversized artifact is a memory/capacity fault.
            SourceError::WeightFit { .. } => catalog::MEM_ARTIFACT_OVERSIZE,
            SourceError::Download(_) => catalog::PROVIDER_DOWNLOAD_FAILED,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn retryable(&self) -> bool {
        // Only a transfer failure is worth retrying; missing/oversized artifacts
        // will not change on retry.
        matches!(self, SourceError::Download(_))
    }
}
