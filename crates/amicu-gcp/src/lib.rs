//! Google Cloud foundation shared by amicu's GCP backends (Compute Engine VMs,
//! Cloud Run GPU): short-lived access tokens from Application Default
//! Credentials with optional service-account impersonation ([`auth`]), a JSON
//! REST client with long-running-operation polling ([`client`], [`operations`]),
//! owner labels ([`labels`]), project / region / zone checks ([`validate`]) and
//! the seam for the GPU catalog ([`catalog`]).
//!
//! No backend lives here yet, and nothing in this crate calls Google unless a
//! caller builds an [`AdcTokenSource`] and a [`GcpClient`] over the real
//! transport. Tokens and keys are never `Debug`-printed or put in an error.

pub mod auth;
pub mod catalog;
pub mod error;
pub mod labels;
pub mod validate;

pub use auth::{
    AccessToken, AdcTokenSource, AuthConfig, IMPERSONATE_ENV_VAR, TokenSource, impersonation_target,
};
pub use catalog::GcpCatalog;
pub use error::GcpError;
pub use labels::{
    OWNER_LABEL, ensure_owned, has_owner_marker, owner_labels, owner_marker, sanitize_label_value,
};
pub use validate::{validate_project_id, validate_region, validate_zone, zone_region};
