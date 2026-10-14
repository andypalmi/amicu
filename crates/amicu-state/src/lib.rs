//! State backend adapters: canonical state, locking, history, and drift.
//!
//! A [`StateBackendAdapter`] stores the canonical [`StateSnapshot`], mediates
//! concurrent writes with an explicit [`LockHandle`], keeps a [`StateVersion`]
//! history for rollback, and reports [`DriftReport`]s between a desired graph
//! and the recorded one. It extends the common
//! [`Adapter`](amicu_core::adapter::Adapter) contract, so instances live in an
//! [`amicu_core::adapter::Registry`] alongside every other adapter family.
//!
//! Two failure modes demand explicit recovery and are never resolved by a
//! silent overwrite: lock contention and state corruption. Both surface as
//! `AMICU-STATE-*` [`AdapterError`](amicu_core::adapter::AdapterError)s via
//! [`StateError`].
//!
//! [`cipher`] defines the optional encryption-at-rest seam ([`StateCipher`],
//! default [`IdentityCipher`]) a backend can wire in at the point snapshot
//! bytes cross the persistence boundary - see that module's docs for scope
//! and the self-host operator's own encryption-at-rest responsibility.
//!
//! [`InMemoryStateBackend`] is the zero-configuration, process-local
//! implementation used for local runs, tests, and pre-database self-host.
//! `amicu-store`'s `PostgresStateBackend` is the durable, production
//! implementation selected instead whenever `DATABASE_URL` is set - see
//! `amicu-server`'s `build_app`.

mod adapter;
mod cipher;
mod clock;
mod drift;
mod error;
mod lock;
mod memory;
mod renew;
mod runtime;
mod snapshot;

pub use adapter::StateBackendAdapter;
pub use cipher::{IdentityCipher, StateCipher};
pub use drift::{DriftReport, detect_drift};
pub use error::{
    STATE_BACKEND, STATE_CORRUPTED, STATE_LOCK_NOT_HELD, STATE_LOCKED, STATE_STOP_NOT_CONFIRMED,
    STATE_VERSION_NOT_FOUND, StateError,
};
pub use lock::LockHandle;
pub use memory::InMemoryStateBackend;
pub use renew::LeaseKeeper;
pub use runtime::{ConfirmedStop, DeploymentPhase, DeploymentRecord, RuntimeState, StopIdentity};
pub use snapshot::{StateSnapshot, StateVersion};

#[cfg(test)]
mod tests;
