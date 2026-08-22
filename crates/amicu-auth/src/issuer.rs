//! Access-token issuance.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::claims::{Claims, Tier};
use crate::error::{AuthError, Result};
use crate::key::SigningKey;

/// A source of wall-clock time, injectable for deterministic expiry tests.
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn now_unix(&self) -> i64;
}

/// System clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    }
}

/// Issuer configuration: identity + token lifetimes.
#[derive(Debug, Clone)]
pub struct IssuerConfig {
    /// `iss` claim.
    pub issuer: String,
    /// `aud` claim.
    pub audience: String,
    /// Access-token lifetime.
    pub access_ttl: Duration,
    /// Refresh-token lifetime.
    pub refresh_ttl: Duration,
    /// Password-reset-token lifetime. Deliberately short: a reset link should be
    /// usable soon after it is requested and not linger.
    pub reset_ttl: Duration,
    /// Email-verification-token lifetime. Longer than a reset link: a "confirm
    /// your email" link is often opened hours after sign-up.
    pub verify_ttl: Duration,
}

impl IssuerConfig {
    /// Config with 15-minute access, 30-day refresh, 1-hour reset, and 24-hour
    /// email-verification lifetimes.
    #[must_use]
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            access_ttl: Duration::from_secs(900),
            refresh_ttl: Duration::from_secs(2_592_000),
            reset_ttl: Duration::from_secs(3_600),
            verify_ttl: Duration::from_secs(86_400),
        }
    }
}

/// Signs short-lived access JWTs.
pub struct TokenIssuer<C: Clock = SystemClock> {
    config: IssuerConfig,
    clock: C,
}

impl TokenIssuer<SystemClock> {
    /// Issuer using the system clock.
    #[must_use]
    pub fn new(config: IssuerConfig) -> Self {
        Self {
            config,
            clock: SystemClock,
        }
    }
}

impl<C: Clock> TokenIssuer<C> {
    /// Issuer with an injected clock.
    pub fn with_clock(config: IssuerConfig, clock: C) -> Self {
        Self { config, clock }
    }

    /// Issuer configuration.
    #[must_use]
    pub fn config(&self) -> &IssuerConfig {
        &self.config
    }

    /// Current time from the issuer's clock (seconds since the Unix epoch).
    #[must_use]
    pub fn now_unix(&self) -> i64 {
        self.clock.now_unix()
    }

    /// Build the claim set for a subject at the current clock time.
    #[must_use]
    pub fn build_claims(&self, sub: &str, tier: Tier, roles: Vec<String>) -> Claims {
        let now = self.clock.now_unix();
        Claims {
            iss: self.config.issuer.clone(),
            aud: self.config.audience.clone(),
            sub: sub.to_string(),
            iat: now,
            nbf: now,
            exp: now + i64::try_from(self.config.access_ttl.as_secs()).unwrap_or(i64::MAX),
            tier,
            roles,
        }
    }

    /// Sign an access token for `sub` with the given key.
    ///
    /// # Errors
    /// Returns [`AuthError::Signing`] if encoding fails.
    pub fn issue_access(
        &self,
        key: &SigningKey,
        sub: &str,
        tier: Tier,
        roles: Vec<String>,
    ) -> Result<String> {
        let claims = self.build_claims(sub, tier, roles);
        self.sign(key, &claims)
    }

    /// Sign an explicit claim set (used by tests to forge expiry/nbf edges).
    ///
    /// # Errors
    /// Returns [`AuthError::Signing`] if encoding fails.
    pub fn sign(&self, key: &SigningKey, claims: &Claims) -> Result<String> {
        jsonwebtoken::encode(&key.header(), claims, key.encoding_key())
            .map_err(|_| AuthError::Signing)
    }
}
