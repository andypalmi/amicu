//! The vLLM runtime adapter.
//!
//! Implements [`RuntimeAdapter`](amicu_runtime::RuntimeAdapter) for local,
//! single-process `vllm serve` launches (amicu-planning#152 - the renderer
//! half; the consumer/on-prem hardware catalog half landed in PR #145).
//! Kubernetes/Helm delivery for vLLM stays out of scope (tracked separately:
//! #27-#30).
//!
//! - **Command generation** ([`command`]) - render the visible, secret-free
//!   `vllm serve` command line from a deployment's typed `vllm {}` settings,
//!   plus the same `extra_args`/`env` escape hatch llama.cpp's renderer
//!   offers (amicu-planning#150's conventions, applied here for the first
//!   time to vLLM).
//! - **Adapter** ([`adapter`]) - planning (`capabilities`/`validate`/
//!   `plan_launch`) is pure and resolves the deployment's model/hardware
//!   through the same [`Catalog`] contract llama.cpp uses. Apply/runtime-time
//!   methods delegate to the *same* [`ProcessSupervisor`] implementation
//!   llama.cpp does (`amicu_llamacpp::host_supervisor`) - both are local HTTP
//!   servers launched and health-checked identically, so nothing about
//!   process supervision is vLLM-specific.

mod adapter;
pub mod command;
mod error;

pub use adapter::VllmAdapter;
pub use command::render_command;
pub use error::VllmError;
// Re-exported so a caller building a vLLM adapter needs only this crate for
// the catalog/supervisor plumbing too, without also depending directly on
// amicu-llamacpp for what is, at this layer, runtime-agnostic wiring.
pub use amicu_llamacpp::{Catalog, MapCatalog, ProcessSupervisor, ResolvedModel, host_supervisor};

#[cfg(test)]
mod tests;
