//! JWKS and OpenID discovery documents.
//!
//! Only public verification keys are represented here. There is deliberately no
//! field that could carry private key material, and none of these types can be
//! constructed from a private key.

use serde::{Deserialize, Serialize};

/// Signing algorithm advertised in the JWKS and discovery document.
pub const ALG: &str = "EdDSA";
/// Key type for Ed25519 (RFC 8037 OKP).
pub const KTY: &str = "OKP";
/// Curve for Ed25519.
pub const CRV: &str = "Ed25519";

/// A single public JWK (Ed25519 / OKP, public half only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwk {
    /// Key type — always `OKP`.
    pub kty: String,
    /// Curve — always `Ed25519`.
    pub crv: String,
    /// Intended use — `sig`.
    #[serde(rename = "use")]
    pub use_: String,
    /// Algorithm — `EdDSA`.
    pub alg: String,
    /// Stable key id, matching the JWT `kid` header.
    pub kid: String,
    /// base64url (no pad) of the 32-byte public key.
    pub x: String,
}

impl Jwk {
    /// Build a public JWK from a key id and raw public key bytes.
    #[must_use]
    pub fn public(kid: impl Into<String>, x_b64url: impl Into<String>) -> Self {
        Self {
            kty: KTY.to_string(),
            crv: CRV.to_string(),
            use_: "sig".to_string(),
            alg: ALG.to_string(),
            kid: kid.into(),
            x: x_b64url.into(),
        }
    }
}

/// A JSON Web Key Set — the public keys served at `/.well-known/jwks.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Jwks {
    /// Active + retiring public keys.
    pub keys: Vec<Jwk>,
}

/// Minimal OpenID Connect discovery document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenIdConfiguration {
    /// Token issuer.
    pub issuer: String,
    /// Absolute URL of the JWKS endpoint.
    pub jwks_uri: String,
    /// Token endpoint.
    pub token_endpoint: String,
    /// Supported signing algorithms.
    pub id_token_signing_alg_values_supported: Vec<String>,
    /// Supported response types.
    pub response_types_supported: Vec<String>,
    /// Supported subject types.
    pub subject_types_supported: Vec<String>,
}

impl OpenIdConfiguration {
    /// Standard discovery document for this issuer.
    #[must_use]
    pub fn new(issuer: impl Into<String>, base_url: &str) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            issuer: issuer.into(),
            jwks_uri: format!("{base}/.well-known/jwks.json"),
            token_endpoint: format!("{base}/token"),
            id_token_signing_alg_values_supported: vec![ALG.to_string()],
            response_types_supported: vec!["token".to_string()],
            subject_types_supported: vec!["public".to_string()],
        }
    }
}
