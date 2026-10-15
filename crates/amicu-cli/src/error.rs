//! The CLI error type.

use std::path::PathBuf;

use amicu_core::adapter::AdapterError;
use amicu_hcl::HclError;

/// Failures surfaced by CLI commands.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// No `*.hcl` config was found in the project directory.
    #[error("no Amicu config found in {}; run `amicu init` first", .0.display())]
    NoConfig(PathBuf),

    /// A filesystem operation failed.
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },

    /// An HCL parse or format error, tagged with the offending file.
    #[error("{}: {source}", .file.display())]
    Hcl { file: PathBuf, source: HclError },

    /// A runtime adapter failed while planning or applying.
    #[error("{0}")]
    Adapter(#[from] AdapterError),

    /// Serialization of a report or plan failed.
    #[error("serialization failed: {0}")]
    Serde(#[from] serde_json::Error),

    /// `amicu modal` failed to authenticate or reach Modal's control plane.
    #[error("{0}")]
    Modal(#[from] amicu_modal::ModalError),

    /// Another amicu process holds the state lock.
    #[error("{0}")]
    StateLocked(String),

    /// A generic, already-formatted message.
    #[error("{0}")]
    Message(String),
}

impl CliError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

impl CliError {
    /// Whether the failure is a write to a closed pipe (`amicu ... | head -1`).
    /// That is the reader's choice, not an error worth reporting.
    pub(crate) fn is_broken_pipe(&self) -> bool {
        match self {
            Self::Io { source, .. } => source.kind() == std::io::ErrorKind::BrokenPipe,
            Self::Serde(e) => e.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe),
            _ => false,
        }
    }
}
