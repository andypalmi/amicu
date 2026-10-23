//! Short-lived OAuth access tokens from Application Default Credentials.
//!
//! [`AdcTokenSource`] wraps Google's `google-cloud-auth`, which follows the
//! standard ADC order (`GOOGLE_APPLICATION_CREDENTIALS` key file, the file
//! `gcloud auth application-default login` writes, then the metadata server on
//! Google Cloud) and caches and refreshes tokens. When a service account is
//! configured to impersonate (an explicit [`AuthConfig`] value, else
//! [`IMPERSONATE_ENV_VAR`]), the ADC identity mints tokens for it instead.
//!
//! The client only sees the [`TokenSource`] trait, so tests use a fake. A token
//! lives in an [`AccessToken`], whose `Debug` redacts it; key files are read
//! only by `google-cloud-auth` and no error here quotes their contents.

use std::fmt;

use async_trait::async_trait;
use google_cloud_auth::credentials::{self, AccessTokenCredentials};

use crate::error::GcpError;

/// Environment variable naming a service account to impersonate (the same
/// variable gcloud and Terraform read).
pub const IMPERSONATE_ENV_VAR: &str = "GOOGLE_IMPERSONATE_SERVICE_ACCOUNT";

/// An OAuth access token. `Debug` redacts it and there is no `Display`; read it
/// with [`expose`](Self::expose) only to build an `Authorization` header.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(String);

impl AccessToken {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The raw token. Never log or print it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(<redacted>)")
    }
}

/// Where the client gets its bearer tokens.
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// A currently valid access token, refreshed when it is about to expire.
    async fn token(&self) -> Result<AccessToken, GcpError>;
}

/// How [`AdcTokenSource`] authenticates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthConfig {
    /// Service account to impersonate. Overrides [`IMPERSONATE_ENV_VAR`].
    pub impersonate_service_account: Option<String>,
}

/// The service account to impersonate: `explicit` when set, else `env`
/// (the value of [`IMPERSONATE_ENV_VAR`]); blank values count as unset.
///
/// # Errors
/// [`GcpError::InvalidServiceAccount`] when the value is not an email address.
pub fn impersonation_target(
    explicit: Option<&str>,
    env: Option<&str>,
) -> Result<Option<String>, GcpError> {
    fn pick(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|v| !v.is_empty())
    }
    let Some(sa) = pick(explicit).or_else(|| pick(env)) else {
        return Ok(None);
    };
    let valid = sa
        .split_once('@')
        .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
        && !sa.chars().any(|c| c.is_whitespace() || c.is_control());
    if valid {
        Ok(Some(sa.to_owned()))
    } else {
        Err(GcpError::InvalidServiceAccount(sa.to_owned()))
    }
}

/// [`TokenSource`] over Application Default Credentials, optionally
/// impersonating a service account.
pub struct AdcTokenSource {
    credentials: AccessTokenCredentials,
    impersonating: Option<String>,
}

impl fmt::Debug for AdcTokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdcTokenSource")
            .field("impersonating", &self.impersonating)
            .finish_non_exhaustive()
    }
}

impl AdcTokenSource {
    /// Builds the credentials from the environment. Reads local files and
    /// variables only; no network call happens until the first token is asked
    /// for.
    ///
    /// # Errors
    /// [`GcpError::MissingCredentials`] when the key file named by
    /// `GOOGLE_APPLICATION_CREDENTIALS` cannot be read,
    /// [`GcpError::InvalidCredentials`] when it is malformed, and
    /// [`GcpError::InvalidServiceAccount`] for a bad impersonation target.
    pub fn new(config: &AuthConfig) -> Result<Self, GcpError> {
        let env = std::env::var(IMPERSONATE_ENV_VAR).ok();
        let target = impersonation_target(
            config.impersonate_service_account.as_deref(),
            env.as_deref(),
        )?;
        let credentials = match &target {
            None => credentials::Builder::default().build_access_token_credentials(),
            Some(sa) => credentials::Builder::default().build().and_then(|source| {
                credentials::impersonated::Builder::from_source_credentials(source)
                    .with_target_principal(sa.as_str())
                    .build_access_token_credentials()
            }),
        }
        .map_err(|e| {
            if e.is_loading() {
                GcpError::MissingCredentials
            } else {
                GcpError::InvalidCredentials
            }
        })?;
        Ok(Self {
            credentials,
            impersonating: target,
        })
    }

    /// The service account tokens are minted for, when impersonating.
    #[must_use]
    pub fn impersonating(&self) -> Option<&str> {
        self.impersonating.as_deref()
    }
}

#[async_trait]
impl TokenSource for AdcTokenSource {
    async fn token(&self) -> Result<AccessToken, GcpError> {
        match self.credentials.access_token().await {
            Ok(t) => Ok(AccessToken::new(t.token)),
            Err(e) => Err(classify_token_error(
                adc_file_present(),
                e.is_transient(),
                is_not_found_shaped(&e),
            )),
        }
    }
}

/// Maps a failed token request to a [`GcpError`].
///
/// With no ADC file the chain falls through to the metadata server, which is
/// absent off Google Cloud: a non-transient, not-found-shaped failure then
/// means "not logged in" ([`GcpError::MissingCredentials`]). Anything else
/// (a transient network error, a rejected refresh token, an ADC file that
/// exists but fails) is returned as a token error, retryable only when the
/// underlying error is transient.
fn classify_token_error(adc_file_present: bool, transient: bool, not_found: bool) -> GcpError {
    if !adc_file_present && !transient && not_found {
        GcpError::MissingCredentials
    } else {
        GcpError::Token {
            retryable: transient,
        }
    }
}

/// Whether the error, or anything in its source chain, reads as "not found":
/// an I/O `NotFound`, or a message saying the resource or file was not found.
fn is_not_found_shaped(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if e.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        {
            return true;
        }
        let msg = e.to_string().to_ascii_lowercase();
        if msg.contains("not found") || msg.contains("404") || msg.contains("no such file") {
            return true;
        }
        cur = e.source();
    }
    false
}

/// Candidate directories holding `application_default_credentials.json`, in
/// lookup order. Every candidate is probed, so a Windows machine with `HOME`
/// set (Git Bash) but credentials under `APPDATA` still counts as logged in.
fn adc_dirs() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let var = |name| std::env::var_os(name).map(PathBuf::from);
    let appdata = var("APPDATA").map(|a| a.join("gcloud"));
    let home = var("HOME").map(|h| h.join(".config/gcloud"));
    // gcloud uses %APPDATA%\gcloud on Windows and ~/.config/gcloud elsewhere.
    let (first, second) = if cfg!(windows) {
        (appdata, home)
    } else {
        (home, appdata)
    };
    [var("CLOUDSDK_CONFIG"), first, second]
        .into_iter()
        .flatten()
        .collect()
}

/// Whether an ADC file exists: the one `GOOGLE_APPLICATION_CREDENTIALS` names
/// or the one `gcloud auth application-default login` writes. Credentials of
/// the metadata server cannot be detected offline, so off an ADC file this
/// reports "missing".
fn adc_file_present() -> bool {
    if std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").is_some() {
        return true;
    }
    adc_dirs()
        .iter()
        .any(|d| d.join("application_default_credentials.json").is_file())
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    /// A token source returning a fixed token, or a fixed error.
    pub(crate) struct FakeTokens(pub(crate) Result<&'static str, GcpError>);

    #[async_trait]
    impl TokenSource for FakeTokens {
        async fn token(&self) -> Result<AccessToken, GcpError> {
            self.0.clone().map(AccessToken::new)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_debug_is_redacted() {
        let t = AccessToken::new("ya29.SECRET");
        assert!(!format!("{t:?}").contains("SECRET"));
        assert_eq!(t.expose(), "ya29.SECRET");
    }

    #[test]
    fn explicit_service_account_wins_over_env() {
        let got = impersonation_target(
            Some("a@p.iam.gserviceaccount.com"),
            Some("b@p.iam.gserviceaccount.com"),
        )
        .unwrap();
        assert_eq!(got.as_deref(), Some("a@p.iam.gserviceaccount.com"));
    }

    #[test]
    fn env_service_account_is_used_when_no_explicit_value() {
        let got = impersonation_target(None, Some(" b@p.iam.gserviceaccount.com ")).unwrap();
        assert_eq!(got.as_deref(), Some("b@p.iam.gserviceaccount.com"));
        // A blank explicit value falls through to the environment.
        let got = impersonation_target(Some(""), Some("b@p.iam.gserviceaccount.com")).unwrap();
        assert_eq!(got.as_deref(), Some("b@p.iam.gserviceaccount.com"));
    }

    #[test]
    fn no_impersonation_when_nothing_is_set() {
        assert_eq!(impersonation_target(None, None).unwrap(), None);
        assert_eq!(impersonation_target(Some(" "), Some("")).unwrap(), None);
    }

    #[test]
    fn a_non_email_target_is_rejected() {
        for bad in ["sa", "@x.com", "a@b", "a b@x.com"] {
            assert_eq!(
                impersonation_target(Some(bad), None),
                Err(GcpError::InvalidServiceAccount(bad.to_owned())),
                "{bad}"
            );
        }
    }

    #[test]
    fn token_error_maps_to_missing_credentials_only_when_not_logged_in() {
        // No ADC file, permanent, not-found-shaped: not logged in.
        assert_eq!(
            classify_token_error(false, false, true),
            GcpError::MissingCredentials
        );
        // A transient failure (metadata server unreachable, 5xx) is retryable.
        assert_eq!(
            classify_token_error(false, true, true),
            GcpError::Token { retryable: true }
        );
        // Permanent but not not-found-shaped (rejected credentials).
        assert_eq!(
            classify_token_error(false, false, false),
            GcpError::Token { retryable: false }
        );
        // An ADC file exists: never "missing", whatever the failure.
        assert_eq!(
            classify_token_error(true, false, true),
            GcpError::Token { retryable: false }
        );
        assert_eq!(
            classify_token_error(true, true, false),
            GcpError::Token { retryable: true }
        );
    }

    #[test]
    fn not_found_shape_is_read_from_the_source_chain() {
        let nf = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert!(is_not_found_shaped(&nf));
        let other = std::io::Error::other("boom");
        assert!(!is_not_found_shaped(&other));
        let wrapped = std::io::Error::other(std::io::Error::other("metadata: 404 Not Found"));
        assert!(is_not_found_shaped(&wrapped));
    }

    #[test]
    fn missing_credentials_error_names_the_login_command() {
        let msg = GcpError::MissingCredentials.to_string();
        assert!(msg.contains("gcloud auth application-default login"));
    }
}
