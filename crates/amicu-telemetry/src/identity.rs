//! Anonymous telemetry identities.
//!
//! - [`InstallId`]: a random 128-bit id for this installation. It is **not
//!   derived** from hardware, hostname, user, or anything else - it is pure
//!   randomness, generated once and stored locally, and can be rotated at any
//!   time (see [`Identity::rotate_install_id`]).
//! - [`AnonDeploymentId`]: a random id scoped to one deployment, so reports from
//!   the same deployment can be joined without revealing the deployment's name.
//!   The local mapping name -> anonymous id never leaves the machine.
//!
//! Persistence is behind the small [`IdentityStore`] trait; this crate makes no
//! decision about where or how ids are stored.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use amicu_core::id::DeploymentId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! random_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "Uuid", into = "Uuid")]
        pub struct $name(Uuid);

        impl TryFrom<Uuid> for $name {
            type Error = InvalidId;

            /// Only random (version 4), non-nil UUIDs are valid ids: anything
            /// else (a name-derived v5, a time-based v1 carrying a MAC, nil)
            /// could encode identity.
            fn try_from(value: Uuid) -> Result<Self, InvalidId> {
                if value.is_nil() {
                    Err(InvalidId("nil id"))
                } else if value.get_version() != Some(uuid::Version::Random) {
                    Err(InvalidId("id is not a random (v4) UUID"))
                } else {
                    Ok(Self(value))
                }
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Uuid {
                id.0
            }
        }

        impl $name {
            /// A fresh id from the operating system's random source.
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::new_v4())
            }

            /// An id from fixed bytes, apart from the UUIDv4 version bits.
            /// Test-only: production ids come from [`generate`](Self::generate).
            #[cfg(test)]
            #[must_use]
            pub(crate) fn from_random_bytes(bytes: [u8; 16]) -> Self {
                Self(uuid::Builder::from_random_bytes(bytes).into_uuid())
            }

            /// The id as a hyphenated lowercase UUID string.
            #[must_use]
            pub fn to_uuid_string(&self) -> String {
                self.0.to_string()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

random_id!(
    /// Anonymous, random, rotatable id of this installation.
    InstallId
);
random_id!(
    /// Anonymous, random id of one deployment; never the deployment's name.
    AnonDeploymentId
);
random_id!(
    /// Random id of one upload batch: the ingestion idempotency key.
    BatchId
);

/// A rejected id value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId(pub &'static str);

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidId {}

/// A persistence failure (opaque, secret-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "identity store error: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

/// Where anonymous ids are persisted. Implemented by the CLI/sidecar (file,
/// keyring, ...); this crate only needs these five operations.
pub trait IdentityStore {
    /// The stored install id, if any.
    ///
    /// # Errors
    /// [`StoreError`] on a read failure.
    fn load_install_id(&self) -> Result<Option<InstallId>, StoreError>;

    /// Persist the install id.
    ///
    /// # Errors
    /// [`StoreError`] on a write failure.
    fn save_install_id(&self, id: &InstallId) -> Result<(), StoreError>;

    /// The stored anonymous id for a deployment, if any.
    ///
    /// # Errors
    /// [`StoreError`] on a read failure.
    fn load_deployment_id(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Option<AnonDeploymentId>, StoreError>;

    /// Persist a deployment's anonymous id.
    ///
    /// # Errors
    /// [`StoreError`] on a write failure.
    fn save_deployment_id(
        &self,
        deployment: &DeploymentId,
        id: &AnonDeploymentId,
    ) -> Result<(), StoreError>;

    /// Forget every deployment mapping (used on rotation).
    ///
    /// # Errors
    /// [`StoreError`] on a write failure.
    fn clear_deployment_ids(&self) -> Result<(), StoreError>;
}

/// Get-or-create and rotation of anonymous ids over an [`IdentityStore`].
#[derive(Debug)]
pub struct Identity<S> {
    store: S,
}

impl<S: IdentityStore> Identity<S> {
    /// Wrap `store`.
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// The install id, generated and persisted on first use.
    ///
    /// # Errors
    /// [`StoreError`] if the store fails (no id is invented in that case).
    pub fn install_id(&self) -> Result<InstallId, StoreError> {
        if let Some(id) = self.store.load_install_id()? {
            return Ok(id);
        }
        let id = InstallId::generate();
        self.store.save_install_id(&id)?;
        Ok(id)
    }

    /// Replace the install id with a fresh random one. Deployment ids are
    /// rotated too, so old and new reports cannot be linked.
    ///
    /// # Errors
    /// [`StoreError`] if the store fails.
    pub fn rotate_install_id(&self) -> Result<InstallId, StoreError> {
        let id = InstallId::generate();
        self.store.save_install_id(&id)?;
        self.store.clear_deployment_ids()?;
        Ok(id)
    }

    /// The anonymous id for `deployment`, generated and persisted on first use.
    ///
    /// # Errors
    /// [`StoreError`] if the store fails.
    pub fn deployment_id(&self, deployment: &DeploymentId) -> Result<AnonDeploymentId, StoreError> {
        if let Some(id) = self.store.load_deployment_id(deployment)? {
            return Ok(id);
        }
        let id = AnonDeploymentId::generate();
        self.store.save_deployment_id(deployment, &id)?;
        Ok(id)
    }
}

/// An in-memory [`IdentityStore`] (tests, and ephemeral sidecars).
#[derive(Debug, Default)]
pub struct MemoryIdentityStore {
    inner: Mutex<MemoryState>,
}

#[derive(Debug, Default)]
struct MemoryState {
    install: Option<InstallId>,
    deployments: HashMap<DeploymentId, AnonDeploymentId>,
}

impl IdentityStore for MemoryIdentityStore {
    fn load_install_id(&self) -> Result<Option<InstallId>, StoreError> {
        Ok(self.inner.lock().expect("identity lock").install)
    }

    fn save_install_id(&self, id: &InstallId) -> Result<(), StoreError> {
        self.inner.lock().expect("identity lock").install = Some(*id);
        Ok(())
    }

    fn load_deployment_id(
        &self,
        deployment: &DeploymentId,
    ) -> Result<Option<AnonDeploymentId>, StoreError> {
        Ok(self
            .inner
            .lock()
            .expect("identity lock")
            .deployments
            .get(deployment)
            .copied())
    }

    fn save_deployment_id(
        &self,
        deployment: &DeploymentId,
        id: &AnonDeploymentId,
    ) -> Result<(), StoreError> {
        self.inner
            .lock()
            .expect("identity lock")
            .deployments
            .insert(deployment.clone(), *id);
        Ok(())
    }

    fn clear_deployment_ids(&self) -> Result<(), StoreError> {
        self.inner
            .lock()
            .expect("identity lock")
            .deployments
            .clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_id_is_stable_until_rotated() {
        let ident = Identity::new(MemoryIdentityStore::default());
        let a = ident.install_id().unwrap();
        assert_eq!(a, ident.install_id().unwrap());
        let b = ident.rotate_install_id().unwrap();
        assert_ne!(a, b);
        assert_eq!(b, ident.install_id().unwrap());
    }

    #[test]
    fn deployment_ids_are_per_deployment_random_and_not_the_name() {
        let ident = Identity::new(MemoryIdentityStore::default());
        let chat = DeploymentId::new("chat_prod");
        let a = ident.deployment_id(&chat).unwrap();
        assert_eq!(a, ident.deployment_id(&chat).unwrap());
        assert_ne!(a, ident.deployment_id(&DeploymentId::new("other")).unwrap());
        assert!(!a.to_uuid_string().contains("chat"));
    }

    #[test]
    fn rotation_unlinks_deployment_ids() {
        let ident = Identity::new(MemoryIdentityStore::default());
        let d = DeploymentId::new("chat_prod");
        let before = ident.deployment_id(&d).unwrap();
        ident.rotate_install_id().unwrap();
        assert_ne!(before, ident.deployment_id(&d).unwrap());
    }

    #[test]
    fn ids_are_random_uuid_strings_and_round_trip() {
        let a = InstallId::generate();
        assert_ne!(a, InstallId::generate());
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, format!("\"{a}\""));
        assert_eq!(serde_json::from_str::<InstallId>(&json).unwrap(), a);
        // Not derived from anything: identical inputs only when bytes are
        // supplied explicitly.
        assert_eq!(
            InstallId::from_random_bytes([7; 16]),
            InstallId::from_random_bytes([7; 16])
        );
    }

    #[test]
    fn only_random_non_nil_uuids_are_accepted() {
        let nil = format!("\"{}\"", Uuid::nil());
        assert!(serde_json::from_str::<InstallId>(&nil).is_err());
        let v5 = "\"6ba7b810-9dad-11d1-80b4-00c04fd430c8\"".to_string(); // a v1 (time/MAC-based) UUID
        assert!(serde_json::from_str::<AnonDeploymentId>(&v5).is_err());
        let v4 = format!("\"{}\"", Uuid::new_v4());
        assert!(serde_json::from_str::<InstallId>(&v4).is_ok());
    }

    #[test]
    fn failing_store_never_invents_an_id() {
        struct Broken;
        impl IdentityStore for Broken {
            fn load_install_id(&self) -> Result<Option<InstallId>, StoreError> {
                Err(StoreError("disk".into()))
            }
            fn save_install_id(&self, _: &InstallId) -> Result<(), StoreError> {
                Err(StoreError("disk".into()))
            }
            fn load_deployment_id(
                &self,
                _: &DeploymentId,
            ) -> Result<Option<AnonDeploymentId>, StoreError> {
                Err(StoreError("disk".into()))
            }
            fn save_deployment_id(
                &self,
                _: &DeploymentId,
                _: &AnonDeploymentId,
            ) -> Result<(), StoreError> {
                Err(StoreError("disk".into()))
            }
            fn clear_deployment_ids(&self) -> Result<(), StoreError> {
                Err(StoreError("disk".into()))
            }
        }
        assert!(Identity::new(Broken).install_id().is_err());
    }
}
