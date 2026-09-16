//! Real SMTP delivery for transactional auth emails.
//!
//! [`SmtpNotifier`] implements the [`Notifier`](amicu_auth::notify::Notifier)
//! seam with an actual mailer, so verification and password-reset messages are
//! sent over SMTP instead of printed to stderr. It lives in this binary crate on
//! purpose: the `lettre` dependency (and its TLS stack) stays out of the core
//! `amicu-auth` crate, which depends only on the abstract seam.
//!
//! # One env surface, two deployment shapes
//!
//! The same `AMICU_SMTP_*` variables serve both a self-hosted BYO-SMTP server
//! and a provider relay (for example Resend). BYO-SMTP points them at the
//! operator's own server; a provider relay points them at the provider's
//! submission host with the API key supplied as the SMTP password.
//!
//! | Env var | Required | Purpose |
//! |---------|----------|---------|
//! | `AMICU_SMTP_HOST` | yes | Submission host (for example `smtp.resend.com`). |
//! | `AMICU_SMTP_FROM` | yes | `From` address (`no-reply@example.com` or `Name <no-reply@example.com>`). |
//! | `AMICU_WEB_URL` | yes | Public base URL of the web app, used to build verify/reset links. |
//! | `AMICU_SMTP_PORT` | no | Submission port. Defaults to 465 (implicit TLS) or 587 (STARTTLS). |
//! | `AMICU_SMTP_USERNAME` | no | SMTP auth username (Resend uses the literal `resend`). |
//! | `AMICU_SMTP_PASSWORD` | no | SMTP auth password / provider API key. Never logged. |
//! | `AMICU_SMTP_STARTTLS` | no | `true` to upgrade a plaintext connection with STARTTLS; default is implicit TLS. |
//!
//! # TLS
//!
//! The transport is always encrypted (implicit TLS, or STARTTLS upgrade) and
//! uses `rustls` with the `ring` crypto backend and bundled webpki roots, to
//! match the workspace's hermetic, no-native-tls convention. Secret material
//! (the SMTP password, the opaque tokens) is carried only inside the TLS
//! session and is never written to a log.

use std::sync::Arc;

use amicu_auth::error::AuthError;
use amicu_auth::notify::{Notification, NotificationKind, Notifier};
use async_trait::async_trait;
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

/// Default submission port for implicit TLS (SMTPS).
const IMPLICIT_TLS_PORT: u16 = 465;
/// Default submission port for a STARTTLS upgrade.
const STARTTLS_PORT: u16 = 587;

/// A configuration failure while assembling an [`SmtpNotifier`].
///
/// Deliberately carries no secret material: the SMTP password and the opaque
/// auth tokens never appear in any variant.
#[derive(Debug)]
pub enum SmtpConfigError {
    /// A required configuration variable was missing or empty.
    MissingVar(&'static str),
    /// `AMICU_SMTP_PORT` was set but not a valid `u16`. The offending value is
    /// echoed (a port number is not a secret).
    InvalidPort(String),
    /// `AMICU_SMTP_STARTTLS` was set but not a recognized boolean.
    InvalidBool {
        /// The variable that failed to parse.
        var: &'static str,
        /// The offending value (not a secret).
        value: String,
    },
    /// `AMICU_SMTP_FROM` was not a valid mailbox. The address string is echoed
    /// (an address is not a secret); no password is ever involved here.
    InvalidFrom(String),
    /// Exactly one of `AMICU_SMTP_USERNAME` / `AMICU_SMTP_PASSWORD` was set.
    /// SMTP credentials are all-or-nothing: a lone username would submit an
    /// empty password, and a lone password would be silently dropped and submit
    /// unauthenticated. Only the variable *names* are reported; the password
    /// value is never echoed.
    IncompleteCredentials {
        /// The credential variable that was present.
        present: &'static str,
        /// The credential variable that was missing.
        missing: &'static str,
    },
    /// The `lettre` SMTP transport could not be built (for example a bad host).
    Transport(lettre::transport::smtp::Error),
}

impl std::fmt::Display for SmtpConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SmtpConfigError::MissingVar(var) => {
                write!(f, "required SMTP env var {var} is missing or empty")
            }
            SmtpConfigError::InvalidPort(value) => {
                write!(f, "AMICU_SMTP_PORT is not a valid port: {value}")
            }
            SmtpConfigError::InvalidBool { var, value } => {
                write!(f, "{var} is not a valid boolean: {value}")
            }
            SmtpConfigError::InvalidFrom(value) => {
                write!(f, "AMICU_SMTP_FROM is not a valid mailbox: {value}")
            }
            SmtpConfigError::IncompleteCredentials { present, missing } => write!(
                f,
                "SMTP credentials are all-or-nothing: {present} is set but {missing} is missing"
            ),
            SmtpConfigError::Transport(e) => write!(f, "SMTP transport build failed: {e}"),
        }
    }
}

impl std::error::Error for SmtpConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SmtpConfigError::Transport(e) => Some(e),
            _ => None,
        }
    }
}

impl From<lettre::transport::smtp::Error> for SmtpConfigError {
    fn from(e: lettre::transport::smtp::Error) -> Self {
        SmtpConfigError::Transport(e)
    }
}

/// Read an env var, treating an unset OR empty value as absent. Shared with
/// the bootstrap, which reads `AMICU_INTERNAL_TOKEN` the same way (env-only,
/// never a CLI flag: a secret must not be visible in the process argument list).
pub(crate) fn non_empty_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// Every `AMICU_SMTP_*` variable. Presence of ANY one signals the operator
/// intends to configure SMTP, so the bootstrap must build the real mailer and
/// let an incomplete required set fail loud, never silently fall back to the
/// token-logging dev notifier. `AMICU_WEB_URL` is deliberately excluded: it is
/// not an SMTP-specific signal (it can be set for other reasons), and a
/// complete host+from set already fails loud when it is missing.
const SMTP_ENV_VARS: [&str; 6] = [
    "AMICU_SMTP_HOST",
    "AMICU_SMTP_FROM",
    "AMICU_SMTP_PORT",
    "AMICU_SMTP_USERNAME",
    "AMICU_SMTP_PASSWORD",
    "AMICU_SMTP_STARTTLS",
];

/// Returns `true` when ANY `AMICU_SMTP_*` variable is present and non-empty in
/// the environment. Used by the bootstrap to decide between the real mailer and
/// the dev logger: any SMTP variable means the operator signaled intent to send
/// real email, so an incomplete configuration must be a hard startup error
/// rather than a silent fall back to the token-logging [`LoggingNotifier`].
///
/// [`LoggingNotifier`]: amicu_auth::notify::LoggingNotifier
#[must_use]
pub fn any_smtp_var_set() -> bool {
    any_smtp_var_set_with(non_empty_env)
}

/// [`any_smtp_var_set`] against an arbitrary lookup (test seam).
fn any_smtp_var_set_with<F: Fn(&str) -> Option<String>>(lookup: F) -> bool {
    SMTP_ENV_VARS.iter().any(|var| lookup(var).is_some())
}

/// A [`Notifier`] that sends transactional auth emails over real SMTP.
///
/// Construct it with [`SmtpNotifier::from_env`]. Building the notifier does not
/// open a connection; the transport connects lazily on the first send.
#[derive(Clone)]
pub struct SmtpNotifier {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    /// Public base URL of the web app, trailing slash trimmed.
    web_url: Arc<str>,
}

impl std::fmt::Debug for SmtpNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the transport: its builder holds the SMTP credentials.
        f.debug_struct("SmtpNotifier")
            .field("from", &self.from)
            .field("web_url", &self.web_url)
            .finish_non_exhaustive()
    }
}

impl SmtpNotifier {
    /// Build the notifier from the `AMICU_SMTP_*` / `AMICU_WEB_URL` environment.
    ///
    /// # Errors
    /// Returns [`SmtpConfigError`] if a required variable is missing or a value
    /// (port, STARTTLS flag, from address, host) is invalid. The SMTP password
    /// is never echoed into an error.
    pub fn from_env() -> Result<Self, SmtpConfigError> {
        Self::from_lookup(non_empty_env)
    }

    /// Assemble from an arbitrary configuration lookup.
    ///
    /// `lookup` returns the value for a variable, or `None` when it is unset or
    /// empty. [`from_env`](Self::from_env) passes the process environment; tests
    /// pass an in-memory map so no global state is touched and no network
    /// connection is opened.
    fn from_lookup<F: Fn(&str) -> Option<String>>(lookup: F) -> Result<Self, SmtpConfigError> {
        // Trim host/from/web_url so stray whitespace in an env value (for example
        // AMICU_WEB_URL="https://app.test ") does not survive into a broken link
        // or an unroutable host, matching the trimming AMICU_SMTP_PORT already does.
        let host = lookup("AMICU_SMTP_HOST")
            .ok_or(SmtpConfigError::MissingVar("AMICU_SMTP_HOST"))?
            .trim()
            .to_string();
        let from_raw = lookup("AMICU_SMTP_FROM")
            .ok_or(SmtpConfigError::MissingVar("AMICU_SMTP_FROM"))?
            .trim()
            .to_string();
        let web_url = lookup("AMICU_WEB_URL")
            .ok_or(SmtpConfigError::MissingVar("AMICU_WEB_URL"))?
            .trim()
            .to_string();

        let from: Mailbox = from_raw
            .parse()
            .map_err(|_| SmtpConfigError::InvalidFrom(from_raw.clone()))?;

        let starttls = match lookup("AMICU_SMTP_STARTTLS") {
            Some(v) => parse_bool(&v).ok_or(SmtpConfigError::InvalidBool {
                var: "AMICU_SMTP_STARTTLS",
                value: v,
            })?,
            None => false,
        };

        let port = match lookup("AMICU_SMTP_PORT") {
            Some(v) => v
                .trim()
                .parse::<u16>()
                .map_err(|_| SmtpConfigError::InvalidPort(v))?,
            None if starttls => STARTTLS_PORT,
            None => IMPLICIT_TLS_PORT,
        };

        // Encrypted transport either way: STARTTLS upgrades a plaintext
        // submission connection, otherwise implicit TLS wraps it from the start.
        let mut builder = if starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&host)?
        }
        .port(port);

        // Credentials are optional, but all-or-nothing when present: BYO-SMTP
        // relays may accept unauthenticated submission on a trusted network,
        // while provider relays (Resend) supply a username plus the API key as
        // the password. Exactly one of the pair is a misconfiguration: a lone
        // username would submit an empty password, and a lone password would be
        // silently dropped and submit unauthenticated, so fail loud instead.
        match (lookup("AMICU_SMTP_USERNAME"), lookup("AMICU_SMTP_PASSWORD")) {
            (Some(username), Some(password)) => {
                builder = builder.credentials(Credentials::new(username, password));
            }
            (None, None) => {}
            (Some(_), None) => {
                return Err(SmtpConfigError::IncompleteCredentials {
                    present: "AMICU_SMTP_USERNAME",
                    missing: "AMICU_SMTP_PASSWORD",
                });
            }
            (None, Some(_)) => {
                return Err(SmtpConfigError::IncompleteCredentials {
                    present: "AMICU_SMTP_PASSWORD",
                    missing: "AMICU_SMTP_USERNAME",
                });
            }
        }

        Ok(Self {
            transport: builder.build(),
            from,
            web_url: web_url.trim_end_matches('/').into(),
        })
    }

    /// Build the outgoing [`Message`] for a notification, without sending it.
    ///
    /// Split out from [`Notifier::notify`] so the rendering (subject, body, and
    /// the exact verify/reset link) is unit-testable without a network
    /// connection.
    ///
    /// # Errors
    /// Returns [`AuthError::Backend`] if the recipient is not a valid mailbox,
    /// a token is not valid UTF-8, or the MIME body cannot be assembled. The
    /// error carries no token or address material.
    fn render(&self, message: &Notification) -> Result<Message, AuthError> {
        let to: Mailbox = message.recipient.parse().map_err(|_| AuthError::Backend)?;

        let rendered = match &message.kind {
            NotificationKind::VerifyEmail { verify_token } => {
                let token = verify_token
                    .expose_secret_str()
                    .map_err(|_| AuthError::Backend)?;
                // Tokens are URL_SAFE_NO_PAD base64url, so they need no
                // percent-encoding here; this interpolation relies on that.
                let link = format!("{}/verify?token={token}", self.web_url);
                RenderedEmail {
                    subject: "Confirm your email address",
                    plain: format!(
                        "Confirm your email address to finish setting up your account.\n\n\
                         Open this link to confirm:\n{link}\n\n\
                         If you did not create an account, you can ignore this message.\n"
                    ),
                    html: format!(
                        "<p>Confirm your email address to finish setting up your account.</p>\
                         <p><a href=\"{link}\">Confirm your email address</a></p>\
                         <p>If you did not create an account, you can ignore this message.</p>"
                    ),
                }
            }
            NotificationKind::PasswordReset { reset_token } => {
                let token = reset_token
                    .expose_secret_str()
                    .map_err(|_| AuthError::Backend)?;
                let link = format!("{}/reset?token={token}", self.web_url);
                RenderedEmail {
                    subject: "Reset your password",
                    plain: format!(
                        "We received a request to reset your password.\n\n\
                         Open this link to choose a new password:\n{link}\n\n\
                         If you did not request this, you can ignore this message. \
                         Your password will not change.\n"
                    ),
                    html: format!(
                        "<p>We received a request to reset your password.</p>\
                         <p><a href=\"{link}\">Choose a new password</a></p>\
                         <p>If you did not request this, you can ignore this message. \
                         Your password will not change.</p>"
                    ),
                }
            }
            NotificationKind::AccountExists => {
                let sign_in = format!("{}/login", self.web_url);
                RenderedEmail {
                    subject: "You already have an account",
                    plain: format!(
                        "Someone tried to create an account with this email address, \
                         but one already exists.\n\n\
                         You can sign in here:\n{sign_in}\n\n\
                         If you forgot your password, you can reset it from the sign-in page.\n"
                    ),
                    html: format!(
                        "<p>Someone tried to create an account with this email address, \
                         but one already exists.</p>\
                         <p><a href=\"{sign_in}\">Sign in</a></p>\
                         <p>If you forgot your password, you can reset it from the \
                         sign-in page.</p>"
                    ),
                }
            }
        };

        Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(rendered.subject)
            .multipart(MultiPart::alternative_plain_html(
                rendered.plain,
                rendered.html,
            ))
            .map_err(|_| AuthError::Backend)
    }
}

/// The rendered pieces of one email, before it is assembled into a [`Message`].
struct RenderedEmail {
    subject: &'static str,
    plain: String,
    html: String,
}

#[async_trait]
impl Notifier for SmtpNotifier {
    async fn notify(&self, message: Notification) -> Result<(), AuthError> {
        let recipient = message.recipient.clone();
        let email = self.render(&message)?;
        // On failure log the recipient plus lettre's transport error. That error
        // is connection/auth-level (it distinguishes DNS vs TLS vs auth failures)
        // and carries no message body, so it is safe to log, unlike the opaque
        // token, which lives in the body and must never reach a log, and the SMTP
        // password, which lettre's error never contains.
        self.transport.send(email).await.map_err(|err| {
            eprintln!("[notify] SMTP send failed for {recipient}: {err}");
            AuthError::Backend
        })?;
        Ok(())
    }
}

/// Parse a permissive boolean from a value (`true/false`, `1/0`, `yes/no`,
/// `on/off`), case-insensitively. Returns `None` if unrecognized.
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::secret::SecretValue;
    use std::collections::HashMap;

    /// A configuration lookup backed by a fixed map. Empty values are treated as
    /// absent, matching the real [`non_empty_env`] semantics. The returned
    /// closure owns its data, so it borrows nothing from `pairs`.
    fn map_lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| map.get(key).filter(|v| !v.trim().is_empty()).cloned()
    }

    /// A notifier pointed at a dummy host: enough to render messages. It never
    /// connects, because no test calls `notify`/`send`.
    fn test_notifier() -> SmtpNotifier {
        SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_SMTP_FROM", "Amicu <no-reply@amicu.test>"),
            ("AMICU_WEB_URL", "https://app.example.test/"),
        ]))
        .expect("dummy config should build")
    }

    fn message_string(msg: &Message) -> String {
        String::from_utf8(msg.formatted()).expect("message is valid utf8")
    }

    /// The wire message with quoted-printable transport encoding reversed enough
    /// to assert on the embedded link: soft line breaks removed, then the `=3D`
    /// escape for `=` decoded. lettre QP-encodes a part only when a line is long
    /// enough to need wrapping, so the raw body may or may not be encoded; this
    /// normalizes both cases to the plain URL text.
    fn decoded_message(msg: &Message) -> String {
        message_string(msg)
            .replace("=\r\n", "")
            .replace("=\n", "")
            .replace("=3D", "=")
    }

    #[test]
    fn from_env_errors_when_host_missing() {
        let err = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_FROM", "no-reply@amicu.test"),
            ("AMICU_WEB_URL", "https://app.example.test"),
        ]))
        .expect_err("missing host must fail");
        assert!(matches!(
            err,
            SmtpConfigError::MissingVar("AMICU_SMTP_HOST")
        ));
    }

    #[test]
    fn from_env_errors_when_from_missing() {
        let err = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_WEB_URL", "https://app.example.test"),
        ]))
        .expect_err("missing from must fail");
        assert!(matches!(
            err,
            SmtpConfigError::MissingVar("AMICU_SMTP_FROM")
        ));
    }

    #[test]
    fn from_env_errors_when_web_url_missing() {
        let err = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_SMTP_FROM", "no-reply@amicu.test"),
        ]))
        .expect_err("missing web url must fail");
        assert!(matches!(err, SmtpConfigError::MissingVar("AMICU_WEB_URL")));
    }

    #[test]
    fn from_env_treats_empty_value_as_missing() {
        let err = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "   "),
            ("AMICU_SMTP_FROM", "no-reply@amicu.test"),
            ("AMICU_WEB_URL", "https://app.example.test"),
        ]))
        .expect_err("empty host must fail");
        assert!(matches!(
            err,
            SmtpConfigError::MissingVar("AMICU_SMTP_HOST")
        ));
    }

    #[test]
    fn from_env_rejects_bad_port() {
        let err = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_SMTP_FROM", "no-reply@amicu.test"),
            ("AMICU_WEB_URL", "https://app.example.test"),
            ("AMICU_SMTP_PORT", "not-a-port"),
        ]))
        .expect_err("bad port must fail");
        assert!(matches!(err, SmtpConfigError::InvalidPort(_)));
    }

    #[test]
    fn from_env_rejects_bad_starttls() {
        let err = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_SMTP_FROM", "no-reply@amicu.test"),
            ("AMICU_WEB_URL", "https://app.example.test"),
            ("AMICU_SMTP_STARTTLS", "maybe"),
        ]))
        .expect_err("bad bool must fail");
        assert!(matches!(err, SmtpConfigError::InvalidBool { .. }));
    }

    #[test]
    fn from_env_rejects_bad_from() {
        let err = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_SMTP_FROM", "not a mailbox at all"),
            ("AMICU_WEB_URL", "https://app.example.test"),
        ]))
        .expect_err("bad from must fail");
        assert!(matches!(err, SmtpConfigError::InvalidFrom(_)));
    }

    #[test]
    fn from_env_builds_with_full_config_and_hides_password() {
        let notifier = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.resend.com"),
            ("AMICU_SMTP_FROM", "Amicu <no-reply@amicu.test>"),
            ("AMICU_WEB_URL", "https://app.example.test"),
            ("AMICU_SMTP_PORT", "587"),
            ("AMICU_SMTP_USERNAME", "resend"),
            ("AMICU_SMTP_PASSWORD", "re_secret_key"),
            ("AMICU_SMTP_STARTTLS", "true"),
        ]))
        .expect("full config should build");
        // The password must not surface in Debug output.
        let dbg = format!("{notifier:?}");
        assert!(
            !dbg.contains("re_secret_key"),
            "debug leaked password: {dbg}"
        );
    }

    #[test]
    fn any_smtp_var_set_detects_any_single_var() {
        // No SMTP var at all: the bootstrap uses the dev LoggingNotifier.
        assert!(!any_smtp_var_set_with(map_lookup(&[])));
        // A non-SMTP var (AMICU_WEB_URL) alone does NOT signal SMTP intent.
        assert!(!any_smtp_var_set_with(map_lookup(&[(
            "AMICU_WEB_URL",
            "https://app.example.test"
        )])));
        // Any single AMICU_SMTP_* var signals intent to configure SMTP.
        assert!(any_smtp_var_set_with(map_lookup(&[(
            "AMICU_SMTP_HOST",
            "smtp.example.test"
        )])));
        assert!(any_smtp_var_set_with(map_lookup(&[(
            "AMICU_SMTP_STARTTLS",
            "true"
        )])));
    }

    #[test]
    fn partial_config_host_without_from_is_hard_error_not_logging() {
        // Host is set, so the operator signaled SMTP intent: selection would
        // build the real notifier. The incomplete required set (missing FROM)
        // must fail loud rather than silently fall back to the token-logging
        // LoggingNotifier.
        let lookup = map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_WEB_URL", "https://app.example.test"),
        ]);
        assert!(
            any_smtp_var_set_with(&lookup),
            "host present must select the real mailer path"
        );
        let err = SmtpNotifier::from_lookup(&lookup).expect_err("missing from must fail loud");
        assert!(matches!(
            err,
            SmtpConfigError::MissingVar("AMICU_SMTP_FROM")
        ));
    }

    #[test]
    fn no_smtp_vars_selects_logging_notifier_path() {
        // With zero SMTP vars the selection stays on the LoggingNotifier branch;
        // there is nothing to build, and no hard error is raised.
        assert!(!any_smtp_var_set_with(map_lookup(&[])));
    }

    #[test]
    fn username_without_password_is_error() {
        let err = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_SMTP_FROM", "no-reply@amicu.test"),
            ("AMICU_WEB_URL", "https://app.example.test"),
            ("AMICU_SMTP_USERNAME", "resend"),
        ]))
        .expect_err("username without password must fail");
        assert!(matches!(
            err,
            SmtpConfigError::IncompleteCredentials {
                present: "AMICU_SMTP_USERNAME",
                missing: "AMICU_SMTP_PASSWORD",
            }
        ));
    }

    #[test]
    fn password_without_username_is_error() {
        let err = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_SMTP_FROM", "no-reply@amicu.test"),
            ("AMICU_WEB_URL", "https://app.example.test"),
            ("AMICU_SMTP_PASSWORD", "re_secret_key"),
        ]))
        .expect_err("password without username must fail");
        assert!(matches!(
            err,
            SmtpConfigError::IncompleteCredentials {
                present: "AMICU_SMTP_PASSWORD",
                missing: "AMICU_SMTP_USERNAME",
            }
        ));
    }

    #[test]
    fn trims_whitespace_on_web_url() {
        // Leading/trailing whitespace on AMICU_WEB_URL must not survive into the
        // verify link (it would otherwise break the URL).
        let notifier = SmtpNotifier::from_lookup(map_lookup(&[
            ("AMICU_SMTP_HOST", "smtp.example.test"),
            ("AMICU_SMTP_FROM", "no-reply@amicu.test"),
            ("AMICU_WEB_URL", "  https://app.example.test/  "),
        ]))
        .expect("whitespace-padded config should build");
        let msg = notifier
            .render(&Notification {
                recipient: "user@example.test".to_string(),
                kind: NotificationKind::VerifyEmail {
                    verify_token: SecretValue::new(b"tok".to_vec()),
                },
            })
            .expect("render verify");
        let body = decoded_message(&msg);
        assert!(
            body.contains("https://app.example.test/verify?token=tok"),
            "{body}"
        );
    }

    #[test]
    fn renders_verify_email_with_exact_link() {
        let notifier = test_notifier();
        let msg = notifier
            .render(&Notification {
                recipient: "user@example.test".to_string(),
                kind: NotificationKind::VerifyEmail {
                    verify_token: SecretValue::new(b"verify-token-123".to_vec()),
                },
            })
            .expect("render verify");
        let out = message_string(&msg);
        assert!(out.contains("To: user@example.test"), "{out}");
        assert!(out.contains("From: Amicu <no-reply@amicu.test>"), "{out}");
        assert!(out.contains("Subject: Confirm your email address"), "{out}");
        // The contract link, with the trailing slash on AMICU_WEB_URL trimmed.
        let body = decoded_message(&msg);
        assert!(
            body.contains("https://app.example.test/verify?token=verify-token-123"),
            "{body}"
        );
    }

    #[test]
    fn renders_password_reset_with_reset_link() {
        let notifier = test_notifier();
        let msg = notifier
            .render(&Notification {
                recipient: "user@example.test".to_string(),
                kind: NotificationKind::PasswordReset {
                    reset_token: SecretValue::new(b"reset-token-abc".to_vec()),
                },
            })
            .expect("render reset");
        let out = message_string(&msg);
        assert!(out.contains("Subject: Reset your password"), "{out}");
        let body = decoded_message(&msg);
        assert!(
            body.contains("https://app.example.test/reset?token=reset-token-abc"),
            "{body}"
        );
    }

    #[test]
    fn renders_account_exists_without_token() {
        let notifier = test_notifier();
        let msg = notifier
            .render(&Notification {
                recipient: "owner@example.test".to_string(),
                kind: NotificationKind::AccountExists,
            })
            .expect("render account-exists");
        let out = message_string(&msg);
        assert!(
            out.contains("Subject: You already have an account"),
            "{out}"
        );
        let body = decoded_message(&msg);
        assert!(body.contains("https://app.example.test/login"), "{body}");
        assert!(!body.contains("token="), "{body}");
    }
}
