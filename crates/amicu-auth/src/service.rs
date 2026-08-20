//! Auth service orchestration: sign-up, sign-in, refresh rotation, logout, and
//! JWKS/discovery publication.

use std::sync::{Arc, OnceLock, RwLock};

use amicu_core::secret::SecretValue;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand_core::{OsRng, RngCore};

use crate::claims::Tier;
use crate::error::{AuthError, Result};
use crate::issuer::{Clock, SystemClock, TokenIssuer};
use crate::jwks::{Jwks, OpenIdConfiguration};
use crate::key::SigningKey;
use crate::keyring::Keyring;
use crate::notify::{Notification, NotificationKind, Notifier};
use crate::password::{check_password_policy, hash_password, verify_password};
use crate::refresh::{self, RefreshStore};
use crate::reset::{self, ResetStore};
use crate::user::{User, UserStore};

/// Result of a sign-up attempt, reported truthfully to the (trusted) caller.
///
/// The HTTP boundary MUST collapse both variants to one identical response so a
/// client cannot tell a new registration from a hit on an existing account
/// (email enumeration). The distinction is retained here only for internal use:
/// audit metrics today, and — once the email pipeline lands — deciding which
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
    keyring: RwLock<Keyring>,
    issuer: TokenIssuer<C>,
    users: Arc<dyn UserStore>,
    refresh: Arc<dyn RefreshStore>,
    resets: Arc<dyn ResetStore>,
    notifier: Arc<dyn Notifier>,
    base_url: String,
}

impl<C: Clock> AuthService<C> {
    /// Assemble the service.
    pub fn new(
        keyring: Keyring,
        issuer: TokenIssuer<C>,
        users: Arc<dyn UserStore>,
        refresh: Arc<dyn RefreshStore>,
        resets: Arc<dyn ResetStore>,
        notifier: Arc<dyn Notifier>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            keyring: RwLock::new(keyring),
            issuer,
            users,
            refresh,
            resets,
            notifier,
            base_url: base_url.into(),
        }
    }

    /// Register a new user.
    ///
    /// Anti-enumeration: this never signals whether the email was already
    /// taken. An `AlreadyRegistered` collision is reported as its own
    /// [`SignUpOutcome`] variant (not an error), and the same password hashing
    /// runs on both paths so neither the result *nor the timing* distinguishes
    /// a new account from an existing one. The caller MUST render both outcomes
    /// identically (see [`SignUpOutcome`]) — mirroring the sign-in
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
            Ok(()) => Ok(SignUpOutcome::Created(id)),
            // Swallow the collision: surfacing it would enable enumeration.
            //
            // Instead notify the *real* owner out-of-band via the notification
            // seam (amicu-planning#102): an "account already exists / reset your
            // password" nudge that the requester never observes on the identical
            // HTTP response. A verification link on `Created` still waits on the
            // email-verification pipeline (amicu-planning#96).
            Err(AuthError::UserExists) => {
                // Dispatch off the response path (see `spawn_notification`): the
                // notice is sent only on the collision path, so awaiting it here
                // would make the collision measurably slower than a fresh
                // sign-up — an account-enumeration timing oracle. The response
                // stays identical and same-timing on both paths.
                self.spawn_notification(Notification {
                    recipient: email,
                    kind: NotificationKind::AccountExists,
                });
                Ok(SignUpOutcome::AlreadyRegistered)
            }
            Err(other) => Err(other),
        }
    }

    /// Authenticate with email+password and issue a token pair.
    ///
    /// # Errors
    /// [`AuthError::InvalidCredentials`] on unknown user or wrong password.
    pub async fn sign_in(&self, email: &str, password: &SecretValue) -> Result<TokenPair> {
        let email = normalize_email(email);
        let user = self.users.get_by_email(&email).await?;
        let Some(user) = user else {
            // Equalize timing against a dummy hash to blunt user enumeration.
            let _ = verify_password(password, dummy_hash());
            return Err(AuthError::InvalidCredentials);
        };
        verify_password(password, &user.password_hash)?;
        self.issue_pair(&user).await
    }

    /// Rotate a refresh token: validate it, revoke it, and issue a fresh pair
    /// within the same token family.
    ///
    /// Reuse/theft detection: presenting a token that is *already revoked*
    /// (typically an old token replayed after the family has rotated past it)
    /// is treated as a compromise — the entire family is revoked and flagged,
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
                Err(AuthError::RefreshInvalid)
            }
            // This caller won the rotation; mint the successor into the family.
            refresh::RefreshConsume::Consumed { user_id, family_id } => {
                let user = self
                    .users
                    .get_by_id(&user_id)
                    .await?
                    .ok_or(AuthError::RefreshInvalid)?;
                self.issue_pair_in_family(&user, &family_id).await
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
    /// Anti-enumeration: this ALWAYS returns immediately with `Ok`, and the HTTP
    /// boundary always returns the same generic `202`, whether or not the email
    /// is registered. Crucially, ALL of the account-dependent work — the user
    /// lookup, and (only when the account exists) the token mint, store insert,
    /// and out-of-band delivery — runs on a detached background task, so the
    /// synchronous response time is the *same* on the exists and miss paths.
    ///
    /// Doing that work inline would be an account-enumeration timing oracle: the
    /// exists path does strictly more synchronous work than the miss path, and
    /// the gap becomes a strong signal the moment a synchronous mailer replaces
    /// the dev logger (recover would block on SMTP only for real accounts).
    /// Detaching it removes the send (and the store I/O) from the response path
    /// entirely. A reset token is still minted and delivered only when the
    /// account exists; on a miss nothing is sent and nothing is stored.
    ///
    /// # Errors
    /// Never returns an error: background failures are swallowed (surfacing them
    /// would itself be an existence signal). Kept `Result` for a uniform handler
    /// signature.
    // Intentionally `async` with no `.await` on the response path: detaching all
    // account-dependent work is the whole anti-enumeration point (see above). The
    // `async` signature is part of the `AuthService` contract — a persistent
    // store would enqueue via an awaited durable-queue write here — so keep it.
    // `unused_async_trait_impl` (clippy 1.98+) is the same intent as
    // `unused_async` for this signature-contract case: the `async` is kept
    // deliberately, not by accident.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn request_password_reset(&self, email: &str) -> Result<()> {
        let email = normalize_email(email);
        // Empty email is a request-level no-op (not an account signal); returning
        // here changes nothing observable since the exists/miss paths are already
        // equalized by the detached task below.
        if email.is_empty() {
            return Ok(());
        }
        // Move the seams into a detached task. The response returns now, in
        // account-independent time; the (possibly network-bound) lookup + mint +
        // store + send happen off the response path. See the doc comment.
        let users = self.users.clone();
        let resets = self.resets.clone();
        let notifier = self.notifier.clone();
        let expires_at = self.issuer.now_unix()
            + i64::try_from(self.issuer.config().reset_ttl.as_secs()).unwrap_or(i64::MAX);
        tokio::spawn(async move {
            let Ok(Some(user)) = users.get_by_email(&email).await else {
                return; // unknown account: send nothing, store nothing
            };
            let minted = reset::mint(&user.id, expires_at);
            if resets.insert(minted.record).await.is_err() {
                return; // best-effort: a store failure must not surface anywhere
            }
            // Best-effort delivery: a transport failure must not change anything
            // observable (it already changed nothing on the response path).
            let _ = notifier
                .notify(Notification {
                    recipient: email,
                    kind: NotificationKind::PasswordReset {
                        reset_token: minted.token,
                    },
                })
                .await;
        });
        Ok(())
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
        // — the user simply requests another reset. The reverse order would leave
        // the dangerous state (new password live, but old sessions still valid)
        // on a partial failure.
        self.refresh.revoke_all_for_user(&user_id).await?;
        self.users.update_password(&user_id, password_hash).await?;
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

    /// The discovery document for `/.well-known/openid-configuration`.
    #[must_use]
    pub fn openid_configuration(&self) -> OpenIdConfiguration {
        OpenIdConfiguration::new(self.issuer.config().issuer.clone(), &self.base_url)
    }

    /// Deliver a notification out-of-band without gating the caller's response.
    ///
    /// The (possibly network-bound) send runs on a detached task, so response
    /// timing never depends on whether — or to whom — a message is dispatched.
    /// This is load-bearing for anti-enumeration: awaiting a send that only
    /// happens on the account-exists path would be an enumeration timing oracle,
    /// trivially today and strongly once a synchronous mailer replaces the dev
    /// logger. Delivery is best-effort; a transport failure is swallowed.
    fn spawn_notification(&self, notification: Notification) {
        let notifier = self.notifier.clone();
        tokio::spawn(async move {
            let _ = notifier.notify(notification).await;
        });
    }

    /// Issue a token pair opening a brand-new refresh family (fresh sign-in).
    async fn issue_pair(&self, user: &User) -> Result<TokenPair> {
        self.issue_pair_in_family(user, &refresh::new_family_id())
            .await
    }

    /// Issue a token pair whose refresh token joins `family_id` (rotation).
    async fn issue_pair_in_family(&self, user: &User, family_id: &str) -> Result<TokenPair> {
        let config = self.issuer.config();
        let access_ttl = i64::try_from(config.access_ttl.as_secs()).unwrap_or(i64::MAX);
        let access_token = {
            let keyring = self.keyring.read().expect("keyring poisoned");
            self.issuer
                .issue_access(keyring.active(), &user.id, user.tier, user.roles.clone())?
        };
        let expires_at = self.issuer.now_unix()
            + i64::try_from(config.refresh_ttl.as_secs()).unwrap_or(i64::MAX);
        let minted = refresh::mint_in_family(&user.id, family_id, expires_at);
        self.refresh.insert(minted.record).await?;
        Ok(TokenPair {
            access_token,
            refresh_token: minted.token,
            token_type: "Bearer",
            expires_in: access_ttl,
        })
    }
}

fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

fn new_user_id() -> String {
    let mut bytes = [0u8; 12];
    OsRng.fill_bytes(&mut bytes);
    format!("usr_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// A stable dummy PHC hash used to equalize sign-in timing for unknown users.
fn dummy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        hash_password(&SecretValue::new(b"amicu-timing-equalizer".to_vec())).unwrap_or_default()
    })
}
