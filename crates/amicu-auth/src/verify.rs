//! Single-use, expiring email-verification tokens.
//!
//! Mirrors the reset-token store's hygiene: the user holds a high-entropy
//! opaque token (mailed in a "confirm your email" link); the server persists
//! only its SHA-256 hash plus an expiry. Consumption is atomic and single-use
//! (a reused, expired, or unknown token is rejected), so a verification token
//! cannot be replayed to flip the flag twice.
//!
//! Like reset tokens there is no family/rotation: a verify token is minted, used
//! once, and dies. Flipping the account's `email_verified` flag on a successful
//! verify is the service's job, not the store's.

use std::collections::HashMap;
use std::sync::Mutex;

use amicu_core::secret::SecretValue;
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::error::Result;

/// A freshly minted verify token: the opaque secret handed to the user and the
/// server-side record to persist.
pub struct MintedVerify {
    /// The opaque token string (give this to the user via the notifier, once).
    pub token: SecretValue,
    /// The record to store.
    pub record: VerifyRecord,
}

/// Server-side email-verification-token record. Holds only a hash, never the
/// token.
#[derive(Debug, Clone)]
pub struct VerifyRecord {
    /// SHA-256 of the opaque token (lookup key).
    pub token_hash: [u8; 32],
    /// Owning user id.
    pub user_id: String,
    /// Expiry, seconds since the Unix epoch.
    pub expires_at: i64,
    /// Whether the token has already been consumed.
    pub consumed: bool,
}

/// Mint an opaque verify token (32 random bytes, base64url) for `user_id`.
#[must_use]
pub fn mint(user_id: &str, expires_at: i64) -> MintedVerify {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let token_hash = hash_token(&token);
    MintedVerify {
        token: SecretValue::new(token.into_bytes()),
        record: VerifyRecord {
            token_hash,
            user_id: user_id.to_string(),
            expires_at,
            consumed: false,
        },
    }
}

/// SHA-256 of a presented token, for lookup/comparison.
#[must_use]
pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Outcome of atomically consuming (compare-and-mark) a presented verify token.
///
/// Exactly one caller can ever win [`Consumed`](VerifyConsume::Consumed) for a
/// given token: the store performs the liveness check and the mark-consumed as
/// one indivisible step, so a token cannot be replayed to verify an account
/// twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyConsume {
    /// No such token.
    NotFound,
    /// The token was live and THIS caller atomically consumed it; proceed to
    /// mark `user_id`'s email verified.
    Consumed {
        /// Owning user id.
        user_id: String,
    },
    /// The token existed but was already consumed (a replay).
    Reuse,
    /// The token was live but past its expiry.
    Expired,
}

/// Persistence + single-use consumption for email-verification tokens.
#[async_trait]
pub trait VerifyStore: Send + Sync {
    /// Persist a new record.
    async fn insert(&self, record: VerifyRecord) -> Result<()>;
    /// Atomically consume a presented token: in a single indivisible step, check
    /// that it is live (present, not consumed, not expired at `now`) and, if so,
    /// mark it consumed and report [`VerifyConsume::Consumed`].
    ///
    /// This MUST be atomic with respect to concurrent callers, so a token cannot
    /// be redeemed twice. A persistent implementation backs this with a
    /// conditional update (e.g. `UPDATE ... SET consumed=true WHERE
    /// token_hash=$1 AND NOT consumed RETURNING user_id`) and treats "zero rows
    /// affected" on an existing token as reuse.
    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<VerifyConsume>;
}

/// In-memory verify store (tests, single-node dev).
#[derive(Default)]
pub struct InMemoryVerifyStore {
    inner: Mutex<HashMap<[u8; 32], VerifyRecord>>,
}

#[async_trait]
impl VerifyStore for InMemoryVerifyStore {
    async fn insert(&self, record: VerifyRecord) -> Result<()> {
        self.inner
            .lock()
            .expect("verify store poisoned")
            .insert(record.token_hash, record);
        Ok(())
    }

    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<VerifyConsume> {
        // Whole check-and-mark runs under one lock: no await splits it, so two
        // racing consumers of the same token cannot both observe it live.
        let mut map = self.inner.lock().expect("verify store poisoned");
        let Some(record) = map.get(token_hash) else {
            return Ok(VerifyConsume::NotFound);
        };
        if record.consumed {
            return Ok(VerifyConsume::Reuse);
        }
        if record.expires_at <= now {
            return Ok(VerifyConsume::Expired);
        }
        let user_id = record.user_id.clone();
        // Win the race: mark consumed before releasing the lock.
        map.get_mut(token_hash)
            .expect("token present under lock")
            .consumed = true;
        Ok(VerifyConsume::Consumed { user_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mint_then_consume_once_succeeds_and_replay_is_reuse() {
        let store = InMemoryVerifyStore::default();
        let minted = mint("usr_1", 9_999_999_999);
        let hash = minted.record.token_hash;
        store.insert(minted.record).await.unwrap();

        assert_eq!(
            store.consume(&hash, 0).await.unwrap(),
            VerifyConsume::Consumed {
                user_id: "usr_1".to_string()
            }
        );
        // Second consumption of the same token is rejected as reuse.
        assert_eq!(store.consume(&hash, 0).await.unwrap(), VerifyConsume::Reuse);
    }

    #[tokio::test]
    async fn expired_and_unknown_tokens_are_rejected() {
        let store = InMemoryVerifyStore::default();
        let minted = mint("usr_1", 100);
        let hash = minted.record.token_hash;
        store.insert(minted.record).await.unwrap();

        // At/after expiry the live token is Expired, not Consumed.
        assert_eq!(
            store.consume(&hash, 100).await.unwrap(),
            VerifyConsume::Expired
        );
        // A token that was never inserted is NotFound.
        assert_eq!(
            store.consume(&hash_token("never-minted"), 0).await.unwrap(),
            VerifyConsume::NotFound
        );
    }
}
