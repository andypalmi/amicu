//! A minimal tonic/rustls client for Modal's (modal.com) control-plane
//! gRPC API.
//!
//! This crate is gRPC plumbing only: it vendors Modal's proto (see
//! `proto/VENDORED.md`), generates a tonic client with `build.rs`,
//! authenticates with token headers, and exposes [`ModalClient::hello`]
//! (the `ClientHello` RPC) as the minimal end-to-end auth check. It
//! deliberately has **no provisioning logic** — no `AppGetOrCreate`,
//! `ImageGetOrCreate`, `FunctionCreate`, sandbox, or volume calls. Those
//! land in later PRs against `andypalmi/amicu-planning#158`, on top of the
//! connection and interceptor this crate provides.
//!
//! Only `amicu-cli` depends on this crate (via the `amicu modal
//! auth-check` command) — `amicu-api`/`amicu-server`, and therefore the
//! Docker runtime image, do not.

mod client;
mod credentials;
mod error;
mod interceptor;
mod pb;

pub use client::{DEFAULT_ENDPOINT, ENDPOINT_ENV_VAR, Hello, ModalClient};
pub use credentials::{Credentials, MissingCredential};
pub use error::ModalError;
