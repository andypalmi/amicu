//! Secret-manager adapter family.
//!
//! A [`SecretManagerAdapter`] mediates access to an external secret backend.
//! Secret *values* ([`amicu_core::secret::SecretValue`]) are resolved only at
//! apply time and are never returned during `plan`, logged, or sent in
//! telemetry; a [`SecretRef`] is metadata and is safe to surface anywhere.
//!
//! Every failure classifies to an `AMICU-POLICY-*` code. In particular an
//! **unavailable secret is a policy error** ([`SecretError::Unavailable`]) that
//! blocks any new apply depending on it, rather than a transient fault.

use std::collections::HashMap;
use std::sync::Mutex;

use amicu_core::adapter::{
    Adapter, AdapterError, AdapterErrorKind, AdapterId, DescribeCapabilities, HealthStatus,
};
use amicu_core::diagnostic::{Category, DiagnosticCode, Severity};
use amicu_core::secret::{SecretRef, SecretValue};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// `AMICU-POLICY-010` - a referenced secret is unavailable; blocks new applies.
const SECRET_UNAVAILABLE: DiagnosticCode = DiagnosticCode::new(Category::Policy, 10);
/// `AMICU-POLICY-011` - the backend does not support the requested operation.
const SECRET_UNSUPPORTED: DiagnosticCode = DiagnosticCode::new(Category::Policy, 11);
/// `AMICU-POLICY-012` - the secret backend failed transiently.
const SECRET_BACKEND: DiagnosticCode = DiagnosticCode::new(Category::Policy, 12);

/// Plain-data descriptor of what a secret manager supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretCapabilities {
    /// Whether [`SecretManagerAdapter::put`] is supported.
    pub writable: bool,
    /// Whether [`SecretManagerAdapter::rotate`] is supported.
    pub rotation: bool,
    /// Whether version-pinned [`SecretRef`]s are supported.
    pub versioning: bool,
}

/// Errors raised by secret-manager adapters, all classified as `AMICU-POLICY-*`.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The referenced secret does not exist or cannot be reached. This is a
    /// policy block: dependent new applies cannot proceed until it is resolved.
    #[error("secret '{name}' is unavailable")]
    Unavailable {
        /// The name of the missing secret.
        name: String,
    },
    /// The backend does not support the attempted operation (e.g. `put` on a
    /// read-only manager, or `rotate` when rotation is unsupported).
    #[error("operation '{operation}' is not supported by this secret manager")]
    Unsupported {
        /// The unsupported operation.
        operation: &'static str,
    },
    /// The backend failed in a way that may succeed on retry.
    #[error("secret backend error: {detail}")]
    Backend {
        /// A safe, secret-free description of the failure.
        detail: String,
    },
}

impl AdapterErrorKind for SecretError {
    fn code(&self) -> DiagnosticCode {
        match self {
            SecretError::Unavailable { .. } => SECRET_UNAVAILABLE,
            SecretError::Unsupported { .. } => SECRET_UNSUPPORTED,
            SecretError::Backend { .. } => SECRET_BACKEND,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn retryable(&self) -> bool {
        // Only backend faults may clear on retry; unavailability and
        // unsupported operations are permanent policy blocks.
        matches!(self, SecretError::Backend { .. })
    }
}

/// A secret-manager adapter: resolves, stores, rotates, and lists secrets.
///
/// `resolve` runs only during apply. `list_refs` returns metadata references
/// and never secret values.
#[async_trait]
pub trait SecretManagerAdapter: Adapter + DescribeCapabilities {
    /// What this manager supports (write, rotation, versioning).
    fn capabilities(&self) -> SecretCapabilities;

    /// Resolve a reference to its secret value. Apply-time only.
    ///
    /// # Errors
    /// [`SecretError::Unavailable`] (`AMICU-POLICY-010`) when the secret is
    /// missing, folded into an [`AdapterError`].
    async fn resolve(&self, r: &SecretRef) -> Result<SecretValue, AdapterError>;

    /// Store (create or overwrite) a secret value.
    ///
    /// # Errors
    /// [`SecretError::Unsupported`] on a read-only manager, or a backend error.
    async fn put(&self, r: &SecretRef, v: SecretValue) -> Result<(), AdapterError>;

    /// Rotate a secret to a fresh value.
    ///
    /// # Errors
    /// [`SecretError::Unsupported`] when rotation is not supported, or
    /// [`SecretError::Unavailable`] for an unknown secret.
    async fn rotate(&self, r: &SecretRef) -> Result<(), AdapterError>;

    /// List references to every known secret - metadata only, never values.
    ///
    /// # Errors
    /// A backend error if the listing cannot be produced.
    async fn list_refs(&self) -> Result<Vec<SecretRef>, AdapterError>;
}

/// A secret-manager adapter is the apply layer's secret resolver: an
/// `Arc<dyn SecretManagerAdapter>` (or `&dyn SecretManagerAdapter`) can be
/// handed to any code that only needs [`SecretResolver`].
#[async_trait]
impl amicu_core::secret::SecretResolver for dyn SecretManagerAdapter {
    async fn resolve(&self, secret: &SecretRef) -> Result<SecretValue, AdapterError> {
        SecretManagerAdapter::resolve(self, secret).await
    }
}

/// An in-memory secret manager for tests and local development.
pub struct InMemorySecretManager {
    id: AdapterId,
    capabilities: SecretCapabilities,
    store: Mutex<HashMap<String, SecretValue>>,
}

impl InMemorySecretManager {
    /// A writable, rotation- and versioning-capable in-memory manager.
    #[must_use]
    pub fn new(id: &str) -> Self {
        Self {
            id: AdapterId::new(id),
            capabilities: SecretCapabilities {
                writable: true,
                rotation: true,
                versioning: false,
            },
            store: Mutex::new(HashMap::new()),
        }
    }

    /// Override the advertised capabilities (e.g. to simulate a read-only backend).
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: SecretCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SecretValue>> {
        self.store.lock().expect("secret store mutex poisoned")
    }
}

#[async_trait]
impl Adapter for InMemorySecretManager {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(HealthStatus::Healthy)
    }
}

impl DescribeCapabilities for InMemorySecretManager {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl SecretManagerAdapter for InMemorySecretManager {
    fn capabilities(&self) -> SecretCapabilities {
        self.capabilities
    }

    async fn resolve(&self, r: &SecretRef) -> Result<SecretValue, AdapterError> {
        self.lock().get(&r.name).cloned().ok_or_else(|| {
            AdapterError::from_kind(SecretError::Unavailable {
                name: r.name.clone(),
            })
        })
    }

    async fn put(&self, r: &SecretRef, v: SecretValue) -> Result<(), AdapterError> {
        if !self.capabilities.writable {
            return Err(AdapterError::from_kind(SecretError::Unsupported {
                operation: "put",
            }));
        }
        self.lock().insert(r.name.clone(), v);
        Ok(())
    }

    async fn rotate(&self, r: &SecretRef) -> Result<(), AdapterError> {
        if !self.capabilities.rotation {
            return Err(AdapterError::from_kind(SecretError::Unsupported {
                operation: "rotate",
            }));
        }
        let mut store = self.lock();
        if !store.contains_key(&r.name) {
            return Err(AdapterError::from_kind(SecretError::Unavailable {
                name: r.name.clone(),
            }));
        }
        // Deterministic stand-in rotation: replace with a fresh, distinct value.
        store.insert(
            r.name.clone(),
            SecretValue::new(format!("rotated:{}", r.name)),
        );
        Ok(())
    }

    async fn list_refs(&self) -> Result<Vec<SecretRef>, AdapterError> {
        Ok(self.lock().keys().map(SecretRef::new).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::adapter::Registry;
    use amicu_core::secret::SecretResolver;
    use std::sync::Arc;

    fn manager() -> InMemorySecretManager {
        InMemorySecretManager::new("vault")
    }

    #[tokio::test]
    async fn reports_capabilities() {
        let caps = manager().capabilities();
        assert!(caps.writable);
        assert!(caps.rotation);
        assert!(!caps.versioning);
    }

    #[tokio::test]
    async fn put_then_resolve_round_trips_value() {
        let m = manager();
        let r = SecretRef::new("db_password");
        m.put(&r, SecretValue::new("s3cr3t".to_string()))
            .await
            .unwrap();

        let value = m.resolve(&r).await.unwrap();
        assert_eq!(value.expose_secret_str().unwrap(), "s3cr3t");
    }

    #[tokio::test]
    async fn list_refs_returns_metadata_but_never_values() {
        let m = manager();
        m.put(
            &SecretRef::new("a"),
            SecretValue::new("value-a".to_string()),
        )
        .await
        .unwrap();
        m.put(
            &SecretRef::new("b"),
            SecretValue::new("value-b".to_string()),
        )
        .await
        .unwrap();

        let refs = m.list_refs().await.unwrap();
        assert_eq!(refs.len(), 2);

        // The listed refs, and their serialization, never contain any value.
        let json = serde_json::to_string(&refs).unwrap();
        assert!(json.contains("\"a\"") && json.contains("\"b\""));
        assert!(!json.contains("value-a"));
        assert!(!json.contains("value-b"));
    }

    #[tokio::test]
    async fn rotate_replaces_the_value() {
        let m = manager();
        let r = SecretRef::new("api_key");
        m.put(&r, SecretValue::new("original".to_string()))
            .await
            .unwrap();

        m.rotate(&r).await.unwrap();
        let rotated = m.resolve(&r).await.unwrap();
        assert_ne!(rotated.expose_secret_str().unwrap(), "original");
    }

    #[tokio::test]
    async fn resolve_missing_secret_is_a_policy_error() {
        let err = manager()
            .resolve(&SecretRef::new("nope"))
            .await
            .expect_err("missing secret");

        assert_eq!(err.code(), SECRET_UNAVAILABLE);
        assert_eq!(err.code().to_string(), "AMICU-POLICY-010");
        assert_eq!(err.code().category(), Category::Policy);
        assert_eq!(err.severity(), Severity::Error);
        // Unavailability is a permanent policy block, not a transient fault.
        assert!(!err.is_retryable());
        assert!(std::error::Error::source(&err).is_some());
    }

    #[tokio::test]
    async fn read_only_manager_rejects_put_as_policy_error() {
        let m = manager().with_capabilities(SecretCapabilities {
            writable: false,
            rotation: false,
            versioning: false,
        });
        let err = m
            .put(&SecretRef::new("x"), SecretValue::new("v".to_string()))
            .await
            .expect_err("read-only");
        assert_eq!(err.code(), SECRET_UNSUPPORTED);
        assert_eq!(err.code().category(), Category::Policy);
    }

    #[tokio::test]
    async fn registry_registers_and_discovers_secret_managers() {
        let mut registry: Registry<dyn SecretManagerAdapter> = Registry::new();
        registry.register(Arc::new(manager()));
        registry.register(Arc::new(InMemorySecretManager::new("kms")));

        assert_eq!(registry.len(), 2);
        let found = registry.get(&AdapterId::new("vault")).expect("registered");
        assert!(found.capabilities().writable);

        let discovered = registry.discover();
        assert_eq!(discovered.len(), 2);
        // Discovery exposes only capabilities, never any secret material.
        for entry in discovered {
            assert!(entry.capabilities.get("writable").is_some());
            assert!(entry.capabilities.get("versioning").is_some());
        }
    }

    #[tokio::test]
    async fn usable_as_secret_resolver() {
        let m = manager();
        let r = SecretRef::new("token");
        m.put(&r, SecretValue::new("abc123".to_string()))
            .await
            .unwrap();

        // A `dyn SecretManagerAdapter` is usable through the core apply-layer
        // abstraction, decoupled from the concrete backend.
        let manager: &dyn SecretManagerAdapter = &m;
        let value = SecretResolver::resolve(manager, &r).await.unwrap();
        assert_eq!(value.expose_secret_str().unwrap(), "abc123");
    }

    #[tokio::test]
    async fn discover_output_never_contains_secret_contents() {
        let m = manager();
        m.put(
            &SecretRef::new("k"),
            SecretValue::new("leak-me".to_string()),
        )
        .await
        .unwrap();

        let mut registry: Registry<dyn SecretManagerAdapter> = Registry::new();
        registry.register(Arc::new(m));
        let json = serde_json::to_string(&registry.discover()).unwrap();
        assert!(!json.contains("leak-me"));
    }
}
