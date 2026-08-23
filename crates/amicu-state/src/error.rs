//! State-family error model, classified into the uniform [`AdapterError`].
//!
//! Every variant carries a stable `AMICU-STATE-*` diagnostic code. Two failure
//! modes are load-bearing for correctness and demand explicit recovery, never a
//! silent overwrite: lock contention ([`StateError::LockContention`]) and state
//! corruption ([`StateError::Corrupted`]). A push or rollback presented without
//! the lock guarding the state ([`StateError::LockNotHeld`]) is likewise rejected.

use amicu_core::adapter::{AdapterError, AdapterErrorKind};
use amicu_core::diagnostic::{Category, DiagnosticCode, Severity};

/// `AMICU-STATE-001` - state is locked by another operation.
///
/// Re-exported from the core catalog rather than re-declared, so this crate and
/// core cannot drift on the shared code.
pub use amicu_core::diagnostic::catalog::STATE_LOCKED;

// `AMICU-STATE-002/003/004` have no core catalog entry yet; these are an
// intentional local mirror pending a core seed. Keep them numerically aligned
// with `Category::State`.
/// `AMICU-STATE-002` - a push or rollback was presented without the lock guarding the state.
pub const STATE_LOCK_NOT_HELD: DiagnosticCode = DiagnosticCode::new(Category::State, 2);
/// `AMICU-STATE-003` - recorded state is corrupt and needs manual recovery.
pub const STATE_CORRUPTED: DiagnosticCode = DiagnosticCode::new(Category::State, 3);
/// `AMICU-STATE-004` - the requested history version does not exist.
pub const STATE_VERSION_NOT_FOUND: DiagnosticCode = DiagnosticCode::new(Category::State, 4);
/// `AMICU-STATE-005` - the backend's underlying storage failed (connectivity,
/// driver error). Distinct from `STATE_CORRUPTED`: the recorded state itself
/// is not known to be bad, the backend simply could not be reached right now.
pub const STATE_BACKEND: DiagnosticCode = DiagnosticCode::new(Category::State, 5);

/// Failures a state backend can surface, each mapped to an `AMICU-STATE-*` code.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The state is already locked by another operation. Contention is surfaced
    /// explicitly rather than waiting forever; callers may retry with backoff.
    #[error("state is locked by another operation (holder: {holder})")]
    LockContention {
        /// Identity currently holding the lock.
        holder: String,
    },

    /// A push or rollback was rejected because no matching lock is held. The
    /// state is never overwritten without the lock that guards it; the caller
    /// must acquire the lock first and retry.
    #[error("rejected: the presented lock does not match the held lock")]
    LockNotHeld,

    /// The recorded state failed its integrity check. This is non-retryable and
    /// requires manual recovery; the backend must never silently overwrite it.
    #[error("state is corrupt and requires manual recovery: {detail}")]
    Corrupted {
        /// Safe, secret-free summary of the integrity failure.
        detail: String,
    },

    /// A rollback (or lookup) referenced a version absent from history.
    #[error("unknown state version: {version}")]
    VersionNotFound {
        /// The version id that could not be found.
        version: String,
    },

    /// The backend's underlying storage failed (connectivity, driver error).
    /// Unlike [`StateError::Corrupted`], the recorded state itself is not
    /// known to be bad; the backend simply could not be reached right now, so
    /// callers may retry.
    #[error("state backend error: {detail}")]
    Backend {
        /// Safe, secret-free summary of the failure.
        detail: String,
    },
}

impl AdapterErrorKind for StateError {
    fn code(&self) -> DiagnosticCode {
        match self {
            StateError::LockContention { .. } => STATE_LOCKED,
            StateError::LockNotHeld => STATE_LOCK_NOT_HELD,
            StateError::Corrupted { .. } => STATE_CORRUPTED,
            StateError::VersionNotFound { .. } => STATE_VERSION_NOT_FOUND,
            StateError::Backend { .. } => STATE_BACKEND,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn retryable(&self) -> bool {
        // Contention may clear once the holder releases the lock, and a backend
        // hiccup (connectivity) may clear on retry; a missing lock, corruption,
        // and an unknown version all require deliberate action first.
        matches!(
            self,
            StateError::LockContention { .. } | StateError::Backend { .. }
        )
    }
}

impl From<StateError> for AdapterError {
    fn from(error: StateError) -> Self {
        error.into_adapter_error()
    }
}
