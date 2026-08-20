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
use crate::password::{check_password_policy, hash_password, verify_password};
use crate::refresh::{self, RefreshStore};
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
    base_url: String,
}

impl<C: Clock> AuthService<C> {
    /// Assemble the service.
    pub fn new(
        keyring: Keyring,
        issuer: TokenIssuer<C>,
        users: Arc<dyn UserStore>,
        refresh: Arc<dyn RefreshStore>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            keyring: RwLock::new(keyring),
            issuer,
            users,
            refresh,
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
            .create(User {
                id: id.clone(),
                email,
                password_hash,
                tier: Tier::Community,
                roles: Vec::new(),
            })
            .await
        {
            Ok(()) => Ok(SignUpOutcome::Created(id)),
            // Swallow the collision: surfacing it would enable enumeration.
            //
            // TODO(amicu-planning#96): once the email-verification pipeline
            // lands, dispatch here — a verification link on `Created`, an
            // "account already exists / reset your password" notice on
            // `AlreadyRegistered` — both out-of-band, preserving the identical
            // HTTP response.
            Err(AuthError::UserExists) => Ok(SignUpOutcome::AlreadyRegistered),
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
        let record = self
            .refresh
            .get(&hash)
            .await?
            .ok_or(AuthError::RefreshInvalid)?;
        // Reuse detection: an already-revoked token being presented again is a
        // theft/replay signal. Revoke and flag the whole family, then reject.
        if record.revoked {
            self.refresh.compromise_family(&record.family_id).await?;
            return Err(AuthError::RefreshInvalid);
        }
        if record.expires_at <= self.issuer.now_unix() {
            return Err(AuthError::RefreshInvalid);
        }
        // Rotate: revoke the presented token before minting its successor into
        // the same family.
        self.refresh.revoke(&hash).await?;
        let user = self
            .users
            .get_by_id(&record.user_id)
            .await?
            .ok_or(AuthError::RefreshInvalid)?;
        self.issue_pair_in_family(&user, &record.family_id).await
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
