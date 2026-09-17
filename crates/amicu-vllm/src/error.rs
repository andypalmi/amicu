//! The vLLM adapter error enum, classified into the uniform `AdapterError`.
//!
//! Mirrors `amicu_llamacpp::error::LlamaCppError`'s shape (amicu-planning#152):
//! the same diagnostic codes, the same escape-hatch conflict classification,
//! reused rather than re-derived.

use amicu_core::adapter::AdapterErrorKind;
use amicu_core::diagnostic::{DiagnosticCode, Severity, catalog};
use amicu_core::id::{HardwareProfileId, ModelId};

/// Failures the vLLM adapter raises, each mapped to a stable
/// `AMICU-<CATEGORY>-<NUMBER>` code via [`AdapterErrorKind`].
#[derive(Debug, thiserror::Error)]
pub enum VllmError {
    /// The deployment references a model absent from the catalog. Maps to
    /// `AMICU-REF-001`.
    #[error("model '{0}' was not found in the catalog")]
    ModelNotFound(ModelId),

    /// The deployment references a hardware profile absent from the catalog.
    /// Maps to `AMICU-REF-001`.
    #[error("hardware profile '{0}' was not found in the catalog")]
    HardwareNotFound(HardwareProfileId),

    /// A deployment for another runtime family reached the vLLM adapter.
    /// Maps to `AMICU-CAP-001`.
    #[error("vLLM adapter received a non-vllm runtime ({0})")]
    WrongRuntime(&'static str),

    /// The `vllm` binary could not be located. Maps to `AMICU-PROVIDER-001`.
    #[error("vllm binary not found (searched PATH and configured path)")]
    BinaryNotFound,

    /// A probe command failed to execute. Transient; maps to
    /// `AMICU-PROVIDER-001`.
    #[error("probe failed: {0}")]
    Probe(String),

    /// The backend could not start the instance. Transient; maps to
    /// `AMICU-PROVIDER-001`.
    #[error("runtime backend failed: {0}")]
    Supervisor(String),

    /// An `extra_args` flag duplicates a flag already rendered by a typed
    /// field. Maps to `AMICU-CAP-002`.
    #[error("extra_args flag `{0}` duplicates a typed field's flag")]
    ExtraArgDuplicatesTyped(String),

    /// An `extra_args` flag is on the fixed denylist. Maps to `AMICU-CAP-003`.
    #[error("extra_args flag `{0}` is not allowed (amicu owns this setting)")]
    ExtraArgDenylisted(String),

    /// A settings `env` key is one of the fixed dynamic-loader-hook names, or
    /// vLLM's own `VLLM_API_KEY`. Maps to `AMICU-CAP-003`.
    #[error("env var `{0}` is not allowed (amicu owns this setting)")]
    EnvKeyDenylisted(String),
}

impl AdapterErrorKind for VllmError {
    fn code(&self) -> DiagnosticCode {
        match self {
            VllmError::ModelNotFound(_) | VllmError::HardwareNotFound(_) => {
                catalog::REF_UNKNOWN_RESOURCE
            }
            VllmError::WrongRuntime(_) => catalog::CAP_RUNTIME_ARTIFACT_MISMATCH,
            VllmError::BinaryNotFound | VllmError::Probe(_) | VllmError::Supervisor(_) => {
                catalog::PROVIDER_CAPACITY_UNAVAILABLE
            }
            VllmError::ExtraArgDuplicatesTyped(_) => catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED,
            VllmError::ExtraArgDenylisted(_) | VllmError::EnvKeyDenylisted(_) => {
                catalog::CAP_EXTRA_ARG_DENYLISTED
            }
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn retryable(&self) -> bool {
        matches!(self, VllmError::Probe(_) | VllmError::Supervisor(_))
    }
}
