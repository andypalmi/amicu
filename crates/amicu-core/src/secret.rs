//! Foundational secret types shared across adapter families.
//!
//! A [`SecretRef`] is metadata - a typed pointer to a secret by name/scope -
//! and is safe to serialize into plans, logs, and telemetry. A [`SecretValue`]
//! wraps the sensitive bytes, zeroizes them on drop, and is deliberately not
//! `Display`/`Serialize`; its redacted [`Debug`] never reveals the contents.
//! The [`SecretResolver`] abstraction is what the apply layer uses to turn a
//! `SecretRef` into a `SecretValue` at apply time - never during `plan`.

use std::str::Utf8Error;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::adapter::AdapterError;

/// A typed, serde-serializable reference to a secret.
///
/// This is metadata only and carries no secret material, so it is safe to
/// appear in plans, diagnostics, logs, and telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SecretRef {
    /// The secret's name within its backend.
    pub name: String,
    /// Optional logical scope (namespace, tenant, environment, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Optional version selector; `None` means the current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl SecretRef {
    /// A reference to the current version of a named secret in the default scope.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            scope: None,
            version: None,
        }
    }

    /// Set the logical scope.
    #[must_use]
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// Pin a specific version.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }
}

/// A zeroize-on-drop wrapper around sensitive secret material.
///
/// The value is never logged, planned, or sent in telemetry: it has no
/// `Display`, no `Serialize`, and a [`Debug`] that renders only `SecretValue(***)`.
/// The bytes are readable exclusively through the explicitly named
/// [`expose_secret`](Self::expose_secret) / [`expose_secret_str`](Self::expose_secret_str),
/// and are zeroized when the value is dropped.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretValue {
    bytes: Vec<u8>,
}

impl SecretValue {
    /// Wrap raw secret bytes (accepts `Vec<u8>`, `String`, `&[u8]`, ...).
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    /// Borrow the raw secret bytes. The name makes exposure explicit at call
    /// sites; callers must not log or serialize the result.
    #[must_use]
    pub fn expose_secret(&self) -> &[u8] {
        &self.bytes
    }

    /// Borrow the secret as UTF-8, if it is valid UTF-8.
    ///
    /// # Errors
    /// Returns [`Utf8Error`] when the bytes are not valid UTF-8.
    pub fn expose_secret_str(&self) -> Result<&str, Utf8Error> {
        std::str::from_utf8(&self.bytes)
    }

    /// Whether the wrapped secret is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Redacted: never renders the wrapped bytes.
impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(***)")
    }
}

/// Resolves a [`SecretRef`] to a [`SecretValue`] at apply time.
///
/// The apply layer depends on this narrow abstraction rather than on a
/// concrete secret backend; a secret-manager adapter is the usual
/// implementor. Resolution is an apply-time side effect and must never run
/// during `plan`.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolve a reference to its secret value.
    ///
    /// # Errors
    /// Returns an [`AdapterError`] (an `AMICU-POLICY-*` code for an
    /// unavailable secret) when resolution fails.
    async fn resolve(&self, secret: &SecretRef) -> Result<SecretValue, AdapterError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}

    #[test]
    fn secret_value_is_zeroize_on_drop() {
        // Compile-time proof that the zeroize-on-drop contract holds.
        assert_zeroize_on_drop::<SecretValue>();
    }

    #[test]
    fn debug_is_redacted_and_never_shows_contents() {
        let value = SecretValue::new("hunter2".to_string());
        let rendered = format!("{value:?}");
        assert_eq!(rendered, "SecretValue(***)");
        assert!(!rendered.contains("hunter2"));
        // Also inside a surrounding structure.
        let nested = format!("{:?}", Some(&value));
        assert!(!nested.contains("hunter2"));
    }

    #[test]
    fn expose_secret_recovers_bytes_and_str() {
        let value = SecretValue::new(b"top-secret".to_vec());
        assert_eq!(value.expose_secret(), b"top-secret");
        assert_eq!(value.expose_secret_str().unwrap(), "top-secret");
        assert!(!value.is_empty());
    }

    #[test]
    fn clone_is_independent() {
        let value = SecretValue::new("abc".to_string());
        let cloned = value.clone();
        drop(value);
        // The clone owns its own bytes and is unaffected by the original drop.
        assert_eq!(cloned.expose_secret_str().unwrap(), "abc");
    }

    #[test]
    fn zeroize_clears_bytes_in_place() {
        let mut value = SecretValue::new(vec![1u8, 2, 3, 4]);
        value.zeroize();
        assert!(value.expose_secret().iter().all(|&b| b == 0));
    }

    #[test]
    fn secret_ref_round_trips_as_metadata() {
        let full = SecretRef::new("db_password")
            .with_scope("prod")
            .with_version("3");
        let json = serde_json::to_string(&full).unwrap();
        assert_eq!(
            serde_json::from_str::<SecretRef>(&json).unwrap(),
            full,
            "SecretRef must round-trip"
        );
        assert!(json.contains("db_password"));

        // Optional fields are omitted when absent.
        let minimal = SecretRef::new("api_key");
        let json = serde_json::to_value(&minimal).unwrap();
        assert_eq!(json, serde_json::json!({"name": "api_key"}));
    }
}
