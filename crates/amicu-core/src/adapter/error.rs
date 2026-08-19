//! Uniform adapter error model.
//!
//! Adapter families define their own [`thiserror`] enums but converge on a
//! single concrete [`AdapterError`] at every dyn-dispatched boundary. The
//! error carries a stable [`DiagnosticCode`] (`AMICU-<CATEGORY>-<NUMBER>`), a
//! [`Severity`], a `retryable` flag driving retry/backoff, and a safe,
//! secret-free message. Adapters never panic; they return this error instead.

use std::sync::Arc;

use crate::diagnostic::{DiagnosticCode, Severity};

/// The concrete, dyn-boundary-friendly error every adapter method returns.
///
/// A concrete struct (rather than `Box<dyn Trait>`) keeps `Result` handling
/// ergonomic at `Arc<dyn Adapter>` call sites while still preserving the
/// original cause via [`std::error::Error::source`]. Cloneable so a single
/// failure can fan out to multiple planning results.
#[derive(Debug, Clone)]
pub struct AdapterError {
    code: DiagnosticCode,
    severity: Severity,
    retryable: bool,
    message: String,
    source: Option<Arc<dyn std::error::Error + Send + Sync + 'static>>,
}

impl AdapterError {
    /// Construct an error from its classification and a safe message.
    ///
    /// The message must not contain secrets or raw credentials.
    #[must_use]
    pub fn new(
        code: DiagnosticCode,
        severity: Severity,
        retryable: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            severity,
            retryable,
            message: message.into(),
            source: None,
        }
    }

    /// Convenience constructor for a transient, error-severity failure that
    /// callers should retry with backoff.
    #[must_use]
    pub fn transient(code: DiagnosticCode, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Error, true, message)
    }

    /// Convenience constructor for a permanent, error-severity failure that
    /// retrying cannot resolve.
    #[must_use]
    pub fn permanent(code: DiagnosticCode, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Error, false, message)
    }

    /// Attach an underlying cause, exposed through [`std::error::Error::source`].
    ///
    /// The source is kept for diagnostics only; it is never rendered into the
    /// user-facing [`Display`](std::fmt::Display) message, so it cannot leak
    /// secrets there.
    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Arc::new(source));
        self
    }

    /// Thin delegator to [`AdapterErrorKind::into_adapter_error`], the
    /// canonical fold path. Retained for out-of-crate call sites; new code
    /// should call `error.into_adapter_error()` directly.
    #[inline]
    #[must_use]
    pub fn from_kind<E: AdapterErrorKind>(error: E) -> Self {
        error.into_adapter_error()
    }

    /// The stable diagnostic code, e.g. `AMICU-PROVIDER-001`.
    #[must_use]
    pub fn code(&self) -> DiagnosticCode {
        self.code
    }

    /// The severity of the failure.
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.severity
    }

    /// Whether the failure is transient and worth retrying.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    /// The safe, human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for AdapterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|s| &**s as &(dyn std::error::Error + 'static))
    }
}

/// Implemented by adapter-specific error enums to expose their classification.
///
/// A `thiserror` enum implements this to map each variant onto a stable
/// [`DiagnosticCode`], a [`Severity`], and a transient-vs-permanent verdict.
/// The default [`into_adapter_error`](AdapterErrorKind::into_adapter_error)
/// then folds the enum into the uniform [`AdapterError`], preserving the enum
/// as the error source. This is the `From`-style bridge every family reuses.
pub trait AdapterErrorKind: std::error::Error + Send + Sync + Sized + 'static {
    /// The stable diagnostic code for this error.
    fn code(&self) -> DiagnosticCode;

    /// The severity of this error.
    fn severity(&self) -> Severity;

    /// Whether this error is transient and safe to retry.
    fn retryable(&self) -> bool;

    /// Fold this classified error into the uniform [`AdapterError`].
    #[must_use]
    fn into_adapter_error(self) -> AdapterError {
        AdapterError::new(
            self.code(),
            self.severity(),
            self.retryable(),
            self.to_string(),
        )
        .with_source(self)
    }
}
