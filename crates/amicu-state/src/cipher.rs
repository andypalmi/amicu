//! The state-content encryption-at-rest seam.
//!
//! # Scope: metadata and secret *references*, never secret material
//!
//! A [`StateSnapshot`](crate::StateSnapshot) wraps a [`Graph`](amicu_core::Graph),
//! and nothing inside a `Graph` can ever hold a raw secret:
//! [`SecretValue`](amicu_core::secret::SecretValue) deliberately does not
//! implement `Serialize` (see `amicu_core::secret` and
//! `amicu_state::snapshot`'s "secret-refs-by-type" doc section, enforced at
//! compile time by `amicu-state/tests/compile_fail/`), so only
//! [`SecretRef`](amicu_core::secret::SecretRef) pointers - safe, non-sensitive
//! metadata - can ever reach persisted state. A [`StateCipher`] therefore
//! encrypts control-plane metadata and secret *references*, not secrets: it
//! adds defense in depth, it does not stand between an attacker and plaintext
//! credentials, because no plaintext credential is ever there to find.
//!
//! # Self-host vs. SaaS
//!
//! [`IdentityCipher`] is the default and the only implementation this crate
//! ships: a byte-for-byte no-op that returns its input unchanged. Every
//! self-host deployment - and the whole existing `amicu-store` test suite -
//! runs with no [`StateCipher`] configured at all, so `PostgresStateBackend`
//! never touches this seam and persists exactly the bytes it always has.
//!
//! **Self-host operators are responsible for their own database's encryption
//! at rest** (disk/volume encryption, filesystem-level encryption, or
//! whatever their Postgres deployment or managed-database provider offers).
//! This crate does not provide it and this seam does not substitute for it.
//!
//! This seam exists so a SaaS-hosted deployment can plug in envelope/KMS
//! encryption (e.g. per-workspace data keys unwrapped from a managed KMS) at
//! the point snapshot bytes cross the persistence boundary, without
//! forking `PostgresStateBackend` or touching self-host's path. Implementing
//! that SaaS cipher is out of scope here; this crate defines only the trait
//! and its no-op default.
//!
//! [`Graph`]: amicu_core::Graph

use async_trait::async_trait;

use crate::error::StateError;

/// Encrypts and decrypts the opaque bytes a [`StateBackendAdapter`] persists
/// for a snapshot's content, at the boundary where they cross into and out of
/// storage.
///
/// See the [module docs](self) for what this does and does not protect.
///
/// [`StateBackendAdapter`]: crate::StateBackendAdapter
#[async_trait]
pub trait StateCipher: Send + Sync {
    /// Encrypt `plaintext`, returning opaque ciphertext bytes that
    /// [`decrypt`](Self::decrypt) can recover the exact original from.
    ///
    /// # Errors
    /// Returns [`StateError::Backend`] if encryption fails (e.g. the
    /// configured key or KMS is unavailable).
    async fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, StateError>;

    /// Decrypt bytes produced by [`encrypt`](Self::encrypt), recovering the
    /// original plaintext exactly.
    ///
    /// # Errors
    /// Returns [`StateError::Corrupted`] if the ciphertext cannot be
    /// authenticated or decrypted (tampering, or the wrong key), or
    /// [`StateError::Backend`] if the configured key or KMS is unavailable.
    async fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, StateError>;
}

/// The default, no-op [`StateCipher`]: returns its input unchanged.
///
/// This is the implicit backend behavior whenever no cipher is configured -
/// see the [module docs](self) for why that is the correct default for
/// self-host and for the existing test suite.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityCipher;

#[async_trait]
impl StateCipher for IdentityCipher {
    async fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, StateError> {
        Ok(plaintext.to_vec())
    }

    async fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, StateError> {
        Ok(ciphertext.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn identity_cipher_encrypt_is_byte_identical_to_input() {
        let cipher = IdentityCipher;
        let plaintext = b"{\"schema_version\":\"v1\"}".to_vec();
        let ciphertext = cipher.encrypt(&plaintext).await.unwrap();
        assert_eq!(
            ciphertext, plaintext,
            "identity cipher must not alter bytes"
        );
    }

    #[tokio::test]
    async fn identity_cipher_round_trips_arbitrary_bytes() {
        let cipher = IdentityCipher;
        let plaintext = vec![0u8, 1, 2, 255, 254, 3, 7, 8];
        let ciphertext = cipher.encrypt(&plaintext).await.unwrap();
        let recovered = cipher.decrypt(&ciphertext).await.unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[tokio::test]
    async fn identity_cipher_round_trips_empty_input() {
        let cipher = IdentityCipher;
        let ciphertext = cipher.encrypt(&[]).await.unwrap();
        assert_eq!(ciphertext, Vec::<u8>::new());
        let recovered = cipher.decrypt(&ciphertext).await.unwrap();
        assert_eq!(recovered, Vec::<u8>::new());
    }
}
