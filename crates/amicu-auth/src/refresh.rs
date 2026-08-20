//! Opaque, revocable refresh tokens with reuse/theft detection.
//!
//! The client holds a high-entropy opaque token; the server stores only its
//! SHA-256 hash plus metadata. Tokens rotate on use (the presented one is
//! revoked and a new one issued) and can be revoked on logout.
//!
//! # Token families (reuse detection)
//! Every token carries a `family_id`. A fresh sign-in opens a new family;
//! each rotation mints its successor into the *same* family. Because a rotation
//! revokes the token it replaces, a valid family has exactly one live token at
//! a time (its current leaf). Re-presenting an already-revoked token is
//! therefore anomalous: either a client replayed an old token, or a stolen
//! token is being replayed after the legitimate client already rotated past it.
//! Either way it is treated as a compromise signal — the whole family is
//! revoked and flagged (see [`RefreshStore::compromise_family`]) so the
//! attacker's rotated token and the victim's live token both die, forcing a
//! fresh authentication. This mirrors the OAuth 2.0 refresh-token rotation
//! guidance (RFC 6749 §10.4 / OAuth 2.0 Security BCP §4.13).
//!
//! By design this means replaying *any* leaked old token also terminates the
//! legitimate user's current session (they must re-authenticate). That is the
//! intended, BCP-recommended trade-off: a false negative (letting a possibly
//! stolen token live) is worse than the friction of a re-login.
//!
//! Rotation is consumed atomically via [`RefreshStore::consume`] (compare-and-
//! revoke in one indivisible step), so concurrent refreshes of the same token
//! cannot both succeed and leave two live successors in a family.

use std::collections::{HashMap, HashSet};
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
    /// Rotation-family id. Shared by a sign-in token and every token rotated
    /// from it; the unit of reuse-detection revocation.
    pub family_id: String,
    /// Expiry, seconds since the Unix epoch.
    pub expires_at: i64,
    /// Whether the token has been revoked.
    pub revoked: bool,
}

/// Mint a fresh opaque refresh token into a brand-new family (sign-in).
#[must_use]
pub fn mint(user_id: &str, expires_at: i64) -> MintedRefresh {
    mint_in_family(user_id, &new_family_id(), expires_at)
}

/// Mint an opaque refresh token (32 random bytes, base64url) into a specific
/// family. Rotation reuses the presented token's family so the whole chain
/// shares a single reuse-detection unit.
#[must_use]
pub fn mint_in_family(user_id: &str, family_id: &str, expires_at: i64) -> MintedRefresh {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let token_hash = hash_token(&token);
    MintedRefresh {
        token: SecretValue::new(token.into_bytes()),
        record: RefreshRecord {
            token_hash,
            user_id: user_id.to_string(),
            family_id: family_id.to_string(),
            expires_at,
            revoked: false,
        },
    }
}

/// A new high-entropy family id (16 random bytes, base64url).
#[must_use]
pub fn new_family_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    format!("fam_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// SHA-256 of a presented token, for lookup/comparison.
#[must_use]
pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Outcome of atomically consuming (compare-and-revoke) a presented token.
///
/// Exactly one caller can ever win [`Consumed`](RefreshConsume::Consumed) for a
/// given token: the store performs the liveness check and the revoke as one
/// indivisible step, so two concurrent rotations of the same token cannot both
/// proceed to mint (which would break the one-live-token-per-family invariant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshConsume {
    /// No such token.
    NotFound,
    /// The token was live and THIS caller atomically revoked it; proceed to
    /// mint a successor into `family_id` for `user_id`.
    Consumed {
        /// Owning user id.
        user_id: String,
        /// Family the successor must join.
        family_id: String,
    },
    /// The token existed but was already revoked, or its family was already
    /// flagged compromised — a reuse/theft signal. The caller must compromise
    /// the family (idempotent) and reject.
    Reuse {
        /// Family to compromise.
        family_id: String,
    },
    /// The token was live but past its expiry.
    Expired,
}

/// Persistence + revocation for refresh tokens.
#[async_trait]
pub trait RefreshStore: Send + Sync {
    /// Persist a new record.
    async fn insert(&self, record: RefreshRecord) -> Result<()>;
    /// Look up a record by token hash.
    async fn get(&self, token_hash: &[u8; 32]) -> Result<Option<RefreshRecord>>;
    /// Atomically consume a presented token: in a single indivisible step,
    /// check that it is live (present, not revoked, family not compromised, not
    /// expired at `now`) and, if so, revoke it and report [`RefreshConsume::Consumed`].
    ///
    /// This is the rotation primitive. It MUST be atomic with respect to
    /// concurrent callers: the check and the revoke cannot be split across an
    /// await/round-trip, or two racing refreshes of the same token could both
    /// pass the check and mint two live successors. A persistent implementation
    /// backs this with a conditional update (e.g. `UPDATE ... SET revoked=true
    /// WHERE token_hash=$1 AND NOT revoked RETURNING user_id, family_id`) and
    /// treats "zero rows affected" on an existing token as reuse.
    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<RefreshConsume>;
    /// Mark a single record revoked (idempotent).
    async fn revoke(&self, token_hash: &[u8; 32]) -> Result<()>;
    /// Revoke *every* refresh token owned by a user, across all families
    /// (idempotent). Used by password reset to force re-authentication on every
    /// device after the credential changes. A persistent implementation performs
    /// this as one conditional update keyed by `user_id`.
    async fn revoke_all_for_user(&self, user_id: &str) -> Result<()>;
    /// Reuse/theft response: revoke *every* token in a family and flag the
    /// family compromised. Idempotent.
    ///
    /// Callers invoke this when an already-revoked token is re-presented (see
    /// the module docs). A persistent implementation MUST perform the revoke +
    /// flag atomically (one transaction) so a concurrent rotation cannot mint a
    /// survivor.
    ///
    // TODO(amicu-planning#96): when the persistent store lands, back the
    // compromised flag with a durable, cross-node table (indexed by
    // `family_id`). The user-facing "suspicious sign-in" notice can now ride the
    // notification seam added in amicu-planning#102
    // (`crate::notify::Notifier`); it is not wired here because
    // `compromise_family` sees only a `family_id`, not the owning user's email —
    // resolving that needs a family→user lookup this in-memory store does not
    // index yet. Wire it at the service call site once the persistent store
    // exposes that mapping.
    async fn compromise_family(&self, family_id: &str) -> Result<()>;
    /// Whether a family has been flagged compromised by reuse detection.
    /// Primarily an observability/audit hook — enforcement already follows from
    /// every token in the family having been revoked.
    async fn is_family_compromised(&self, family_id: &str) -> Result<bool>;
}

/// In-memory refresh store (tests, single-node dev).
#[derive(Default)]
pub struct InMemoryRefreshStore {
    inner: Mutex<InMemoryState>,
}

#[derive(Default)]
struct InMemoryState {
    tokens: HashMap<[u8; 32], RefreshRecord>,
    compromised: HashSet<String>,
}

#[async_trait]
impl RefreshStore for InMemoryRefreshStore {
    async fn insert(&self, record: RefreshRecord) -> Result<()> {
        self.inner
            .lock()
            .expect("refresh store poisoned")
            .tokens
            .insert(record.token_hash, record);
        Ok(())
    }

    async fn get(&self, token_hash: &[u8; 32]) -> Result<Option<RefreshRecord>> {
        Ok(self
            .inner
            .lock()
            .expect("refresh store poisoned")
            .tokens
            .get(token_hash)
            .cloned())
    }

    async fn consume(&self, token_hash: &[u8; 32], now: i64) -> Result<RefreshConsume> {
        // Whole check-and-revoke runs under one lock: no await splits it, so two
        // racing consumers of the same token cannot both observe it live.
        let mut state = self.inner.lock().expect("refresh store poisoned");
        let Some(record) = state.tokens.get(token_hash) else {
            return Ok(RefreshConsume::NotFound);
        };
        let family_id = record.family_id.clone();
        // Reuse: already revoked, or the family was flagged compromised (which
        // also catches a token minted into a family that was compromised after
        // this token's own liveness was last observed).
        if record.revoked || state.compromised.contains(&family_id) {
            return Ok(RefreshConsume::Reuse { family_id });
        }
        if record.expires_at <= now {
            return Ok(RefreshConsume::Expired);
        }
        let user_id = record.user_id.clone();
        // Win the race: revoke before releasing the lock.
        state
            .tokens
            .get_mut(token_hash)
            .expect("token present under lock")
            .revoked = true;
        Ok(RefreshConsume::Consumed { user_id, family_id })
    }

    async fn revoke(&self, token_hash: &[u8; 32]) -> Result<()> {
        if let Some(rec) = self
            .inner
            .lock()
            .expect("refresh store poisoned")
            .tokens
            .get_mut(token_hash)
        {
            rec.revoked = true;
        }
        Ok(())
    }

    async fn revoke_all_for_user(&self, user_id: &str) -> Result<()> {
        let mut state = self.inner.lock().expect("refresh store poisoned");
        for rec in state.tokens.values_mut() {
            if rec.user_id == user_id {
                rec.revoked = true;
            }
        }
        Ok(())
    }

    async fn compromise_family(&self, family_id: &str) -> Result<()> {
        let mut state = self.inner.lock().expect("refresh store poisoned");
        for rec in state.tokens.values_mut() {
            if rec.family_id == family_id {
                rec.revoked = true;
            }
        }
        state.compromised.insert(family_id.to_string());
        Ok(())
    }

    async fn is_family_compromised(&self, family_id: &str) -> Result<bool> {
        Ok(self
            .inner
            .lock()
            .expect("refresh store poisoned")
            .compromised
            .contains(family_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compromise_family_revokes_all_members_and_flags() {
        let store = InMemoryRefreshStore::default();
        let a = mint_in_family("usr_1", "fam_x", 9_999_999_999);
        let b = mint_in_family("usr_1", "fam_x", 9_999_999_999);
        let other = mint_in_family("usr_1", "fam_y", 9_999_999_999);
        let (a_hash, b_hash, other_hash) = (
            a.record.token_hash,
            b.record.token_hash,
            other.record.token_hash,
        );
        store.insert(a.record).await.unwrap();
        store.insert(b.record).await.unwrap();
        store.insert(other.record).await.unwrap();

        assert!(!store.is_family_compromised("fam_x").await.unwrap());
        store.compromise_family("fam_x").await.unwrap();

        assert!(store.is_family_compromised("fam_x").await.unwrap());
        assert!(store.get(&a_hash).await.unwrap().unwrap().revoked);
        assert!(store.get(&b_hash).await.unwrap().unwrap().revoked);
        // A different family is untouched.
        assert!(!store.get(&other_hash).await.unwrap().unwrap().revoked);
        assert!(!store.is_family_compromised("fam_y").await.unwrap());
    }

    #[test]
    fn family_ids_are_prefixed_and_unique() {
        let a = new_family_id();
        let b = new_family_id();
        assert!(a.starts_with("fam_"));
        assert_ne!(a, b);
    }
}
