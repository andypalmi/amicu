//! First-party authentication: asymmetrically-signed (Ed25519/EdDSA) access
//! JWTs published via JWKS, plus a reusable offline verifier.
//!
//! The service is the single issuer/signer; every other service is a stateless
//! verifier that fetches the JWKS once and validates tokens locally. The private
//! signing key never leaves the service and is never logged or serialized
//! (see [`key`]); JWKS exposes public keys only.
//!
//! # Features
//! - `verifier` (default): the offline [`verifier::Verifier`] seam.
//! - `issuer` (default): signing keys, rotation, token issuance, Argon2id
//!   password hashing, and the [`service::AuthService`] orchestration.
//! - `reqwest-provider`: a reqwest-backed [`JwksProvider`](verifier::JwksProvider).

// The suggested `Duration::from_mins`/`from_days` are unstable
// (duration_constructors) on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

pub mod authz;
pub mod claims;
pub mod error;
pub mod jwks;
pub mod tenant;

pub use authz::{
    ALL_PERMISSIONS, Permission, authorize_invite_create, authorize_invite_revoke,
    authorize_membership_mutation,
};
pub use claims::{Claims, Tier};
pub use error::{AuthError, Result};
pub use jwks::{Jwk, Jwks, OpenIdConfiguration};
pub use tenant::{Role, WorkspaceId};

#[cfg(feature = "verifier")]
pub mod verifier;
#[cfg(feature = "verifier")]
pub use verifier::{
    JwksProvider, MonoClock, StaticJwksProvider, SystemMonoClock, Verifier, VerifierConfig,
};

#[cfg(feature = "reqwest-provider")]
pub mod http_provider;
#[cfg(feature = "reqwest-provider")]
pub use http_provider::HttpJwksProvider;

#[cfg(feature = "issuer")]
pub mod issuer;
#[cfg(feature = "issuer")]
pub mod key;
#[cfg(feature = "issuer")]
pub mod keyring;
#[cfg(feature = "issuer")]
pub mod membership;
#[cfg(feature = "issuer")]
pub mod notify;
#[cfg(feature = "issuer")]
pub mod password;
#[cfg(feature = "issuer")]
pub mod refresh;
#[cfg(feature = "issuer")]
pub mod reset;
#[cfg(feature = "issuer")]
pub mod service;
#[cfg(feature = "issuer")]
pub mod user;
#[cfg(feature = "issuer")]
pub mod verify;

#[cfg(feature = "issuer")]
pub use issuer::{Clock, IssuerConfig, SystemClock, TokenIssuer};
#[cfg(feature = "issuer")]
pub use key::SigningKey;
#[cfg(feature = "issuer")]
pub use keyring::Keyring;
#[cfg(feature = "issuer")]
pub use membership::{
    MembershipContext, MembershipStore, PersonalMembershipStore, WorkspaceMembership,
};
#[cfg(feature = "issuer")]
pub use notify::{LoggingNotifier, Notification, NotificationKind, Notifier};
#[cfg(feature = "issuer")]
pub use reset::{InMemoryResetStore, MintedReset, ResetConsume, ResetRecord, ResetStore};
#[cfg(feature = "issuer")]
pub use service::{AuthService, SignUpOutcome, TokenPair};
#[cfg(feature = "issuer")]
pub use verify::{InMemoryVerifyStore, MintedVerify, VerifyConsume, VerifyRecord, VerifyStore};
