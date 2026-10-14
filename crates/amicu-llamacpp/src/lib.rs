//! The llama.cpp runtime adapter.
//!
//! Implements [`RuntimeAdapter`](amicu_runtime::RuntimeAdapter) for the
//! `llama-server` local path (PRD "llama.cpp Local Path" steps 2–4):
//!
//! - **Binary detection** ([`binary`]) - locate `llama-server`, probe
//!   `--version`, and gate on a minimum. Command execution is injectable behind
//!   [`CommandRunner`](binary::CommandRunner) for hermetic tests.
//! - **GGUF download** ([`download`]) - resumable/retryable fetch layered on the
//!   registry's [`ModelSourceAdapter`](amicu_registry::ModelSourceAdapter).
//! - **Command generation** ([`command`]) - render the visible, secret-free
//!   `llama-server` command line.
//!
//! Planning (`capabilities`/`validate`/`plan_launch`) is pure and resolves the
//! deployment's model/hardware through a [`Catalog`]. Apply/runtime-time methods
//! delegate to a [`ProcessSupervisor`](supervisor::ProcessSupervisor), backed by
//! launchd (macOS) or systemd (Linux) with endpoint health checks
//! ([`service`], [`health`]).

mod adapter;
pub mod binary;
pub mod command;
pub mod download;
mod error;
pub mod health;
pub mod service;
pub mod supervisor;

pub use adapter::{Catalog, LlamaCppAdapter, MapCatalog, ResolvedModel};
pub use binary::{
    BinaryConfig, BinaryInfo, CommandRunner, HostRunner, LlamaVersion, detect_binary,
};
pub use command::render_command;
pub use download::{DEFAULT_MAX_ATTEMPTS, download_gguf};
pub use error::LlamaCppError;
pub use health::{Health, HealthProbe, HttpHealthProbe};
pub use service::{
    CommandExec, ExecOutput, HostBackend, HostContext, HostExec, LaunchdManager, ServiceManager,
    ServicePaths, ServiceSpec, ServiceState, SystemdManager, allow_service_commands,
};
pub use supervisor::{
    LaunchdSupervisor, ProcessSupervisor, ReadyWait, ResolvedSecret, ServiceSupervisor,
    SystemdSupervisor, host_label, host_supervisor, project_host_label, resolve_launch_secrets,
};

#[cfg(test)]
mod tests;
