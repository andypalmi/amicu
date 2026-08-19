//! The capability token proving a caller holds the state lock.

use amicu_core::adapter::AdapterId;

/// Proof that a caller currently holds the state lock.
///
/// A handle pairs the holder's [`AdapterId`] with an opaque `token` the backend
/// mints on [`lock`](crate::StateBackendAdapter::lock). `push` and `rollback`
/// require the handle and the backend compares the token against the one it
/// recorded, so neither can overwrite state without the exact lock that guards
/// it. The token is not a secret vault, but a stale or forged handle is rejected
/// rather than honored.
///
/// # Security
/// [`LockHandle::new`] is public and the token is a plain `u64`, so a caller can
/// fabricate any handle. Real backends MUST therefore mint tokens from an
/// **unpredictable (random)** source: a sequential or otherwise guessable token
/// can be forged by an attacker to impersonate the lock holder and race a write
/// past the ownership check. The in-crate fake derives non-sequential tokens for
/// the same reason (see the test backend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockHandle {
    holder: AdapterId,
    token: u64,
}

impl LockHandle {
    /// Mint a handle for `holder` bound to backend-chosen `token`.
    #[must_use]
    pub fn new(holder: AdapterId, token: u64) -> Self {
        Self { holder, token }
    }

    /// The identity that acquired the lock.
    #[must_use]
    pub fn holder(&self) -> &AdapterId {
        &self.holder
    }

    /// The opaque token the backend matches to authorize a push.
    #[must_use]
    pub fn token(&self) -> u64 {
        self.token
    }
}
