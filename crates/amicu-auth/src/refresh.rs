//! Opaque, revocable refresh tokens.
//!
//! The client holds a high-entropy opaque token; the server stores only its
//! SHA-256 hash plus metadata. Tokens rotate on use (the presented one is
//! revoked and a new one issued) and can be revoked on logout.

use std::collections::HashMap;
use std::sync::Mutex;

use amicu_core::secret::SecretValue;
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::error::Result;

/// A freshly minted refresh token: the opaque secret handed to the client and
/// the server-side record to persist.
pub struct MintedRefresh {
    /// The opaque token string (give this to the client, once).
    pub token: SecretValue,
    /// The record to store.
    pub record: RefreshRecord,
}

/// Server-side refresh-token record. Holds only a hash, never the token.
#[derive(Debug, Clone)]
pub struct RefreshRecord {
    /// SHA-256 of the opaque token (lookup key).
    pub token_hash: [u8; 32],
    /// Owning user id.
    pub user_id: String,
    /// Expiry, seconds since the Unix epoch.
    pub expires_at: i64,
    /// Whether the token has been revoked.
    pub revoked: bool,
}

/// Mint a new opaque refresh token (32 random bytes, base64url) for a user.
#[must_use]
pub fn mint(user_id: &str, expires_at: i64) -> MintedRefresh {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let token_hash = hash_token(&token);
    MintedRefresh {
        token: SecretValue::new(token.into_bytes()),
        record: RefreshRecord {
            token_hash,
            user_id: user_id.to_string(),
            expires_at,
            revoked: false,
        },
    }
}

/// SHA-256 of a presented token, for lookup/comparison.
#[must_use]
pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Persistence + revocation for refresh tokens.
#[async_trait]
pub trait RefreshStore: Send + Sync {
    /// Persist a new record.
    async fn insert(&self, record: RefreshRecord) -> Result<()>;
    /// Look up a record by token hash.
    async fn get(&self, token_hash: &[u8; 32]) -> Result<Option<RefreshRecord>>;
    /// Mark a record revoked (idempotent).
    async fn revoke(&self, token_hash: &[u8; 32]) -> Result<()>;
}

/// In-memory refresh store (tests, single-node dev).
#[derive(Default)]
pub struct InMemoryRefreshStore {
    inner: Mutex<HashMap<[u8; 32], RefreshRecord>>,
}

#[async_trait]
impl RefreshStore for InMemoryRefreshStore {
    async fn insert(&self, record: RefreshRecord) -> Result<()> {
        self.inner
            .lock()
            .expect("refresh store poisoned")
            .insert(record.token_hash, record);
        Ok(())
    }

    async fn get(&self, token_hash: &[u8; 32]) -> Result<Option<RefreshRecord>> {
        Ok(self
            .inner
            .lock()
            .expect("refresh store poisoned")
            .get(token_hash)
            .cloned())
    }

    async fn revoke(&self, token_hash: &[u8; 32]) -> Result<()> {
        if let Some(rec) = self
            .inner
            .lock()
            .expect("refresh store poisoned")
            .get_mut(token_hash)
        {
            rec.revoked = true;
        }
        Ok(())
    }
}
