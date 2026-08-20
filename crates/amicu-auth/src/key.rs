//! Ed25519 signing keys.
//!
//! Security discipline (mirrors `amicu_core::secret`): the private key is loaded
//! transiently as a zeroizing PKCS#8 PEM, consumed to build an opaque signer
//! handle, and the PEM is dropped/zeroized immediately. [`SigningKey`] has no
//! `Serialize`/`Display`, its `Debug` is redacted, and it exposes only the
//! public key and an opaque [`jsonwebtoken::EncodingKey`]. Nothing here can hand
//! back the private bytes.

use amicu_core::secret::SecretValue;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rand_core::OsRng;
use sha2::{Digest, Sha256};

use crate::error::{AuthError, Result};
use crate::jwks::Jwk;

/// An Ed25519 signing key: opaque private signer + published public half.
pub struct SigningKey {
    kid: String,
    public_key: [u8; 32],
    // Opaque `ring`-backed signer. Not `Serialize`, not `Debug`.
    encoding: EncodingKey,
}

impl SigningKey {
    /// Generate a fresh Ed25519 key.
    ///
    /// # Errors
    /// Returns [`AuthError::KeyGen`] if PKCS#8 encoding fails.
    pub fn generate() -> Result<Self> {
        let dalek = ed25519_dalek::SigningKey::generate(&mut OsRng);
        Self::from_dalek(&dalek)
    }

    /// Load a key from a PKCS#8 PEM (e.g. from a secret manager at boot).
    ///
    /// The PEM is consumed transiently and never retained.
    ///
    /// # Errors
    /// Returns [`AuthError::KeyGen`] if the PEM is not a valid Ed25519 key.
    pub fn from_pkcs8_pem(pem: &SecretValue) -> Result<Self> {
        let pem_str = pem.expose_secret_str().map_err(|_| AuthError::KeyGen)?;
        let dalek =
            ed25519_dalek::SigningKey::from_pkcs8_pem(pem_str).map_err(|_| AuthError::KeyGen)?;
        Self::from_dalek(&dalek)
    }

    fn from_dalek(dalek: &ed25519_dalek::SigningKey) -> Result<Self> {
        // Transient zeroizing PKCS#8 DER: built, handed to `EncodingKey`, dropped.
        let der = SecretValue::new(
            dalek
                .to_pkcs8_der()
                .map_err(|_| AuthError::KeyGen)?
                .as_bytes()
                .to_vec(),
        );
        let encoding = EncodingKey::from_ed_der(der.expose_secret());
        drop(der);

        let public_key = dalek.verifying_key().to_bytes();
        let kid = derive_kid(&public_key);
        Ok(Self {
            kid,
            public_key,
            encoding,
        })
    }

    /// The stable key id (also the JWT `kid` header).
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The public JWK for this key.
    #[must_use]
    pub fn public_jwk(&self) -> Jwk {
        Jwk::public(&self.kid, URL_SAFE_NO_PAD.encode(self.public_key))
    }

    /// A JWT header carrying this key's `kid` and the EdDSA algorithm.
    #[must_use]
    pub fn header(&self) -> Header {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        header
    }

    /// The opaque signer handle. The private bytes are not reachable through it.
    #[must_use]
    pub fn encoding_key(&self) -> &EncodingKey {
        &self.encoding
    }
}

/// Redacted: exposes only the (public) key id and algorithm.
impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey")
            .field("kid", &self.kid)
            .field("public_key", &"EdDSA(<public>)")
            .field("encoding", &"<redacted>")
            .finish()
    }
}

/// Stable kid: first 16 base64url chars of SHA-256 over the public key. Stable
/// across restarts for the same key, so retiring keys keep their published kid.
fn derive_kid(public_key: &[u8; 32]) -> String {
    let digest = Sha256::digest(public_key);
    let encoded = URL_SAFE_NO_PAD.encode(digest);
    encoded[..16].to_string()
}
