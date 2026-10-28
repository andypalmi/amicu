//! Google Cloud foundation and backends for amicu: short-lived access tokens
//! from Application Default Credentials with optional service-account
//! impersonation ([`auth`]), a JSON REST client with long-running-operation
//! polling ([`client`], [`operations`]), owner labels ([`labels`]), project /
//! region / zone checks ([`validate`]), the GPU catalog with its drift check
//! ([`catalog`]) and zone placement ([`placement`]), Secret Manager
//! ([`secrets`]), the Compute Engine VM lifecycle for vLLM ([`compute`],
//! supervised for `apply` by [`gce`]) and Cloud Run GPU services
//! ([`cloudrun`]).
//!
//! Nothing in this crate calls Google unless a caller builds an
//! [`AdcTokenSource`] and a [`GcpClient`] over the real transport. Tokens and keys are never `Debug`-printed or put in an error.

pub mod auth;
pub mod catalog;
pub mod client;
pub mod cloudrun;
pub mod compute;
pub mod error;
pub mod gce;
pub mod labels;
pub mod network;
pub mod operations;
pub mod placement;
pub mod secrets;
pub mod validate;

pub use auth::{
    AccessToken, AdcTokenSource, AuthConfig, IMPERSONATE_ENV_VAR, TokenSource, impersonation_target,
};
// `catalog::Provisioning` says how a machine type can be obtained; the VM
// spec's `compute::Provisioning` says how this VM is. Distinct names at the root.
pub use catalog::{GcpCatalog, Provisioning as CatalogProvisioning, resolve_model};
pub use client::{COMPUTE_BASE_URL, GcpClient, RUN_BASE_URL};
pub use compute::Provisioning as VmProvisioning;
pub use error::GcpError;
pub use gce::{GceLaunch, GceRecord, GceSupervisor, GceTiming};
pub use labels::{
    OWNER_LABEL, ensure_owned, has_owner_marker, owner_labels, owner_marker, sanitize_label_value,
};
pub use network::{DEFAULT_NETWORK_PATH, NetworkRef, SubnetworkRef};
pub use operations::{
    PollTiming, wait_compute_global_operation, wait_compute_zone_operation, wait_run_operation,
};
pub use placement::{PlacementError, Shape, candidate_zones, resolve_shape};
pub use secrets::{
    ACCESSOR_ROLE, Replication, SECRETMANAGER_BASE_URL, SecretDelete, SecretKind, secret_id,
};
pub use validate::{validate_project_id, validate_region, validate_zone, zone_region};
