//! Auth service orchestration: sign-up, sign-in, refresh rotation, logout, and
//! JWKS/discovery publication.

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use amicu_core::secret::SecretValue;

use crate::claims::Tier;
use crate::error::{AuthError, Result};
use crate::issuer::{Clock, SystemClock, TokenIssuer};
use crate::jwks::{Jwks, OpenIdConfiguration};
use crate::key::SigningKey;
use crate::keyring::Keyring;
use crate::membership::{MembershipStore, PersonalMembershipStore, WorkspaceMembership};
use crate::notify::{Notification, NotificationKind, Notifier};
use crate::password::{MAX_PASSWORD_LEN, check_password_policy, hash_password, verify_password};
use crate::refresh::{self, RefreshStore};
use crate::reset::{self, ResetStore};
use crate::security::{SecurityEvent, Severity};
use crate::tenant::WorkspaceId;
use crate::throttle::{Admission, InMemoryLoginThrottle, LoginThrottle};
use crate::user::{User, UserStore};
use crate::verify::{self, VerifyStore};

/// Minimum wall-clock duration of the two account-lookup flows that would
/// otherwise be an account-enumeration timing oracle: `/recover`
/// ([`AuthService::request_password_reset`]) and `/verify/resend`
/// ([`AuthService::resend_verification`]).
///
/// # Why a floor replaced `tokio::spawn`
///
/// These flows used to detach ALL account-dependent work (lookup, token mint,
/// store insert, out-of-band send) onto a background task, so the response
/// returned in account-independent time. On Cloud Run that is fatal: CPU is
/// throttled to near-zero the moment the response is written, so the detached
/// task stalls or dies and the email is silently dropped
/// (andypalmi/amicu-planning#141). The work therefore moved back ON to the
/// request path - where CPU is allocated - and is now durable (it ends in an
/// `email_outbox` row, not an SMTP round trip).
///
/// That reintroduces the timing oracle the detach existed to close: the
/// account-EXISTS branch does two extra `INSERT`s that the MISS branch does not.
/// This floor closes it again by construction: BOTH branches sleep until the
/// same fixed deadline measured from entry, so the observable response time is a
/// constant, independent of everything the handler did or did not do.
///
/// 250ms is chosen to sit comfortably above the exists branch's actual cost (a
/// lookup plus one two-statement transaction - single-digit milliseconds against
/// a pooled Postgres, even through a connection pooler), so the floor binds and
/// the padding, not the work, is what the caller measures. Both flows are
/// human-initiated, rare, and already rate-limited, so a fixed quarter-second is
/// free in practice.
///
/// The residual gap: if the exists branch ever takes LONGER than the floor
/// (a database in serious trouble), the floor stops binding and the difference
/// becomes observable again - but a database that slow degrades the lookup both
/// branches share too, so the signal is buried in noise rather than clean.
const ANTI_ENUMERATION_FLOOR: Duration = Duration::from_millis(250);

/// Budget for one throttle round trip. The throttle FAILS OPEN on error or
/// timeout (see [`AuthService::admit_sign_in`]).
const THROTTLE_TIMEOUT: Duration = Duration::from_millis(1_000);

/// Result of a sign-up attempt, reported truthfully to the (trusted) caller.
///
/// The HTTP boundary MUST collapse both variants to one identical response so a
/// client cannot tell a new registration from a hit on an existing account
/// (email enumeration). The distinction is retained here only for internal use:
/// audit metrics today, and - once the email pipeline lands - deciding which
/// message to dispatch (a verification link vs. an "account already exists"
/// notice), both delivered out-of-band so neither is observable on the HTTP
/// response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignUpOutcome {
    /// A new account was created; the id is for internal use only and MUST NOT
    /// be echoed to the client.
    Created(String),
    /// The email was already registered; no new account was created.
    AlreadyRegistered,
}

/// Tokens returned to a client on sign-in/refresh.
pub struct TokenPair {
    /// Short-lived access JWT (verify offline via JWKS).
    pub access_token: String,
    /// Opaque, rotating refresh token (give to the client once).
    pub refresh_token: SecretValue,
    /// Always `Bearer`.
    pub token_type: &'static str,
    /// Access-token lifetime, seconds.
    pub expires_in: i64,
}

/// Redacted: bearer tokens are secrets and are never rendered.
impl std::fmt::Debug for TokenPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenPair")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// The issuer service.
pub struct AuthService<C: Clock = SystemClock> {
    keyring: Arc<RwLock<Keyring>>,
    issuer: TokenIssuer<C>,
    users: Arc<dyn UserStore>,
    refresh: Arc<dyn RefreshStore>,
    resets: Arc<dyn ResetStore>,
    verifies: Arc<dyn VerifyStore>,
    /// The workspace-membership authority: resolves `wsrole`/`tier` at mint time
    /// and re-validates membership on refresh rotation and workspace switch.
    /// Defaults to a [`PersonalMembershipStore`] over the user store (the
    /// zero-config dev/in-memory path); the Postgres deployment installs the
    /// real multi-workspace store via
    /// [`with_membership_store`](AuthService::with_membership_store).
    memberships: Arc<dyn MembershipStore>,
    notifier: Arc<dyn Notifier>,
    throttle: Arc<dyn LoginThrottle>,
    /// Process-local backoff used when `throttle` errors or times out, so a
    /// saturated pool degrades to per-replica protection, never to none.
    fallback_throttle: InMemoryLoginThrottle,
    base_url: String,
}

impl<C: Clock> AuthService<C> {
    /// Assemble the service.
    ///
    /// Takes each persistence seam and the notifier explicitly; the argument
    /// count is inherent to wiring the stores, not a design smell.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        keyring: Keyring,
        issuer: TokenIssuer<C>,
        users: Arc<dyn UserStore>,
        refresh: Arc<dyn RefreshStore>,
        resets: Arc<dyn ResetStore>,
        verifies: Arc<dyn VerifyStore>,
        notifier: Arc<dyn Notifier>,
        base_url: impl Into<String>,
    ) -> Self {
        // Default membership authority: each user's single personal workspace,
        // derived from the user store. The Postgres deployment overrides this
        // with the real multi-workspace store via `with_membership_store`.
        let memberships: Arc<dyn MembershipStore> =
            Arc::new(PersonalMembershipStore::new(users.clone()));
        Self {
            keyring: Arc::new(RwLock::new(keyring)),
            issuer,
            users,
            refresh,
            resets,
            verifies,
            memberships,
            notifier,
            throttle: Arc::new(InMemoryLoginThrottle::default()),
            fallback_throttle: InMemoryLoginThrottle::default(),
            base_url: base_url.into(),
        }
    }

    /// Install the workspace-membership authority (the real multi-workspace
    /// store on the Postgres path). Without this, the service uses a
    /// [`PersonalMembershipStore`] that knows only each user's personal
    /// workspace - correct for zero-config dev, but it cannot switch to shared
    /// workspaces or reflect invites/removals.
    #[must_use]
    pub fn with_membership_store(mut self, memberships: Arc<dyn MembershipStore>) -> Self {
        self.memberships = memberships;
        self
    }

    /// Replace the per-account sign-in backoff store (default: process-local
    /// [`InMemoryLoginThrottle`]). Multi-replica deployments install a shared,
    /// Postgres-backed one.
    #[must_use]
    pub fn with_login_throttle(mut self, throttle: Arc<dyn LoginThrottle>) -> Self {
        self.throttle = throttle;
        self
    }

    /// Register a new user.
    ///
    /// Anti-enumeration: this never signals whether the email was already
    /// taken. An `AlreadyRegistered` collision is reported as its own
    /// [`SignUpOutcome`] variant (not an error), and the same password hashing
    /// runs on both paths so neither the result *nor the timing* distinguishes
    /// a new account from an existing one. The caller MUST render both outcomes
    /// identically (see [`SignUpOutcome`]) - mirroring the sign-in
    /// dummy-hash defense.
    ///
    /// Input validation that concerns the *request itself* (empty email, weak
    /// password) is still reported as an error: it reveals nothing about which
    /// accounts exist.
    ///
    /// # Errors
    /// [`AuthError::InvalidCredentials`] (empty email or password),
    /// [`AuthError::WeakPassword`] (fails the password policy), or
    /// [`AuthError::PasswordHash`].
    pub async fn sign_up(&self, email: &str, password: &SecretValue) -> Result<SignUpOutcome> {
        let email = normalize_email(email);
        if email.is_empty() {
            return Err(AuthError::InvalidCredentials);
        }
        check_password_policy(password)?;
        // Always hash: on the collision path this is the timing equalizer, so a
        // duplicate sign-up costs the same Argon2 work as a fresh one.
        let password_hash = hash_password(password)?;
        let id = new_user_id();
        match self
            .users
            .create(User::new(
                id.clone(),
                email.clone(),
                password_hash,
                Tier::Community,
            ))
            .await
        {
            Ok(()) => {
                // Queue the verification link DURABLY, inline (see
                // `queue_email_verification`): the mint + store-insert + enqueue
                // run on the request path, where Cloud Run allocates CPU, and the
                // actual SMTP send happens later in the outbox drain. The
                // unconditional Argon2id hash above is what equalizes Created vs
                // AlreadyRegistered timing; this extra insert is noise beside it.
                self.queue_email_verification(&id, email).await;
                Ok(SignUpOutcome::Created(id))
            }
            // Swallow the collision: surfacing it would enable enumeration.
            //
            // Instead notify the *real* owner out-of-band via the notification
            // seam (amicu-planning#102): an "account already exists / reset your
            // password" nudge that the requester never observes on the identical
            // HTTP response. The Created path (above) queues a verification link
            // over the same seam; both outcomes stay identical on the HTTP
            // response and in timing.
            Err(AuthError::UserExists) => {
                // Queue the notice DURABLY, inline: one `email_outbox` insert
                // against the Created path's two, both dwarfed by the
                // unconditional Argon2id hash above (see
                // `queue_email_verification`'s docs). Best-effort: a queueing
                // failure must not change anything observable.
                let _ = self
                    .notifier
                    .notify(Notification {
                        recipient: email,
                        kind: NotificationKind::AccountExists,
                    })
                    .await;
                Ok(SignUpOutcome::AlreadyRegistered)
            }
            Err(other) => Err(other),
        }
    }

    /// Authenticate with email+password and issue a token pair.
    ///
    /// Email-verification gate: when the `AMICU_REQUIRE_EMAIL_VERIFICATION`
    /// environment gate is enabled, an account whose email is not yet verified is
    /// rejected with [`AuthError::EmailUnverified`], but only AFTER the password
    /// has verified. A wrong password therefore still returns the generic
    /// [`AuthError::InvalidCredentials`], so the distinct `EmailUnverified` is
    /// only ever observable by someone who already holds the correct password
    /// (no new enumeration surface). The gate defaults OFF: no SMTP sender ships
    /// in this PR.
    ///
    /// # Errors
    /// [`AuthError::InvalidCredentials`] on unknown user or wrong password;
    /// [`AuthError::EmailUnverified`] when the gate is on and the account's email
    /// is unverified.
    pub async fn sign_in(&self, email: &str, password: &SecretValue) -> Result<TokenPair> {
        self.sign_in_from(email, password, None).await
    }

    /// [`Self::sign_in`] with the caller's network (an opaque pseudonym of the
    /// client's IPv4 /24 or IPv6 /64, `None` when there is no trusted client
    /// IP). The hard backoff is keyed on email AND network, so an attacker on a
    /// different network cannot lock the real owner out; the owner's own
    /// network (or an unattributed one) shares the counter.
    ///
    /// # Errors
    /// As [`Self::sign_in`], plus [`AuthError::Throttled`] while locked.
    pub async fn sign_in_from(
        &self,
        email: &str,
        password: &SecretValue,
        client_net: Option<&str>,
    ) -> Result<TokenPair> {
        let email = normalize_email(email);
        // Bound the Argon2 input. Rejected before any lookup, so it is the same
        // generic failure for every email.
        if password.expose_secret().len() > MAX_PASSWORD_LEN {
            return Err(AuthError::InvalidCredentials);
        }
        // Per-account backoff, keyed by the email pseudonym whether or not the
        // account exists (anti-enumeration). While locked, spend one dummy
        // Argon2 verify so the response time does not reveal the lock state.
        let throttle_key = throttle_key(&email, client_net);
        if let Admission::Locked { retry_after_secs } = self.admit_sign_in(&throttle_key).await {
            let _ = verify_password(password, dummy_hash());
            return Err(AuthError::Throttled { retry_after_secs });
        }
        let user = self.users.get_by_email(&email).await?;
        let Some(user) = user else {
            // Equalize timing against a dummy hash to blunt user enumeration.
            let _ = verify_password(password, dummy_hash());
            return Err(AuthError::InvalidCredentials);
        };
        verify_password(password, &user.password_hash)?;
        // Correct password: forget the failures (best-effort).
        if let Err(_e) = self.throttle.clear(&throttle_key).await {
            SecurityEvent::new("login_throttle_unavailable", Severity::Warn)
                .reason("clear_failed")
                .emit();
        }
        // Gate AFTER the password check: a wrong password can never reach here,
        // so the unverified-email signal is only ever shown to the true owner.
        if require_email_verification() && !user.email_verified {
            return Err(AuthError::EmailUnverified);
        }
        // Fresh sign-in defaults to the user's personal workspace (their Owner
        // home). A multi-workspace user switches afterwards via
        // `switch_workspace`. Membership/role/tier are resolved server-side
        // inside `issue_pair_for_workspace`.
        self.issue_pair_for_workspace(&user, user.personal_workspace_id)
            .await
    }

    /// Count this attempt against the account's backoff. On a throttle-store
    /// error or timeout it does NOT fail open to "no protection": it emits a
    /// `login_throttle_unavailable` event and falls back to this replica's
    /// process-local throttle (per-replica, not shared, but never none). That
    /// matters because a saturated pool makes every Postgres-backed limiter
    /// time out together while the user lookup may still wait longer.
    async fn admit_sign_in(&self, key: &str) -> Admission {
        let reason =
            match tokio::time::timeout(THROTTLE_TIMEOUT, self.throttle.begin_attempt(key)).await {
                Ok(Ok(admission)) => return admission,
                Ok(Err(_)) => "error",
                Err(_) => "timeout",
            };
        SecurityEvent::new("login_throttle_unavailable", Severity::Warn)
            .reason(reason)
            .emit();
        self.fallback_throttle
            .begin_attempt(key)
            .await
            .unwrap_or(Admission::Allowed)
    }

    /// Rotate a refresh token: validate it, revoke it, and issue a fresh pair
    /// within the same token family.
    ///
    /// Reuse/theft detection: presenting a token that is *already revoked*
    /// (typically an old token replayed after the family has rotated past it)
    /// is treated as a compromise - the entire family is revoked and flagged,
    /// killing both the attacker's rotated token and the victim's live one and
    /// forcing a fresh sign-in. See the [`refresh`] module docs.
    ///
    /// # Errors
    /// [`AuthError::RefreshInvalid`] if unknown, expired, or revoked (the reuse
    /// case additionally revokes the family before returning this error).
    pub async fn refresh(&self, presented: &SecretValue) -> Result<TokenPair> {
        let token = presented
            .expose_secret_str()
            .map_err(|_| AuthError::RefreshInvalid)?;
        let hash = refresh::hash_token(token);
        // Atomically compare-and-revoke: the liveness check and the revoke are a
        // single indivisible step in the store, so two concurrent refreshes of
        // the same token can never both proceed to mint (which would leave two
        // live successors in one family and silently defeat reuse detection).
        // Exactly one caller wins `Consumed`; the loser sees `Reuse`.
        match self.refresh.consume(&hash, self.issuer.now_unix()).await? {
            refresh::RefreshConsume::NotFound | refresh::RefreshConsume::Expired => {
                Err(AuthError::RefreshInvalid)
            }
            // Reuse/theft: an already-revoked (or already-compromised) token was
            // presented. Revoke and flag the whole family, then reject.
            refresh::RefreshConsume::Reuse { family_id } => {
                self.refresh.compromise_family(&family_id).await?;
                // `family_id` is an opaque server-generated id, not a token.
                SecurityEvent::new("refresh_reuse_detected", Severity::Warn)
                    .reason("family_revoked")
                    .emit();
                Err(AuthError::RefreshInvalid)
            }
            // This caller won the rotation; re-validate membership in the SAME
            // workspace the token was minted for, then mint the successor into
            // the family. Removed member -> the mint refuses (Forbidden), which
            // we surface as a failed refresh: the old token is already consumed
            // and nothing new is minted, so the session dies within the access
            // TTL. Role/tier change -> the successor carries the fresh values
            // (`issue_pair_in_family_for_workspace` re-reads them server-side).
            refresh::RefreshConsume::Consumed {
                user_id,
                workspace_id,
                family_id,
            } => {
                let user = self
                    .users
                    .get_by_id(&user_id)
                    .await?
                    .ok_or(AuthError::RefreshInvalid)?;
                self.issue_pair_in_family_for_workspace(&user, workspace_id, &family_id)
                    .await
                    .map_err(|e| match e {
                        // No longer a member of wsid: the refresh fails outright.
                        AuthError::Forbidden => AuthError::RefreshInvalid,
                        other => other,
                    })
            }
        }
    }

    /// Revoke a refresh token (logout). Idempotent.
    ///
    /// # Errors
    /// Propagates store errors only.
    pub async fn logout(&self, presented: &SecretValue) -> Result<()> {
        if let Ok(token) = presented.expose_secret_str() {
            self.refresh.revoke(&refresh::hash_token(token)).await?;
        }
        Ok(())
    }

    /// Request a password reset for `email`.
    ///
    /// Anti-enumeration: the HTTP boundary always returns the same generic
    /// `202`, whether or not the email is registered, and this call always takes
    /// the same wall-clock time - see [`ANTI_ENUMERATION_FLOOR`] for exactly how
    /// and why (it replaced a `tokio::spawn` that Cloud Run's post-response CPU
    /// throttling made unreliable, andypalmi/amicu-planning#141).
    ///
    /// All of the account-dependent work now runs INLINE, on the request path,
    /// where CPU is guaranteed: the lookup, and - only when the account exists -
    /// the token mint plus a single transaction that writes the reset-token row
    /// and durably queues the email TOGETHER (see
    /// [`ResetStore::insert_and_notify`](crate::reset::ResetStore::insert_and_notify)).
    /// The (possibly network-bound) SMTP send happens later, inside the
    /// authenticated `POST /internal/outbox/drain` request. On a miss nothing is
    /// minted, stored, or queued.
    ///
    /// # Errors
    /// Never returns an error: backend failures are swallowed (surfacing them
    /// would itself be an existence signal). Kept `Result` for a uniform handler
    /// signature.
    pub async fn request_password_reset(&self, email: &str) -> Result<()> {
        // Take the deadline FIRST, before any branch: every path below returns
        // at exactly this instant regardless of what it did.
        let deadline = tokio::time::Instant::now() + ANTI_ENUMERATION_FLOOR;
        let email = normalize_email(email);
        // An empty email is a request-level no-op (not an account signal), but it
        // still waits out the floor so it is not distinguishable either.
        if !email.is_empty() {
            self.deliver_password_reset(&email).await;
        }
        tokio::time::sleep_until(deadline).await;
        Ok(())
    }

    /// Mint, persist, and durably queue a password-reset link for `email`, if
    /// that account exists. Every failure is swallowed: on this flow a backend
    /// error must change nothing observable (the caller pads to a fixed
    /// deadline regardless).
    async fn deliver_password_reset(&self, email: &str) {
        let Ok(Some(user)) = self.users.get_by_email(email).await else {
            return; // unknown account: mint nothing, store nothing, queue nothing
        };
        let expires_at = self.issuer.now_unix()
            + i64::try_from(self.issuer.config().reset_ttl.as_secs()).unwrap_or(i64::MAX);
        let minted = reset::mint(&user.id, expires_at);
        // One transaction on the durable path: the token row and the queued
        // email commit together or not at all.
        let _ = self
            .resets
            .insert_and_notify(
                minted.record,
                Notification {
                    recipient: email.to_string(),
                    kind: NotificationKind::PasswordReset {
                        reset_token: minted.token,
                    },
                },
                self.notifier.as_ref(),
            )
            .await;
    }

    /// Resend the email-verification link for `email`.
    ///
    /// Anti-enumeration: the HTTP boundary always returns the same generic
    /// `202`, whether or not the email is registered and whether or not it is
    /// already verified, and this call always takes the same wall-clock time -
    /// exactly as in
    /// [`request_password_reset`](Self::request_password_reset); see
    /// [`ANTI_ENUMERATION_FLOOR`].
    ///
    /// A fresh verification token is minted, persisted, and queued only when the
    /// account exists AND its email is not yet verified; otherwise nothing is
    /// stored and nothing is queued.
    ///
    /// # Errors
    /// Never returns an error: backend failures are swallowed (surfacing them
    /// would itself be an existence signal). Kept `Result` for a uniform handler
    /// signature.
    pub async fn resend_verification(&self, email: &str) -> Result<()> {
        let deadline = tokio::time::Instant::now() + ANTI_ENUMERATION_FLOOR;
        let email = normalize_email(email);
        if !email.is_empty() {
            self.deliver_verification_resend(&email).await;
        }
        tokio::time::sleep_until(deadline).await;
        Ok(())
    }

    /// Mint, persist, and durably queue a fresh verification link for `email`,
    /// if that account exists and is still unverified. Failures are swallowed
    /// for the same reason as [`Self::deliver_password_reset`].
    async fn deliver_verification_resend(&self, email: &str) {
        let Ok(Some(user)) = self.users.get_by_email(email).await else {
            return; // unknown account: mint nothing, store nothing, queue nothing
        };
        if user.email_verified {
            return; // already verified: a fresh link is neither needed nor sent
        }
        let expires_at = self.issuer.now_unix()
            + i64::try_from(self.issuer.config().verify_ttl.as_secs()).unwrap_or(i64::MAX);
        let minted = verify::mint(&user.id, expires_at);
        let _ = self
            .verifies
            .insert_and_notify(
                minted.record,
                Notification {
                    recipient: email.to_string(),
                    kind: NotificationKind::VerifyEmail {
                        verify_token: minted.token,
                    },
                },
                self.notifier.as_ref(),
            )
            .await;
    }

    /// Consume a reset token and set a new password.
    ///
    /// The password policy is enforced first (a request-level check that reveals
    /// nothing about which tokens/accounts exist, and avoids burning a valid
    /// token on a weak password). The token is then consumed atomically and
    /// single-use: a reused, expired, or unknown token is rejected with the
    /// generic [`AuthError::ResetInvalid`] (no enumeration). On success the
    /// password hash is replaced AND every refresh-token family the user holds is
    /// revoked, forcing a fresh sign-in on every device.
    ///
    /// # Errors
    /// [`AuthError::InvalidCredentials`] / [`AuthError::WeakPassword`] if the new
    /// password fails policy; [`AuthError::ResetInvalid`] if the token is unknown,
    /// expired, or already used; [`AuthError::PasswordHash`] on hashing failure.
    pub async fn reset_password(
        &self,
        token: &SecretValue,
        new_password: &SecretValue,
    ) -> Result<()> {
        // Request-level validation before touching the token: identical error
        // regardless of token validity, so it leaks nothing.
        check_password_policy(new_password)?;
        let raw = token
            .expose_secret_str()
            .map_err(|_| AuthError::ResetInvalid)?;
        let hash = reset::hash_token(raw);
        // Atomic single-use consume: exactly one caller can win `Consumed`.
        let user_id = match self.resets.consume(&hash, self.issuer.now_unix()).await? {
            reset::ResetConsume::Consumed { user_id } => user_id,
            reset::ResetConsume::NotFound
            | reset::ResetConsume::Reuse
            | reset::ResetConsume::Expired => return Err(AuthError::ResetInvalid),
        };
        // Hash the new password only after the token is proven live and claimed.
        let password_hash = hash_password(new_password)?;
        // Revoke sessions BEFORE installing the new password. These two writes
        // are not atomic here (they would be one transaction in a persistent
        // store), so order them for fail-safety: if revocation fails, the token
        // is already consumed but the password is unchanged and sessions survive
        // - the user simply requests another reset. The reverse order would leave
        // the dangerous state (new password live, but old sessions still valid)
        // on a partial failure.
        self.refresh.revoke_all_for_user(&user_id).await?;
        self.users.update_password(&user_id, password_hash).await?;
        SecurityEvent::new("password_changed", Severity::Info)
            .outcome("ok")
            .reason("reset")
            .user(&user_id)
            .emit();
        Ok(())
    }

    /// Consume an email-verification token and mark the account's email verified.
    ///
    /// The token is consumed atomically and single-use: a reused, expired, or
    /// unknown token is rejected with the generic [`AuthError::VerifyInvalid`]
    /// (no enumeration: every rejected token looks identical). On success the
    /// owning account's `email_verified` flag is set; re-verifying an already
    /// verified account with a fresh live token is harmless (the flag set is
    /// idempotent), but a *replayed* token is still rejected.
    ///
    /// # Errors
    /// [`AuthError::VerifyInvalid`] if the token is unknown, expired, or already
    /// used; propagates store errors from marking the account verified.
    pub async fn verify_email(&self, token: &SecretValue) -> Result<()> {
        let raw = token
            .expose_secret_str()
            .map_err(|_| AuthError::VerifyInvalid)?;
        let hash = verify::hash_token(raw);
        // Atomic single-use consume: exactly one caller can win `Consumed`.
        let user_id = match self.verifies.consume(&hash, self.issuer.now_unix()).await? {
            verify::VerifyConsume::Consumed { user_id } => user_id,
            verify::VerifyConsume::NotFound
            | verify::VerifyConsume::Reuse
            | verify::VerifyConsume::Expired => return Err(AuthError::VerifyInvalid),
        };
        self.users.set_email_verified(&user_id).await?;
        SecurityEvent::new("email_verified", Severity::Info)
            .outcome("ok")
            .user(&user_id)
            .emit();
        Ok(())
    }

    /// Introduce a new active signing key; the previous key keeps verifying.
    pub fn rotate_keys(&self, new_active: SigningKey) {
        self.keyring
            .write()
            .expect("keyring poisoned")
            .rotate_in(new_active);
    }

    /// The public JWKS for `/.well-known/jwks.json`.
    #[must_use]
    pub fn jwks(&self) -> Jwks {
        self.keyring.read().expect("keyring poisoned").jwks()
    }

    /// A shared handle to this service's keyring (`Arc<RwLock<Keyring>>`).
    /// Wire this into [`crate::verifier::KeyringJwksProvider`] so an
    /// in-process verifier (e.g. the engine gate) observes
    /// [`Self::rotate_keys`] with NO restart and NO network round-trip —
    /// because it shares the exact same lock this service mutates. Not
    /// instant, though: the verifier's own cache still gates how fast a
    /// change is actually picked up (see [`crate::verifier::KeyringJwksProvider`]'s
    /// docs for the bound). Distinct from [`Self::jwks`], which returns a
    /// point-in-time snapshot.
    #[must_use]
    pub fn keyring_handle(&self) -> Arc<RwLock<Keyring>> {
        self.keyring.clone()
    }

    /// The discovery document for `/.well-known/openid-configuration`.
    #[must_use]
    pub fn openid_configuration(&self) -> OpenIdConfiguration {
        OpenIdConfiguration::new(self.issuer.config().issuer.clone(), &self.base_url)
    }

    /// Mint + persist an email-verification token and queue its link, inline.
    ///
    /// Called only on the sign-up account-created path. This used to be a
    /// detached `tokio::spawn`, which Cloud Run's post-response CPU throttling
    /// makes unreliable (andypalmi/amicu-planning#141); it now runs on the
    /// request path and ends in a durable `email_outbox` row rather than an SMTP
    /// round trip, with the token row and the queued email committed in ONE
    /// transaction on the Postgres path (see
    /// [`VerifyStore::insert_and_notify`](crate::verify::VerifyStore::insert_and_notify)).
    ///
    /// Anti-enumeration is preserved WITHOUT a timing floor here, unlike
    /// `/recover` and `/verify/resend`: `sign_up` unconditionally runs one full
    /// Argon2id hash on BOTH the Created and the AlreadyRegistered path (see
    /// [`Self::sign_up`]), which is deliberately expensive - tens to hundreds of
    /// milliseconds - and dwarfs the one extra `INSERT` (the verify-token row)
    /// that separates the two paths here. The existing dummy-hash equalizer is
    /// what carries the property; this queueing work is noise beside it.
    ///
    /// Store and delivery failures are swallowed (best-effort), exactly as
    /// `request_password_reset` treats them.
    async fn queue_email_verification(&self, user_id: &str, email: String) {
        let expires_at = self.issuer.now_unix()
            + i64::try_from(self.issuer.config().verify_ttl.as_secs()).unwrap_or(i64::MAX);
        let minted = verify::mint(user_id, expires_at);
        let _ = self
            .verifies
            .insert_and_notify(
                minted.record,
                Notification {
                    recipient: email,
                    kind: NotificationKind::VerifyEmail {
                        verify_token: minted.token,
                    },
                },
                self.notifier.as_ref(),
            )
            .await;
    }

    /// List the workspaces the authenticated user belongs to (id, name, role).
    ///
    /// Scoped to the SUBJECT's own memberships: a caller sees only their own.
    /// Backs `GET /workspaces`.
    ///
    /// # Errors
    /// Propagates store errors.
    pub async fn list_workspaces(&self, user_id: &str) -> Result<Vec<WorkspaceMembership>> {
        self.memberships.list_for_user(user_id).await
    }

    /// Switch the active workspace: mint a fresh token pair scoped to `target`,
    /// opening a NEW refresh family - but ONLY IF the subject is a member of
    /// `target` (re-checked server-side against `workspace_members`).
    ///
    /// The client never asserts a workspace it isn't a member of: membership is
    /// resolved here, from the store of record, using only the verified `sub`
    /// and the requested `target`.
    ///
    /// # Errors
    /// [`AuthError::Forbidden`] if the subject is not a member of `target` (the
    /// API maps this to `403`, and no token is minted); propagates store errors.
    pub async fn switch_workspace(&self, user_id: &str, target: WorkspaceId) -> Result<TokenPair> {
        // The subject comes from a verified token, so the user exists; a missing
        // row here is treated as "cannot switch" (no token), not a 500.
        let user = self
            .users
            .get_by_id(user_id)
            .await?
            .ok_or(AuthError::Forbidden)?;
        self.issue_pair_for_workspace(&user, target).await
    }

    /// Issue a token pair scoped to `wsid`, opening a brand-new refresh family
    /// (fresh sign-in / workspace switch). Membership in `wsid` is re-checked
    /// server-side; a non-member is refused with [`AuthError::Forbidden`].
    async fn issue_pair_for_workspace(&self, user: &User, wsid: WorkspaceId) -> Result<TokenPair> {
        self.issue_pair_in_family_for_workspace(user, wsid, &refresh::new_family_id())
            .await
    }

    /// Issue a token pair scoped to `wsid` whose refresh token joins `family_id`
    /// (rotation). The `wsrole` and the workspace `tier` baked into the access
    /// token, and the workspace the refresh token remembers, all come from a
    /// SERVER-SIDE membership lookup - never from a stale client value.
    ///
    /// Returns [`AuthError::Forbidden`] if the subject is not (or is no longer)
    /// a member of `wsid`.
    async fn issue_pair_in_family_for_workspace(
        &self,
        user: &User,
        wsid: WorkspaceId,
        family_id: &str,
    ) -> Result<TokenPair> {
        // The single membership check every mint path funnels through: the role
        // and the workspace tier are authoritative as of THIS instant.
        let ctx = self
            .memberships
            .context(wsid, &user.id)
            .await?
            .ok_or(AuthError::Forbidden)?;
        let config = self.issuer.config();
        let access_ttl = i64::try_from(config.access_ttl.as_secs()).unwrap_or(i64::MAX);
        let access_token = {
            let keyring = self.keyring.read().expect("keyring poisoned");
            self.issuer.issue_access(
                keyring.active(),
                &user.id,
                wsid,
                ctx.role,
                ctx.tier,
                user.roles.clone(),
            )?
        };
        let expires_at = self.issuer.now_unix()
            + i64::try_from(config.refresh_ttl.as_secs()).unwrap_or(i64::MAX);
        let minted = refresh::mint_in_family(&user.id, wsid, family_id, expires_at);
        self.refresh.insert(minted.record).await?;
        Ok(TokenPair {
            access_token,
            refresh_token: minted.token,
            token_type: "Bearer",
            expires_in: access_ttl,
        })
    }
}

/// Canonical form used for account lookup, throttling and event hashing.
#[must_use]
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Backoff key: a pseudonym, so no plaintext email is stored in the throttle.
fn throttle_key(normalized_email: &str, client_net: Option<&str>) -> String {
    crate::security::pseudonym(
        "login-throttle",
        &format!("{normalized_email}|{}", client_net.unwrap_or("")),
    )
}

/// Whether sign-in must reject accounts whose email is unverified.
///
/// Read from `AMICU_REQUIRE_EMAIL_VERIFICATION` on every call (no caching, so a
/// test or deployment can flip it). DEFAULT OFF: the value must be a recognized
/// truthy token (`1`, `true`, `yes`, `on`, case-insensitive) to enable the gate;
/// anything else, including an unset variable, leaves it off. Off is correct
/// for this PR, which ships no SMTP sender, so no user could verify yet.
fn require_email_verification() -> bool {
    std::env::var("AMICU_REQUIRE_EMAIL_VERIFICATION").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// A fresh global user id: a random UUID rendered as text. This is the token
/// subject (`sub`) AND the `users.id` primary key the Postgres store and every
/// auth token table foreign-key to, so it must be a real UUID (not the old
/// opaque `usr_...` surrogate).
fn new_user_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A stable dummy PHC hash used to equalize sign-in timing for unknown users.
fn dummy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        hash_password(&SecretValue::new(b"amicu-timing-equalizer".to_vec())).unwrap_or_default()
    })
}
