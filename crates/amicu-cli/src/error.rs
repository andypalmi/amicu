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
