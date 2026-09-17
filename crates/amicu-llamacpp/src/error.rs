//! The llama.cpp adapter error enum, classified into the uniform `AdapterError`.

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

    /// A deployment for another runtime family reached the llama.cpp adapter.
    /// Maps to `AMICU-CAP-001`.
    #[error("llama.cpp adapter received a non-llama.cpp runtime ({0})")]
    WrongRuntime(&'static str),

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

    /// The resolved artifact's checksum does not match the GGUF variant's
    /// declared `sha256` (amicu-planning#151). Raised before
    /// [`crate::download::download_gguf`] downloads any bytes - fail fast
    /// rather than fetch a file already known to be wrong. Maps to
    /// `AMICU-CAP-001`: like [`LlamaCppError::IncompatibleArtifact`], this is
    /// "the resolved GGUF artifact is not the one this deployment requires",
    /// just discovered by checksum instead of format.
    #[error("artifact checksum mismatch: expected sha256:{expected}, resolved sha256:{actual}")]
    ChecksumMismatch { expected: String, actual: String },

    /// The backend could not start the instance. Transient; maps to
    /// `AMICU-PROVIDER-001`.
    #[error("runtime backend failed: {0}")]
    Supervisor(String),

    /// An `extra_args` flag duplicates a flag already rendered by a typed
    /// field (amicu-planning#150). Maps to `AMICU-CAP-002`.
    #[error("extra_args flag `{0}` duplicates a typed field's flag")]
    ExtraArgDuplicatesTyped(String),

    /// An `extra_args` flag is on the fixed denylist (amicu-planning#150).
    /// Maps to `AMICU-CAP-003`.
    #[error("extra_args flag `{0}` is not allowed (amicu owns this setting)")]
    ExtraArgDenylisted(String),

    /// A settings `env` key is one of the fixed dynamic-loader-hook names
    /// (`LD_PRELOAD` and friends; amicu review hardening, amicu-planning#150).
    /// Maps to `AMICU-CAP-003`.
    #[error("env var `{0}` is not allowed (can load arbitrary code into the process)")]
    EnvKeyDenylisted(String),

    /// A settings `env` key is a `LLAMA_ARG_<X>` mirror of a flag already
    /// rendered by a typed field (amicu-planning#150 review hardening:
    /// llama-server reads `LLAMA_ARG_*` env vars as CLI-flag equivalents, so
    /// this is the same conflict as [`LlamaCppError::ExtraArgDuplicatesTyped`]
    /// reached through `env` instead of `extra_args`). Maps to
    /// `AMICU-CAP-002`.
    #[error(
        "env var `{key}` mirrors typed field flag `{flag}` (llama-server reads LLAMA_ARG_* as CLI flags)"
    )]
    EnvKeyMirrorsTypedFlag { key: String, flag: String },

    /// A settings `env` key is a `LLAMA_ARG_<X>` mirror of a denylisted flag
    /// (amicu-planning#150 review hardening; see
    /// [`LlamaCppError::EnvKeyMirrorsTypedFlag`]). Maps to `AMICU-CAP-003`.
    #[error(
        "env var `{key}` mirrors denylisted flag `{flag}` (llama-server reads LLAMA_ARG_* as CLI flags)"
    )]
    EnvKeyMirrorsDenylistedFlag { key: String, flag: String },

    /// Both `chat_template` and `chat_template_file` are set on the same
    /// settings block (amicu-planning#151): they render mutually exclusive
    /// flags, so having both is ambiguous. Maps to `AMICU-CAP-005`.
    #[error("chat_template and chat_template_file are mutually exclusive; set only one")]
    ChatTemplateConflict,
}

impl AdapterErrorKind for LlamaCppError {
    fn code(&self) -> DiagnosticCode {
        match self {
            LlamaCppError::ModelNotFound(_) | LlamaCppError::HardwareNotFound(_) => {
                catalog::REF_UNKNOWN_RESOURCE
            }
            LlamaCppError::IncompatibleArtifact(_)
            | LlamaCppError::NoVariant(_)
            | LlamaCppError::WrongRuntime(_)
            | LlamaCppError::VersionTooOld { .. }
            | LlamaCppError::ChecksumMismatch { .. } => catalog::CAP_RUNTIME_ARTIFACT_MISMATCH,
            LlamaCppError::BinaryNotFound
            | LlamaCppError::UnparseableVersion(_)
            | LlamaCppError::Probe(_)
            | LlamaCppError::Supervisor(_) => catalog::PROVIDER_CAPACITY_UNAVAILABLE,
            LlamaCppError::DownloadFailed(_) => catalog::PROVIDER_DOWNLOAD_FAILED,
            LlamaCppError::ExtraArgDuplicatesTyped(_)
            | LlamaCppError::EnvKeyMirrorsTypedFlag { .. } => {
                catalog::CAP_EXTRA_ARG_DUPLICATES_TYPED
            }
            LlamaCppError::ExtraArgDenylisted(_)
            | LlamaCppError::EnvKeyDenylisted(_)
            | LlamaCppError::EnvKeyMirrorsDenylistedFlag { .. } => {
                catalog::CAP_EXTRA_ARG_DENYLISTED
            }
            LlamaCppError::ChatTemplateConflict => catalog::CAP_CHAT_TEMPLATE_CONFLICT,
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
