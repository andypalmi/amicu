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

/// Whether the secret `name` is set in the process environment to a valid
/// (UTF-8) value, as the resolver requires. Presence only: the value is
/// dropped and never logged.
pub(crate) fn env_has_secret(name: &str) -> bool {
    std::env::var(name).is_ok()
}

/// The secrets the launches in `result` need that are not set in the
/// environment, sorted and de-duplicated. Only deployments that will be
/// created, updated or replaced carry a launch, so secrets of deployments that
/// are only destroyed or left unchanged never appear here. Names only: values
/// are never read into the result.
pub(crate) fn missing_secrets(
    result: &amicu_engine::planner::PlanResult,
    is_set: fn(&str) -> bool,
) -> Vec<String> {
    let mut names: Vec<String> = result
        .items
        .iter()
        .filter_map(|i| i.launch.as_ref())
        .flat_map(|l| l.secret_refs.iter().chain(l.api_key.as_ref()))
        .filter(|s| !is_set(&s.name))
        .map(|s| s.name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The message for a non-empty [`missing_secrets`] list.
pub(crate) fn missing_secrets_message(names: &[String]) -> String {
    let list = names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let (noun, verb) = if names.len() == 1 {
        ("secret", "is")
    } else {
        ("secrets", "are")
    };
    format!("{noun} {list} {verb} not set in the environment")
}

/// An `AMICU-POLICY-002` error diagnostic naming every unset secret, or none.
pub(crate) fn missing_secrets_diagnostics(
    result: &amicu_engine::planner::PlanResult,
    is_set: fn(&str) -> bool,
) -> Vec<amicu_core::diagnostic::Diagnostic> {
    let names = missing_secrets(result, is_set);
    if names.is_empty() {
        return Vec::new();
    }
    vec![amicu_core::diagnostic::Diagnostic::new(
        DiagnosticCode::new(Category::Policy, 2),
        amicu_core::diagnostic::Severity::Error,
        missing_secrets_message(&names),
    )]
}
