//! Runtime adapter contract for Amicu serving runtimes (llama.cpp, vLLM).
//!
//! This crate defines [`RuntimeAdapter`], the family trait extending the common
//! [`Adapter`](amicu_core::adapter::Adapter) contract with runtime-specific
//! planning and apply methods, plus the plain-data domain types it exchanges:
//! [`RuntimeCapabilities`], [`LaunchPlan`], [`RuntimeHandle`],
//! [`RuntimeStatus`], [`RuntimeMetrics`], and the log types. Runtime failures
//! are classified through [`RuntimeError`] into the uniform
//! [`AdapterError`](amicu_core::adapter::AdapterError).
//!
//! Planning-time methods (`capabilities`, `validate`, `plan_launch`) are pure
//! synchronous `fn`s: they need no async runtime and produce no side effects.
//! Secret values never appear in a [`LaunchPlan`]; they are resolved only at
//! apply time via a [`SecretResolver`](amicu_core::secret::SecretResolver).

mod adapter;
mod capabilities;
mod error;
mod handle;
mod plan;

pub use adapter::RuntimeAdapter;
pub use capabilities::{Parallelism, Platform, RuntimeCapabilities, RuntimeKind};
pub use error::RuntimeError;
pub use handle::{LogOptions, LogStream, RuntimeHandle, RuntimeMetrics, RuntimeStatus};
pub use plan::{LaunchPlan, RenderedLaunch, ResourceEstimate};

#[cfg(test)]
mod tests;
