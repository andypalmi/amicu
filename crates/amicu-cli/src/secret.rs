//! A minimal environment-backed secret resolver for local apply.

use amicu_core::adapter::AdapterError;
use amicu_core::diagnostic::{Category, DiagnosticCode};
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use async_trait::async_trait;

/// Resolves secret references from process environment variables.
///
/// The local llama.cpp path references no secrets today, so this is exercised
/// only if a deployment declares one. Values are never logged or planned.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EnvSecretResolver;

#[async_trait]
impl SecretResolver for EnvSecretResolver {
    async fn resolve(&self, secret: &SecretRef) -> Result<SecretValue, AdapterError> {
        match std::env::var(&secret.name) {
            Ok(value) => Ok(SecretValue::new(value)),
            Err(_) => Err(AdapterError::permanent(
                DiagnosticCode::new(Category::Policy, 2),
                format!("secret '{}' is not set in the environment", secret.name),
            )),
        }
    }
}
