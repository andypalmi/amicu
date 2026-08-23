//! Transactional-notification delivery seam.
//!
//! A deliberately minimal abstraction over "send one out-of-band message to a
//! user" (a password-reset link, an account-already-exists notice, ...). The
//! auth service depends only on the [`Notifier`] trait; the concrete transport
//! (SMTP, SES, a queue) is a later, separate concern and lives behind this seam.
//!
//! Anti-enumeration relies on delivery being *out-of-band*: whether a message is
//! sent, and which kind, must never be observable on the HTTP response. Callers
//! therefore invoke the notifier only on the account-exists path and always
//! return the same generic response regardless of the outcome here.

use amicu_core::secret::SecretValue;
use async_trait::async_trait;

use crate::error::Result;

/// A single transactional message to deliver to a user.
#[derive(Debug)]
pub struct Notification {
    /// Recipient email (normalized lowercase).
    pub recipient: String,
    /// What to send.
    pub kind: NotificationKind,
}

/// The transactional message kinds the auth flows emit.
///
/// Kept small on purpose; grow it as flows need it. Secret material (a reset
/// token) is carried as a [`SecretValue`] so it never lands in logs or telemetry
/// by accident - only a transport that deliberately exposes it (a real mailer, or
/// the dev logger) ever reads the bytes.
#[derive(Debug)]
pub enum NotificationKind {
    /// A password-reset was requested: carries the opaque reset token to embed
    /// in the reset link. Sent only when the account actually exists.
    PasswordReset {
        /// The opaque reset token (deliver once, over the reset link).
        reset_token: SecretValue,
    },
    /// A new account was registered: carries the opaque verification token to
    /// embed in the "confirm your email" link. Sent only on the account-created
    /// path of sign-up.
    VerifyEmail {
        /// The opaque verify token (deliver once, over the verification link).
        verify_token: SecretValue,
    },
    /// A sign-up was attempted against an already-registered email. Nudges the
    /// real owner toward sign-in / password-reset without ever telling the
    /// *requester* that the account exists (this message is out-of-band).
    AccountExists,
}

/// Out-of-band delivery of transactional messages.
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Deliver one message. Implementations must not block the caller on a slow
    /// transport in a way that would make delivery observable on the triggering
    /// HTTP response (queue if needed).
    ///
    /// # Errors
    /// Transport-specific failures. Callers on anti-enumeration paths treat a
    /// failure as non-fatal and still return their generic response.
    async fn notify(&self, message: Notification) -> Result<()>;
}

/// Dev/test [`Notifier`] that writes messages to stderr instead of sending them.
///
/// No SMTP, no network - enough to exercise the seam locally and in tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoggingNotifier;

#[async_trait]
impl Notifier for LoggingNotifier {
    async fn notify(&self, message: Notification) -> Result<()> {
        match &message.kind {
            NotificationKind::PasswordReset { reset_token } => {
                // DEV ONLY: a real mailer embeds this token in a link mailed to
                // the user; the dev logger prints it so a local developer can
                // complete the flow without an email transport. This is the one
                // place the opaque reset secret is intentionally rendered - never
                // do this in a production Notifier.
                let token = reset_token
                    .expose_secret_str()
                    .unwrap_or("<non-utf8-token>");
                eprintln!(
                    "[notify] password-reset to {}: reset_token={token}",
                    message.recipient
                );
            }
            NotificationKind::VerifyEmail { verify_token } => {
                // DEV ONLY: a real mailer embeds this token in a "confirm your
                // email" link mailed to the user; the dev logger prints it so a
                // local developer can complete the flow without an email
                // transport. This is the one place the opaque verify secret is
                // intentionally rendered; never do this in a production
                // Notifier.
                let token = verify_token
                    .expose_secret_str()
                    .unwrap_or("<non-utf8-token>");
                eprintln!(
                    "[notify] email-verification to {}: verify_token={token}",
                    message.recipient
                );
            }
            NotificationKind::AccountExists => {
                eprintln!(
                    "[notify] account-exists notice to {} (sign in or reset instead)",
                    message.recipient
                );
            }
        }
        Ok(())
    }
}
