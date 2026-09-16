//! Transactional email outbox (andypalmi/amicu-planning#141).
//!
//! # Why an outbox at all
//!
//! Cloud Run throttles a container's CPU to near-zero as soon as the response is
//! written, so the previous design - `tokio::spawn`-ing the SMTP send after the
//! HTTP response returned, to keep response timing independent of whether the
//! account exists - stalls or dies and the email is silently dropped. So the
//! request path now only *persists* the message (a single, constant-cost
//! `INSERT` that can join the caller's own transaction via [`enqueue`]), and the
//! actual SMTP work happens INSIDE a later, authenticated
//! `POST /internal/outbox/drain` request, where CPU is allocated.
//!
//! # Concurrency
//!
//! [`claim_due`] is a single statement: a `SELECT ... FOR UPDATE SKIP LOCKED`
//! sub-query feeding an `UPDATE` that stamps a lease (`locked_until`) and bumps
//! `attempts`. `SKIP LOCKED` excludes two claimers *inside* the statement; the
//! lease excludes them for the much longer SMTP send that follows. Together they
//! make overlapping Cloud Scheduler invocations and multiple Cloud Run replicas
//! safe: no message is ever handed to two senders at once.
//!
//! `attempts` is bumped at CLAIM time rather than at failure time, so a drain
//! that crashes (or is SIGTERM'd) mid-send still burns an attempt - a poison
//! message can never be retried forever.
//!
//! # Retry and dead-lettering
//!
//! [`plan_failure`] is the whole retry state machine as one pure function
//! (exponential backoff, capped, dead-lettered after [`MAX_ATTEMPTS`]), so it is
//! unit-testable with no database.
//!
//! # Tenancy
//!
//! `email_outbox` is GLOBAL and carries no row-level security, exactly like
//! `users` / `reset_tokens` / `verify_tokens` - see migration
//! `0014_email_outbox.sql` for the full rationale.

use amicu_auth::notify::{Notification, NotificationKind, Notifier};
use amicu_auth::{AuthError, Result as AuthResult};
use amicu_core::secret::SecretValue;
use async_trait::async_trait;
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::StoreError;

/// How many delivery attempts a message gets before it is dead-lettered.
///
/// With [`BACKOFF_BASE_SECS`] and [`BACKOFF_MAX_SECS`] below, eight attempts
/// span a little over three hours - long enough to ride out a provider incident
/// or a DNS blip, short enough that a permanently undeliverable address stops
/// costing SMTP connections the same day.
pub const MAX_ATTEMPTS: i32 = 8;

/// First retry delay, doubled on each subsequent failure.
pub const BACKOFF_BASE_SECS: i64 = 30;

/// Ceiling on the exponential backoff, so attempt 8 does not land days out.
pub const BACKOFF_MAX_SECS: i64 = 30 * 60;

/// How long a claimed message stays leased to its claimer before another drain
/// may pick it up. Comfortably longer than any single SMTP send (`lettre`'s own
/// transport timeouts are well under this) but short enough that a replica lost
/// to a scale-down does not strand a message for long.
pub const CLAIM_LEASE_SECS: i64 = 120;

/// `NotificationKind::PasswordReset` on the wire.
const KIND_PASSWORD_RESET: &str = "password_reset";
/// `NotificationKind::VerifyEmail` on the wire.
const KIND_VERIFY_EMAIL: &str = "verify_email";
/// `NotificationKind::AccountExists` on the wire.
const KIND_ACCOUNT_EXISTS: &str = "account_exists";

/// A message about to be queued: the stored form of
/// [`amicu_auth::notify::Notification`].
#[derive(Debug, Clone)]
pub struct NewOutboxEmail {
    /// Recipient email (normalized lowercase).
    pub recipient: String,
    /// Discriminant of [`NotificationKind`] - see [`KIND_PASSWORD_RESET`] and
    /// friends.
    pub kind: &'static str,
    /// The opaque one-shot token the link embeds, or `None` for a kind that
    /// carries none.
    pub token: Option<String>,
}

impl NewOutboxEmail {
    /// Flatten a [`Notification`] into its stored form.
    ///
    /// # Errors
    /// [`AuthError::Backend`] if a token is not valid UTF-8 (it never is not:
    /// every token this codebase mints is base64url).
    pub fn from_notification(message: &Notification) -> AuthResult<Self> {
        let (kind, token) = match &message.kind {
            NotificationKind::PasswordReset { reset_token } => (
                KIND_PASSWORD_RESET,
                Some(
                    reset_token
                        .expose_secret_str()
                        .map_err(|_| AuthError::Backend)?
                        .to_string(),
                ),
            ),
            NotificationKind::VerifyEmail { verify_token } => (
                KIND_VERIFY_EMAIL,
                Some(
                    verify_token
                        .expose_secret_str()
                        .map_err(|_| AuthError::Backend)?
                        .to_string(),
                ),
            ),
            NotificationKind::AccountExists => (KIND_ACCOUNT_EXISTS, None),
        };
        Ok(Self {
            recipient: message.recipient.clone(),
            kind,
            token,
        })
    }
}

/// One row handed to a sender by [`claim_due`].
#[derive(Debug, Clone)]
pub struct ClaimedEmail {
    /// Row id, used to mark the outcome.
    pub id: Uuid,
    /// Recipient email.
    pub recipient: String,
    /// Stored [`NotificationKind`] discriminant.
    pub kind: String,
    /// Stored opaque token, if the kind carries one.
    pub token: Option<String>,
    /// Attempt number this claim represents (1 on the first claim).
    pub attempts: i32,
}

impl ClaimedEmail {
    /// Rebuild the [`Notification`] the mailer seam takes.
    ///
    /// Returns `None` when the stored payload cannot produce one - an unknown
    /// `kind`, or a kind that needs a token with none stored. That is a
    /// data-integrity fault (this crate is the only writer), so the drain
    /// dead-letters such a row immediately rather than retrying it forever.
    #[must_use]
    pub fn to_notification(&self) -> Option<Notification> {
        let kind = match (self.kind.as_str(), self.token.as_deref()) {
            (KIND_PASSWORD_RESET, Some(token)) => NotificationKind::PasswordReset {
                reset_token: SecretValue::new(token.as_bytes().to_vec()),
            },
            (KIND_VERIFY_EMAIL, Some(token)) => NotificationKind::VerifyEmail {
                verify_token: SecretValue::new(token.as_bytes().to_vec()),
            },
            (KIND_ACCOUNT_EXISTS, _) => NotificationKind::AccountExists,
            _ => return None,
        };
        Some(Notification {
            recipient: self.recipient.clone(),
            kind,
        })
    }
}

/// What [`plan_failure`] decided to do with a message whose send just failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureOutcome {
    /// Retry later; the row stays claimable from `next_attempt_at` onwards.
    Retry {
        /// When the message becomes due again (Unix seconds).
        next_attempt_at: i64,
    },
    /// Give up: the row is dead-lettered and never claimed again.
    DeadLetter,
}

/// The retry/dead-letter state machine, as a pure function.
///
/// `attempts` is the attempt number that just failed (1 for the first), as
/// stamped by [`claim_due`]. The backoff doubles from [`BACKOFF_BASE_SECS`] and
/// saturates at [`BACKOFF_MAX_SECS`]; once [`MAX_ATTEMPTS`] attempts have failed
/// the message is dead-lettered.
///
/// Deliberately total: a nonsensical `attempts` (<= 0, or already past the
/// ceiling) still yields a defined answer rather than panicking or overflowing.
#[must_use]
pub fn plan_failure(attempts: i32, now: i64) -> FailureOutcome {
    if attempts >= MAX_ATTEMPTS {
        return FailureOutcome::DeadLetter;
    }
    // attempts is in 1..MAX_ATTEMPTS here, so the shift is bounded and the
    // saturating multiply can never overflow into a negative delay.
    let doublings = u32::try_from(attempts.max(1) - 1).unwrap_or(0).min(30);
    let delay = BACKOFF_BASE_SECS
        .saturating_mul(1_i64 << doublings)
        .min(BACKOFF_MAX_SECS);
    FailureOutcome::Retry {
        next_attempt_at: now.saturating_add(delay),
    }
}

/// Queue one email for delivery, on `executor`.
///
/// Generic over [`PgExecutor`] on purpose: pass `&pool` for a standalone insert,
/// or `&mut *tx` to make the enqueue part of the caller's OWN transaction - so a
/// reset/verify token and the email that delivers it commit together or not at
/// all.
///
/// Returns the new row's id.
///
/// # Errors
/// [`StoreError::Database`] on any driver/query failure.
pub async fn enqueue<'e, E>(
    executor: E,
    message: &NewOutboxEmail,
    now: i64,
) -> Result<Uuid, StoreError>
where
    E: PgExecutor<'e>,
{
    let id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO email_outbox (id, created_at, kind, recipient, token, next_attempt_at) \
         VALUES ($1, $2, $3, $4, $5, $2)",
        id,
        now,
        message.kind,
        message.recipient,
        message.token.as_deref(),
    )
    .execute(executor)
    .await?;
    Ok(id)
}

/// Claim up to `limit` due messages, leasing each for [`CLAIM_LEASE_SECS`].
///
/// One statement, so the `FOR UPDATE SKIP LOCKED` row locks and the lease stamp
/// are applied atomically: two concurrent claimers (two replicas, or two
/// overlapping Scheduler invocations) can never be handed the same row.
///
/// # Errors
/// [`StoreError::Database`] on any driver/query failure.
pub async fn claim_due<'e, E>(
    executor: E,
    now: i64,
    limit: i64,
) -> Result<Vec<ClaimedEmail>, StoreError>
where
    E: PgExecutor<'e>,
{
    let rows = sqlx::query!(
        "WITH due AS ( \
             SELECT id FROM email_outbox \
             WHERE sent_at IS NULL \
               AND dead_at IS NULL \
               AND next_attempt_at <= $1 \
               AND (locked_until IS NULL OR locked_until <= $1) \
             ORDER BY next_attempt_at \
             LIMIT $2 \
             FOR UPDATE SKIP LOCKED \
         ) \
         UPDATE email_outbox o \
         SET locked_until = $1 + $3, attempts = o.attempts + 1 \
         FROM due \
         WHERE o.id = due.id \
         RETURNING o.id, o.recipient, o.kind, o.token, o.attempts",
        now,
        limit,
        CLAIM_LEASE_SECS,
    )
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| ClaimedEmail {
            id: r.id,
            recipient: r.recipient,
            kind: r.kind,
            token: r.token,
            attempts: r.attempts,
        })
        .collect())
}

/// Mark a claimed message delivered.
///
/// Clears `token` in the same statement: the raw one-shot secret exists at rest
/// only while the message is queued, never after it has been handed to SMTP.
///
/// # Errors
/// [`StoreError::Database`] on any driver/query failure.
pub async fn mark_sent<'e, E>(executor: E, id: Uuid, now: i64) -> Result<(), StoreError>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        "UPDATE email_outbox \
         SET sent_at = $2, locked_until = NULL, last_error = NULL, token = NULL \
         WHERE id = $1",
        id,
        now,
    )
    .execute(executor)
    .await?;
    Ok(())
}

/// Apply [`plan_failure`]'s decision for a claimed message whose send failed.
///
/// A dead-lettered row also has its `token` cleared - it will never be
/// delivered, so the secret has no reason to persist.
///
/// `error` is a short, transport-level description. Callers must never pass
/// token or credential material; the drain passes only the mailer's own
/// connection/auth-level error text (see `amicu-server`'s `SmtpNotifier`).
///
/// # Errors
/// [`StoreError::Database`] on any driver/query failure.
pub async fn mark_failed<'e, E>(
    executor: E,
    id: Uuid,
    outcome: FailureOutcome,
    now: i64,
    error: &str,
) -> Result<(), StoreError>
where
    E: PgExecutor<'e>,
{
    match outcome {
        FailureOutcome::Retry { next_attempt_at } => {
            sqlx::query!(
                "UPDATE email_outbox \
                 SET next_attempt_at = $2, locked_until = NULL, last_error = $3 \
                 WHERE id = $1",
                id,
                next_attempt_at,
                error,
            )
            .execute(executor)
            .await?;
        }
        FailureOutcome::DeadLetter => {
            sqlx::query!(
                "UPDATE email_outbox \
                 SET dead_at = $2, locked_until = NULL, last_error = $3, token = NULL \
                 WHERE id = $1",
                id,
                now,
                error,
            )
            .execute(executor)
            .await?;
        }
    }
    Ok(())
}

/// The drain's persistence seam, so `amicu-api` can run a drain pass without
/// taking its own `sqlx` dependency.
#[async_trait]
pub trait OutboxStore: Send + Sync {
    /// See [`claim_due`].
    async fn claim_due(&self, now: i64, limit: i64) -> Result<Vec<ClaimedEmail>, StoreError>;
    /// See [`mark_sent`].
    async fn mark_sent(&self, id: Uuid, now: i64) -> Result<(), StoreError>;
    /// See [`mark_failed`].
    async fn mark_failed(
        &self,
        id: Uuid,
        outcome: FailureOutcome,
        now: i64,
        error: &str,
    ) -> Result<(), StoreError>;
}

/// Postgres implementation of [`OutboxStore`] over the single shared pool.
#[derive(Debug, Clone)]
pub struct PostgresOutboxStore {
    pool: PgPool,
}

impl PostgresOutboxStore {
    /// An outbox store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl OutboxStore for PostgresOutboxStore {
    async fn claim_due(&self, now: i64, limit: i64) -> Result<Vec<ClaimedEmail>, StoreError> {
        claim_due(&self.pool, now, limit).await
    }

    async fn mark_sent(&self, id: Uuid, now: i64) -> Result<(), StoreError> {
        mark_sent(&self.pool, id, now).await
    }

    async fn mark_failed(
        &self,
        id: Uuid,
        outcome: FailureOutcome,
        now: i64,
        error: &str,
    ) -> Result<(), StoreError> {
        mark_failed(&self.pool, id, outcome, now, error).await
    }
}

/// A [`Notifier`] that DURABLY QUEUES instead of sending.
///
/// This is the production wiring of the mailer seam on the Postgres path: the
/// auth flows call `notify` on the request path exactly as before, but the call
/// now costs one constant-time `INSERT` rather than an SMTP round trip, and the
/// message survives the response boundary, the instance being frozen, and the
/// instance being scaled to zero. The real SMTP send happens later, inside
/// `POST /internal/outbox/drain`, against the SAME `Notifier` seam.
///
/// This covers the deliveries that have no token row to be atomic WITH - the
/// "you already have an account" notice on a duplicate sign-up. The two that DO
/// (a reset link, a verification link) go through
/// [`ResetStore::insert_and_notify`](amicu_auth::reset::ResetStore::insert_and_notify)
/// / [`VerifyStore::insert_and_notify`](amicu_auth::verify::VerifyStore::insert_and_notify),
/// whose Postgres implementations write the token row and the queue row in ONE
/// transaction and ignore the notifier entirely.
///
/// The dev / no-database path keeps using
/// [`LoggingNotifier`](amicu_auth::notify::LoggingNotifier) unchanged.
#[derive(Debug, Clone)]
pub struct PostgresOutboxNotifier {
    pool: PgPool,
}

impl PostgresOutboxNotifier {
    /// An enqueueing notifier over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl Notifier for PostgresOutboxNotifier {
    async fn notify(&self, message: Notification) -> AuthResult<()> {
        let queued = NewOutboxEmail::from_notification(&message)?;
        enqueue(&self.pool, &queued, now_unix())
            .await
            .map_err(|_| AuthError::Backend)?;
        Ok(())
    }
}

/// Current Unix time in seconds, saturating at 0 before the epoch.
///
/// The outbox is the only store that stamps "now" itself (every other table
/// takes the caller's already-computed expiry), so this local helper avoids
/// threading a clock through the `Notifier` seam for no behavioural gain.
/// `pub(crate)` so the reset/verify stores can stamp an enqueue they make
/// inside their own token-mint transaction.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_failure_retries_after_the_base_delay() {
        assert_eq!(
            plan_failure(1, 1_000),
            FailureOutcome::Retry {
                next_attempt_at: 1_000 + BACKOFF_BASE_SECS
            }
        );
    }

    #[test]
    fn backoff_doubles_each_attempt_until_the_ceiling() {
        // 30, 60, 120, 240, 480, 960, then capped at 1800.
        let expected = [30, 60, 120, 240, 480, 960, 1_800];
        for (i, want) in expected.iter().enumerate() {
            let attempts = i32::try_from(i).expect("small index") + 1;
            assert_eq!(
                plan_failure(attempts, 0),
                FailureOutcome::Retry {
                    next_attempt_at: *want
                },
                "attempt {attempts}"
            );
        }
    }

    #[test]
    fn backoff_never_exceeds_the_ceiling() {
        for attempts in 1..MAX_ATTEMPTS {
            let FailureOutcome::Retry { next_attempt_at } = plan_failure(attempts, 0) else {
                panic!("attempt {attempts} must still retry");
            };
            assert!(
                next_attempt_at <= BACKOFF_MAX_SECS,
                "attempt {attempts} backed off to {next_attempt_at}"
            );
        }
    }

    #[test]
    fn dead_letters_once_max_attempts_have_failed() {
        assert_eq!(plan_failure(MAX_ATTEMPTS, 0), FailureOutcome::DeadLetter);
        assert_eq!(
            plan_failure(MAX_ATTEMPTS + 1, 0),
            FailureOutcome::DeadLetter
        );
        assert_eq!(plan_failure(i32::MAX, 0), FailureOutcome::DeadLetter);
    }

    /// The attempt *before* the ceiling still retries: the dead-letter boundary
    /// is exactly `MAX_ATTEMPTS`, not one either side of it.
    #[test]
    fn last_attempt_before_the_ceiling_still_retries() {
        assert!(matches!(
            plan_failure(MAX_ATTEMPTS - 1, 0),
            FailureOutcome::Retry { .. }
        ));
    }

    /// A nonsensical attempt count is total, not a panic or an overflow.
    #[test]
    fn non_positive_attempts_are_total() {
        assert_eq!(
            plan_failure(0, 100),
            FailureOutcome::Retry {
                next_attempt_at: 100 + BACKOFF_BASE_SECS
            }
        );
        assert!(matches!(
            plan_failure(-5, 100),
            FailureOutcome::Retry { .. }
        ));
    }

    /// A `now` near `i64::MAX` saturates instead of wrapping into the past
    /// (which would make the message immediately due forever).
    #[test]
    fn far_future_now_saturates() {
        let FailureOutcome::Retry { next_attempt_at } = plan_failure(1, i64::MAX) else {
            panic!("must retry");
        };
        assert_eq!(next_attempt_at, i64::MAX);
    }

    #[test]
    fn notification_round_trips_through_the_stored_form() {
        let message = Notification {
            recipient: "user@example.test".to_string(),
            kind: NotificationKind::PasswordReset {
                reset_token: SecretValue::new(b"tok-123".to_vec()),
            },
        };
        let stored = NewOutboxEmail::from_notification(&message).expect("flatten");
        assert_eq!(stored.kind, KIND_PASSWORD_RESET);
        assert_eq!(stored.token.as_deref(), Some("tok-123"));

        let claimed = ClaimedEmail {
            id: Uuid::nil(),
            recipient: stored.recipient,
            kind: stored.kind.to_string(),
            token: stored.token,
            attempts: 1,
        };
        let rebuilt = claimed.to_notification().expect("rebuild");
        assert_eq!(rebuilt.recipient, "user@example.test");
        match rebuilt.kind {
            NotificationKind::PasswordReset { reset_token } => {
                assert_eq!(reset_token.expose_secret_str().unwrap(), "tok-123");
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn account_exists_round_trips_without_a_token() {
        let stored = NewOutboxEmail::from_notification(&Notification {
            recipient: "owner@example.test".to_string(),
            kind: NotificationKind::AccountExists,
        })
        .expect("flatten");
        assert_eq!(stored.kind, KIND_ACCOUNT_EXISTS);
        assert!(stored.token.is_none());
    }

    /// An unrenderable stored payload (unknown kind, or a token-bearing kind
    /// with no token) yields `None` so the drain can dead-letter it instead of
    /// retrying a row it can never send.
    #[test]
    fn unrenderable_payloads_yield_none() {
        let unknown = ClaimedEmail {
            id: Uuid::nil(),
            recipient: "user@example.test".to_string(),
            kind: "not_a_kind".to_string(),
            token: None,
            attempts: 1,
        };
        assert!(unknown.to_notification().is_none());

        let missing_token = ClaimedEmail {
            kind: KIND_VERIFY_EMAIL.to_string(),
            ..unknown
        };
        assert!(missing_token.to_notification().is_none());
    }
}
