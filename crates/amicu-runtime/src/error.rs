//! The runtime family error enum, classified into the uniform `AdapterError`.

use amicu_core::ByteSize;
use amicu_core::adapter::AdapterErrorKind;
use amicu_core::diagnostic::{DiagnosticCode, Severity, catalog};
use amicu_core::resource::ArtifactFormat;

use crate::capabilities::RuntimeKind;

/// Failures a runtime adapter can raise, each mapped to a stable
/// `AMICU-<CATEGORY>-<NUMBER>` code via [`AdapterErrorKind`].
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The deployment's artifact/runtime pairing is unsupported. Maps to
    /// `AMICU-CAP-001`.
    #[error("runtime '{runtime}' does not support artifact format {format:?}")]
    IncompatibleArtifact {
        runtime: RuntimeKind,
        format: ArtifactFormat,
    },

    /// The model does not fit in the available device memory. Maps to
    /// `AMICU-MEM-001`.
    #[error("model needs {required} but only {available} is available")]
    InsufficientMemory {
        required: ByteSize,
        available: ByteSize,
    },

    /// The backend could not start the instance. Transient; maps to
    /// `AMICU-PROVIDER-001`.
    #[error("runtime backend failed to start: {0}")]
    LaunchFailed(String),
}

impl AdapterErrorKind for RuntimeError {
    fn code(&self) -> DiagnosticCode {
        match self {
            RuntimeError::IncompatibleArtifact { .. } => catalog::CAP_RUNTIME_ARTIFACT_MISMATCH,
            RuntimeError::InsufficientMemory { .. } => catalog::MEM_VRAM_EXCEEDED,
            RuntimeError::LaunchFailed(_) => catalog::PROVIDER_CAPACITY_UNAVAILABLE,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn retryable(&self) -> bool {
        // A failed start may succeed on retry; capability/memory misfits will not.
        matches!(self, RuntimeError::LaunchFailed(_))
    }
}
