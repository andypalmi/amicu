//! Typed errors for the GCP client.

/// Everything that can go wrong talking to Google Cloud or validating amicu
/// config for it. No variant carries a token or key: `message` strings come
/// from Google's `error.message` (truncated) and credential-loading errors are
/// reduced to a fixed text.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum GcpError {
    /// No usable Application Default Credentials.
    #[error(
        "no Google Cloud credentials found: run `gcloud auth application-default login`, \
         or set GOOGLE_APPLICATION_CREDENTIALS to a service-account key file"
    )]
    MissingCredentials,
    /// Credentials exist but could not be loaded (malformed file, unsupported type).
    #[error(
        "Google Cloud credentials could not be loaded (check GOOGLE_APPLICATION_CREDENTIALS \
         or re-run `gcloud auth application-default login`)"
    )]
    InvalidCredentials,
    /// Exchanging the credentials for an access token failed.
    #[error(
        "could not obtain a Google Cloud access token (check your credentials, or the \
         impersonation permissions if a service account is set); retryable: {retryable}"
    )]
    Token { retryable: bool },
    /// The impersonation target is not a service-account email.
    #[error("invalid service account to impersonate `{0}` (expected an email address)")]
    InvalidServiceAccount(String),
    /// The exchange never completed (timeout, DNS, TLS). Transient.
    #[error("Google Cloud transport failure: {0}")]
    Transport(String),
    /// `401`/`403` (not a quota error): the token was rejected or lacks permission.
    #[error("Google Cloud denied the request (HTTP {status}, {code}): {message}")]
    PermissionDenied {
        status: u16,
        code: String,
        message: String,
    },
    /// `404`: the resource does not exist (teardown reads this as "gone").
    #[error("Google Cloud resource not found: {what}")]
    NotFound { what: String },
    /// `409`: the resource already exists or is being modified.
    #[error("Google Cloud conflict: {message}")]
    Conflict { message: String },
    /// A hard quota (`quotaExceeded`, `RESOURCE_EXHAUSTED` on a non-`429`
    /// status). Retrying does not help: a quota raise or another region is
    /// needed, so this is not retryable.
    #[error("Google Cloud quota exceeded: {message}")]
    QuotaExceeded { message: String },
    /// Rate limiting (`429`, `rateLimitExceeded`, `RATE_LIMIT_EXCEEDED`).
    /// Retrying after a pause is expected to succeed.
    #[error("Google Cloud rate limit exceeded: {message}")]
    RateLimited { message: String },
    /// `400`/`422`: the request was rejected.
    #[error("Google Cloud rejected the request (HTTP {status}, {code}): {message}")]
    BadRequest {
        status: u16,
        code: String,
        message: String,
    },
    /// Any other non-`2xx` status.
    #[error("Google Cloud API returned HTTP {status} ({code}): {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },
    /// A `2xx` body that did not match the documented shape.
    #[error("Google Cloud returned an unexpected response body: {0}")]
    Decode(String),
    /// A long-running operation finished with an error.
    #[error("Google Cloud operation `{operation}` failed ({code}): {message}")]
    OperationFailed {
        operation: String,
        code: String,
        message: String,
    },
    /// A long-running operation did not finish within the deadline. It may
    /// still complete: look the resource up before retrying.
    #[error("Google Cloud operation `{operation}` did not finish in time")]
    OperationTimeout { operation: String },
    /// A resource without amicu's owner label: left alone.
    #[error("{what} does not carry amicu's owner label; refusing to touch it")]
    NotOwned { what: String },
    /// A project id, region, zone, label or resource id of the wrong shape.
    #[error("invalid {kind} `{value}`: {reason}")]
    Invalid {
        kind: &'static str,
        value: String,
        reason: &'static str,
    },
}

impl GcpError {
    /// Whether retrying the same call later can plausibly succeed.
    ///
    /// This says a retry is not futile, not that it is safe: a
    /// [`GcpError::Transport`] failure on a create `POST` may have been applied
    /// by Google, so create calls must not be retried automatically on
    /// transport errors. Look the resource up first (the owner label and name
    /// make that possible) and only then create again.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::RateLimited { .. } => true,
            Self::Token { retryable } => *retryable,
            Self::Api { status, .. } => *status >= 500,
            _ => false,
        }
    }

    /// Whether the error is Google reporting a hard quota (not a rate limit).
    #[must_use]
    pub fn is_quota(&self) -> bool {
        matches!(self, Self::QuotaExceeded { .. })
    }

    /// Whether the error is Google rate limiting the caller.
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }

    /// Whether the resource is confirmed absent.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}
