//! axum API service.
//!
//! Currently exposes the auth surface: `.well-known` JWKS/discovery plus the
//! sign-up / token / refresh / logout endpoints. See [`auth::auth_router`].

// The suggested `Duration::from_mins`/`from_days` are unstable
// (duration_constructors) on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

pub mod auth;
pub mod ratelimit;

pub use auth::{AuthApiState, auth_router};
pub use ratelimit::RateLimiter;
