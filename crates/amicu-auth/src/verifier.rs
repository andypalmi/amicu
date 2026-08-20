//! Reusable offline token verifier.
//!
//! Other services embed this: it fetches the issuer's JWKS once, caches the
//! decoded keys, and validates tokens locally (signature + `exp`/`nbf` +
//! `aud`/`iss`) with no round-trip on the hot path. It refetches only when the
//! cache is stale or a token carries an unknown `kid` (e.g. after rotation).

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

use crate::claims::Claims;
use crate::error::{AuthError, Result};
use crate::jwks::Jwks;

/// Supplies the current JWKS. Implementors choose the transport (HTTP, a shared
/// cache, a test double); the verifier owns the caching policy.
#[async_trait]
pub trait JwksProvider: Send + Sync {
    /// Fetch the current JWKS.
    ///
    /// # Errors
    /// Returns [`AuthError::JwksFetch`] on transport/parse failure.
    async fn fetch(&self) -> Result<Jwks>;
}

/// A fixed JWKS, for embedding a known key set or for tests.
pub struct StaticJwksProvider {
    jwks: Jwks,
}

impl StaticJwksProvider {
    /// Wrap a fixed JWKS.
    #[must_use]
    pub fn new(jwks: Jwks) -> Self {
        Self { jwks }
    }
}

#[async_trait]
impl JwksProvider for StaticJwksProvider {
    async fn fetch(&self) -> Result<Jwks> {
        Ok(self.jwks.clone())
    }
}

/// Verifier configuration: the identity to enforce and cache freshness.
#[derive(Debug, Clone)]
pub struct VerifierConfig {
    /// Required `iss`.
    pub issuer: String,
    /// Required `aud`.
    pub audience: String,
    /// Clock-skew leeway, seconds.
    pub leeway_secs: u64,
    /// How long cached keys are considered fresh before a background-triggered
    /// refetch on the next verify.
    pub cache_ttl: Duration,
}

impl VerifierConfig {
    /// Config with a 5-minute JWKS cache TTL and 30s leeway.
    #[must_use]
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            leeway_secs: 30,
            cache_ttl: Duration::from_secs(300),
        }
    }
}

struct Cache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
}

/// Offline verifier over a [`JwksProvider`].
pub struct Verifier<P: JwksProvider> {
    provider: P,
    config: VerifierConfig,
    validation: Validation,
    cache: RwLock<Cache>,
}

impl<P: JwksProvider> Verifier<P> {
    /// Build a verifier. Keys are fetched lazily on first verify.
    #[must_use]
    pub fn new(provider: P, config: VerifierConfig) -> Self {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(std::slice::from_ref(&config.issuer));
        validation.set_audience(std::slice::from_ref(&config.audience));
        validation.set_required_spec_claims(&["exp", "iat", "nbf", "sub", "aud", "iss"]);
        validation.validate_nbf = true;
        validation.validate_aud = true;
        validation.leeway = config.leeway_secs;
        Self {
            provider,
            config,
            validation,
            cache: RwLock::new(Cache {
                keys: HashMap::new(),
                fetched_at: None,
            }),
        }
    }

    /// The underlying JWKS provider.
    #[must_use]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Verify a token offline and return its claims.
    ///
    /// Refetches the JWKS only when the cache is empty/stale or the token's
    /// `kid` is unknown, then validates signature, `exp`, `nbf`, `aud`, `iss`.
    ///
    /// # Errors
    /// See [`AuthError`] variants for the rejection reasons.
    pub async fn verify(&self, token: &str) -> Result<Claims> {
        let header = decode_header(token).map_err(|_| AuthError::InvalidToken)?;
        let kid = header.kid.ok_or(AuthError::InvalidToken)?;

        if self.needs_refetch(&kid) {
            self.refetch().await?;
        }

        let data = {
            let cache = self.cache.read().expect("verifier cache poisoned");
            let key = cache.keys.get(&kid).ok_or(AuthError::UnknownKid)?;
            decode::<Claims>(token, key, &self.validation)
        };

        data.map(|d| d.claims).map_err(|e| map_jwt_error(&e))
    }

    fn needs_refetch(&self, kid: &str) -> bool {
        let cache = self.cache.read().expect("verifier cache poisoned");
        match cache.fetched_at {
            None => true,
            Some(at) => at.elapsed() >= self.config.cache_ttl || !cache.keys.contains_key(kid),
        }
    }

    async fn refetch(&self) -> Result<()> {
        let jwks = self.provider.fetch().await?;
        let mut keys = HashMap::with_capacity(jwks.keys.len());
        for jwk in &jwks.keys {
            let raw = URL_SAFE_NO_PAD
                .decode(&jwk.x)
                .map_err(|_| AuthError::JwksFetch)?;
            keys.insert(jwk.kid.clone(), DecodingKey::from_ed_der(&raw));
        }
        let mut cache = self.cache.write().expect("verifier cache poisoned");
        cache.keys = keys;
        cache.fetched_at = Some(Instant::now());
        Ok(())
    }
}

fn map_jwt_error(err: &jsonwebtoken::errors::Error) -> AuthError {
    match err.kind() {
        ErrorKind::ExpiredSignature => AuthError::Expired,
        ErrorKind::ImmatureSignature => AuthError::NotYetValid,
        ErrorKind::InvalidSignature => AuthError::SignatureInvalid,
        ErrorKind::InvalidAudience => AuthError::AudienceRejected,
        ErrorKind::InvalidIssuer => AuthError::IssuerRejected,
        _ => AuthError::InvalidToken,
    }
}
