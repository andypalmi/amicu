//! Ed25519 signing keys.
//!
//! Security discipline (mirrors `amicu_core::secret`): the private key is loaded
//! transiently as a zeroizing PKCS#8 PEM, consumed to build an opaque signer
//! handle, and the PEM is dropped/zeroized immediately. [`SigningKey`] has no
//! `Serialize`/`Display`, its `Debug` is redacted, and it exposes only the
//! public key and an opaque [`jsonwebtoken::EncodingKey`]. Nothing about the
//! [`SigningKey`] type itself can hand back the private bytes. The one
//! deliberate exception is the free function [`generate_pkcs8_pem`], which
//! exists purely for operator tooling that must produce a fresh PEM to feed
//! a secret manager — see its docs.

use amicu_core::secret::SecretValue;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use pkcs8::LineEnding;
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

/// Generate a fresh Ed25519 key and return its PKCS#8 PEM encoding.
///
/// Unlike every other function in this module, this DELIBERATELY hands back
/// raw private key material — wrapped in [`SecretValue`] so it is never at
/// risk of an accidental `Debug`/log leak between generation and the caller
/// consuming it, but still ultimately a `String` the caller is expected to
/// print. That is the entire point: this exists for operator tooling
/// (`amicu-server gen-signing-key`, andypalmi/amicu-planning#140) that must
/// emit a PEM an operator pipes straight into a secret manager (e.g.
/// `gcloud secrets versions add AMICU_SIGNING_KEY_PEM --data-file=-`), which
/// is fundamentally different from [`SigningKey`]'s job of using a key
/// without ever re-exposing it.
///
/// # Errors
/// Returns [`AuthError::KeyGen`] if key generation or PKCS#8 PEM encoding
/// fails.
pub fn generate_pkcs8_pem() -> Result<SecretValue> {
    let dalek = ed25519_dalek::SigningKey::generate(&mut OsRng);
    let pem = dalek
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|_| AuthError::KeyGen)?;
    Ok(SecretValue::new(pem.to_string()))
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

/// Stable kid: first 22 base64url chars (132 bits — comfortably over the
/// 128-bit floor from andypalmi/amicu-planning#147) of SHA-256 over the
/// public key. Stable across restarts for the same key, so retiring keys
/// keep their published kid.
///
/// 22 chars was previously 16 (96 bits): two independently generated keys
/// colliding on kid was only a ~2^-48 birthday-bound risk, not exploitable
/// (`kid` is a selector, not a security boundary — the signature is), but a
/// collision would silently shadow one key with another in the verifier's
/// kid→key map. Widened before any key was provisioned in production, so
/// there is no compatibility concern with already-issued tokens.
fn derive_kid(public_key: &[u8; 32]) -> String {
    let digest = Sha256::digest(public_key);
    let encoded = URL_SAFE_NO_PAD.encode(digest);
    encoded[..22].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// andypalmi/amicu-planning#147: kid must be at least 128 bits of
    /// entropy. 22 base64url chars encode 132 bits (each char is 6 bits),
    /// clearing the 128-bit floor (a base64url char count of 21.33 would be
    /// the exact floor, so 22 is the smallest whole number of chars that
    /// clears it).
    #[test]
    fn kid_is_at_least_128_bits_of_base64url() {
        let key = SigningKey::generate().unwrap();
        assert!(
            key.kid().len() >= 22,
            "kid {:?} is only {} base64url chars, need >= 22 (128 bits)",
            key.kid(),
            key.kid().len()
        );
    }

    /// The same PEM must yield the same kid on every load — no per-boot
    /// randomness, no coordination needed between replicas (see
    /// `docs/ops/signing-key-rotation.md`).
    #[test]
    fn kid_is_stable_across_loads_of_the_same_pem() {
        let pem = generate_pkcs8_pem().unwrap();

        let first = SigningKey::from_pkcs8_pem(&pem).unwrap();
        let second = SigningKey::from_pkcs8_pem(&pem).unwrap();

        assert_eq!(first.kid(), second.kid());
        assert!(first.kid().len() >= 22);
    }
}
