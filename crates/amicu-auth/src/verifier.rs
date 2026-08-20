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

/// A monotonic time source for cache freshness and refetch throttling.
///
/// Injectable so tests advance time deterministically instead of sleeping. This
/// is deliberately monotonic (unlike the issuer's wall-clock `Clock`): cache TTL
/// and refetch cooldown must not be perturbed by clock steps.
pub trait MonoClock: Send + Sync {
    /// The current monotonic instant.
    fn now(&self) -> Instant;
}

/// Monotonic system clock backed by [`Instant::now`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemMonoClock;

impl MonoClock for SystemMonoClock {
    fn now(&self) -> Instant {
        Instant::now()
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
    /// Minimum interval between JWKS refetches triggered by an *unknown* `kid`.
    ///
    /// The `kid` is attacker-controlled (read from the token header before any
    /// signature check), so an unauthenticated flood of random `kid`s must not
    /// force one upstream fetch per request. Within this window at most one
    /// unknown-`kid` refetch happens; a genuinely new key is still picked up by
    /// the first unknown token and, failing that, by the [`Self::cache_ttl`]
    /// refresh.
    pub refetch_cooldown: Duration,
    /// Hard ceiling on how stale the cache may get while still being served.
    ///
    /// Serve-stale keeps valid tokens working through a transient JWKS blip, but
    /// during a *prolonged* provider outage a key the issuer has rotated out or
    /// revoked would otherwise be honoured indefinitely. Past this age (measured
    /// from the last successful fetch) the verifier fails closed even for a
    /// cached `kid`. Must be comfortably longer than [`Self::cache_ttl`].
    pub max_stale: Duration,
}

impl VerifierConfig {
    /// Config with a 5-minute JWKS cache TTL, 30s leeway, a 60s unknown-`kid`
    /// refetch cooldown, and a 1-hour max-stale ceiling.
    #[must_use]
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            leeway_secs: 30,
            cache_ttl: Duration::from_secs(300),
            refetch_cooldown: Duration::from_secs(60),
            max_stale: Duration::from_secs(3600),
        }
    }
}

struct Cache {
    keys: HashMap<String, DecodingKey>,
    /// When the current keys were last fetched successfully (drives TTL).
    fetched_at: Option<Instant>,
    /// When an *unknown-`kid`*-triggered refetch was last attempted (drives the
    /// cooldown). Kept separate from `fetched_at` so the mandatory first fetch
    /// and TTL refreshes never consume the unknown-`kid` budget.
    last_unknown_refetch: Option<Instant>,
}

/// What `verify` should do about the JWKS cache before validating a token.
enum RefetchPlan {
    /// Cache is usable as-is; validate against it.
    Skip,
    /// Refetch first. When the trigger was an unknown `kid`, the cooldown slot
    /// has already been claimed atomically inside `plan_refetch`.
    Fetch,
}

/// Offline verifier over a [`JwksProvider`], with a monotonic [`MonoClock`] for
/// cache freshness and refetch throttling.
pub struct Verifier<P: JwksProvider, C: MonoClock = SystemMonoClock> {
    provider: P,
    config: VerifierConfig,
    validation: Validation,
    cache: RwLock<Cache>,
    clock: C,
}

impl<P: JwksProvider> Verifier<P, SystemMonoClock> {
    /// Build a verifier over the system monotonic clock. Keys are fetched lazily
    /// on first verify.
    #[must_use]
    pub fn new(provider: P, config: VerifierConfig) -> Self {
        Self::with_clock(provider, config, SystemMonoClock)
    }
}

impl<P: JwksProvider, C: MonoClock> Verifier<P, C> {
    /// Build a verifier with an injected monotonic clock (deterministic tests).
    #[must_use]
    pub fn with_clock(provider: P, config: VerifierConfig, clock: C) -> Self {
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
                last_unknown_refetch: None,
            }),
            clock,
        }
    }

    /// The underlying JWKS provider.
    #[must_use]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Verify a token offline and return its claims.
    ///
    /// Refetches the JWKS when the cache is empty, when it is stale and the
    /// `kid` is known, or when the `kid` is unknown and the refetch cooldown has
    /// elapsed (bounding attacker-driven refetch amplification). If a refetch
    /// fails but the token's key is still cached and the cache is younger than
    /// [`VerifierConfig::max_stale`], it is served from the stale cache (fail-open
    /// on availability); it hard-fails when no usable key exists or the cache is
    /// egregiously old. Signature, `exp`, `nbf`, `aud`, and `iss` are always
    /// enforced.
    ///
    /// # Errors
    /// See [`AuthError`] variants for the rejection reasons.
    pub async fn verify(&self, token: &str) -> Result<Claims> {
        let header = decode_header(token).map_err(|_| AuthError::InvalidToken)?;
        let kid = header.kid.ok_or(AuthError::InvalidToken)?;
        let now = self.clock.now();

        if let RefetchPlan::Fetch = self.plan_refetch(&kid, now)
            && let Err(e) = self.refetch(now).await
        {
            // Serve-stale: tolerate a transient provider failure as long as the
            // token's key is still cached and the cache has not aged past
            // `max_stale`. Otherwise fail closed.
            if !self.can_serve_stale(&kid, now) {
                return Err(e);
            }
        }

        let data = {
            let cache = self.cache.read().expect("verifier cache poisoned");
            let key = cache.keys.get(&kid).ok_or(AuthError::UnknownKid)?;
            decode::<Claims>(token, key, &self.validation)
        };

        data.map(|d| d.claims).map_err(|e| map_jwt_error(&e))
    }

    /// Decide whether a refetch is needed and, for the unknown-`kid` path, claim
    /// the cooldown slot atomically so that under a concurrent flood only one
    /// task per window proceeds to fetch.
    fn plan_refetch(&self, kid: &str, now: Instant) -> RefetchPlan {
        // Fast path: the common known-and-fresh case only needs a read lock.
        {
            let cache = self.cache.read().expect("verifier cache poisoned");
            if let Some(fetched_at) = cache.fetched_at
                && cache.keys.contains_key(kid)
                && now.duration_since(fetched_at) < self.config.cache_ttl
            {
                return RefetchPlan::Skip;
            }
        }

        // Slow path: a refetch may be warranted. Take the write lock so the
        // unknown-`kid` cooldown is a single atomic check-and-set; the write lock
        // serializes concurrent tasks, so exactly one claims the slot per window.
        let mut cache = self.cache.write().expect("verifier cache poisoned");
        let Some(fetched_at) = cache.fetched_at else {
            // Never fetched: a mandatory first fetch, not charged to the cooldown.
            return RefetchPlan::Fetch;
        };

        if cache.keys.contains_key(kid) {
            // Known `kid`: refetch only to refresh a stale cache. Reachable only
            // with a validly-signed token, so not an amplification vector.
            return if now.duration_since(fetched_at) >= self.config.cache_ttl {
                RefetchPlan::Fetch
            } else {
                RefetchPlan::Skip
            };
        }

        // Unknown `kid`: cooldown-gated regardless of staleness so a flood of
        // random `kid`s cannot force one upstream fetch per request. Claim the
        // slot in the same critical section that observed it free.
        let ready = match cache.last_unknown_refetch {
            None => true,
            Some(at) => now.duration_since(at) >= self.config.refetch_cooldown,
        };
        if ready {
            cache.last_unknown_refetch = Some(now);
            RefetchPlan::Fetch
        } else {
            RefetchPlan::Skip
        }
    }

    /// Whether a cached key for `kid` may still be served after a refetch
    /// failure: it must exist and the cache must be younger than `max_stale`.
    fn can_serve_stale(&self, kid: &str, now: Instant) -> bool {
        let cache = self.cache.read().expect("verifier cache poisoned");
        let Some(fetched_at) = cache.fetched_at else {
            return false;
        };
        now.duration_since(fetched_at) < self.config.max_stale && cache.keys.contains_key(kid)
    }

    async fn refetch(&self, now: Instant) -> Result<()> {
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
        cache.fetched_at = Some(now);
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
