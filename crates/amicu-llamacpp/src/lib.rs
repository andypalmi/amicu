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
//! launchd (macOS), systemd (Linux) or Task Scheduler (Windows) with endpoint
//! health checks ([`service`], [`service_windows`], [`health`]).

mod adapter;
pub mod binary;
pub mod command;
pub mod download;
mod error;
pub mod health;
pub mod remote;
pub mod service;
pub mod service_windows;
pub mod supervisor;

pub use adapter::{Catalog, LlamaCppAdapter, MapCatalog, ResolvedModel};
pub use binary::{
    BinaryConfig, BinaryInfo, CommandRunner, HostRunner, LlamaVersion, detect_binary,
};
pub use command::{render_command, render_command_with_alias};
pub use download::{DEFAULT_MAX_ATTEMPTS, download_gguf};
pub use error::LlamaCppError;
pub use health::{Health, HealthProbe, HttpHealthProbe};
pub use service::{
    CommandExec, ExecOutput, HostBackend, HostContext, HostExec, LaunchdManager, Presence,
    ServiceManager, ServicePaths, ServiceSpec, ServiceState, SystemdManager,
    allow_service_commands,
};
pub use service_windows::TaskSchedulerManager;
pub use supervisor::{
    LaunchdSupervisor, ProcessSupervisor, ReadyWait, ResolvedSecret, ServiceSupervisor,
    SidecarPlanner, SystemdSupervisor, TaskSchedulerSupervisor, host_label, host_supervisor,
    host_supervisor_with_sidecar, project_host_label, resolve_launch_secrets, sidecar_label,
    sidecar_main_label,
};

#[cfg(test)]
mod tests;
