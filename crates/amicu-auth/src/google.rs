//! Verifier for Google-issued OIDC ID tokens.
//!
//! This is what authenticates Cloud Scheduler's once-a-minute
//! `POST /internal/outbox/drain` call. Cloud Scheduler mints a Google-SIGNED ID
//! token as a dedicated service account and sends it as `Authorization: Bearer`;
//! there is no shared secret anywhere in the path, and nothing sensitive ever
//! lands in Terraform state.
//!
//! # Why not [`crate::verifier::Verifier`]
//!
//! The first-party verifier is Ed25519/`EdDSA`-only, all the way down: its
//! [`Jwks`](crate::jwks::Jwks) type models an RFC 8037 OKP key (a single `x`
//! coordinate) and it builds `DecodingKey::from_ed_der`. Google signs ID tokens
//! with RS256 and publishes RSA JWKs (`n`/`e`), so there is no key shape or
//! algorithm in common - reusing it would mean widening the first-party JWKS
//! type to model keys the first-party issuer will never mint. This module is
//! therefore deliberately separate, but it is NOT a second JWT stack: it uses
//! the same `jsonwebtoken` crate, the same [`AuthError`] taxonomy, and the same
//! [`MonoClock`] cache-freshness seam.
//!
//! # What is checked
//!
//! In order, and all of them:
//! 1. RS256 header algorithm and a `kid` present in Google's published key set.
//! 2. Signature, against that key.
//! 3. `exp` / `iat` (with leeway).
//! 4. `iss` is one of [`GOOGLE_ISSUERS`].
//! 5. `aud` equals the configured audience (the Cloud Run service URL).
//! 6. `email_verified` is true AND `email` is in the configured allowlist.
//!
//! 1-5 are rejections of the TOKEN (the caller could not prove who it is): the
//! HTTP boundary renders them `401`. 6 is a rejection of the CALLER (a valid,
//! correctly-audienced Google token from an identity we do not accept):
//! [`AuthError::Forbidden`], rendered `403`.
//!
//! # Key caching
//!
//! Google's certs endpoint sets `Cache-Control: max-age=...`; [`CachedCerts`]
//! carries that value and the verifier honours it, clamped to
//! [`GoogleIdTokenConfig::min_cache`]/[`max_cache`](GoogleIdTokenConfig::max_cache)
//! so a missing, absurdly short, or absurdly long value cannot turn every drain
//! request into an upstream fetch or pin a rotated-out key forever. An UNKNOWN
//! `kid` also triggers a refetch, but only once per
//! [`GoogleIdTokenConfig::refetch_cooldown`]: `kid` is read from an unverified
//! token header, so it is attacker-controlled and must never be a
//! one-fetch-per-request amplifier.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

use crate::error::{AuthError, Result};
use crate::verifier::{MonoClock, SystemMonoClock};

/// Both spellings Google uses for the `iss` claim of an ID token. Google's own
/// documentation requires accepting either.
pub const GOOGLE_ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];

/// Google's public signing certificates in JWK form.
pub const GOOGLE_CERTS_URI: &str = "https://www.googleapis.com/oauth2/v3/certs";

/// One RSA public key from Google's certs document. Only the fields needed to
/// build a decoding key are modelled; anything else Google adds is ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct RsaJwk {
    /// Key id, matching the JWT `kid` header.
    pub kid: String,
    /// base64url modulus.
    pub n: String,
    /// base64url exponent.
    pub e: String,
}

/// Google's certs document: a set of [`RsaJwk`]s.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RsaJwks {
    /// The published keys.
    pub keys: Vec<RsaJwk>,
}

/// A fetched certs document plus the freshness the origin advertised.
#[derive(Debug, Clone)]
pub struct CachedCerts {
    /// The key set.
    pub jwks: RsaJwks,
    /// `Cache-Control: max-age`, or `None` when the response did not say.
    pub max_age: Option<Duration>,
}

/// Supplies Google's current signing certificates. Implementors choose the
/// transport; the verifier owns the caching policy. Injectable so tests never
/// make a real network call.
#[async_trait]
pub trait GoogleCertsSource: Send + Sync {
    /// Fetch the current certs document.
    ///
    /// # Errors
    /// [`AuthError::JwksFetch`] on transport/parse failure.
    async fn fetch(&self) -> Result<CachedCerts>;
}

/// The claims this verifier reads out of a Google ID token.
///
/// `aud` and `iss` are validated by `jsonwebtoken` during decode, not here;
/// they are carried so a caller can log or assert on them.
#[derive(Debug, Clone, Deserialize)]
pub struct GoogleIdTokenClaims {
    /// Issuer - one of [`GOOGLE_ISSUERS`].
    pub iss: String,
    /// Audience - the configured Cloud Run service URL.
    pub aud: String,
    /// Google's opaque subject id for the calling identity.
    pub sub: String,
    /// Expiry, seconds since the Unix epoch.
    pub exp: i64,
    /// The calling service account's email, when the token carries one.
    #[serde(default)]
    pub email: Option<String>,
    /// Whether Google asserts that `email` is verified. Absent on tokens minted
    /// without the email scope, which this verifier refuses.
    #[serde(default)]
    pub email_verified: Option<bool>,
}

/// What a [`GoogleIdTokenVerifier`] enforces, and how long it caches keys.
#[derive(Debug, Clone)]
pub struct GoogleIdTokenConfig {
    /// Required `aud`. On Cloud Run this is the service's own base URL (NOT the
    /// `/internal/outbox/drain` path) - the audience Cloud Scheduler is
    /// configured to mint, matching Google's documented pattern.
    pub audience: String,
    /// Allowlisted `email` claims: the Cloud Scheduler invoker service
    /// account(s). Compared case-insensitively. An EMPTY allowlist accepts
    /// nobody - a misconfiguration must fail closed, never open.
    pub allowed_emails: Vec<String>,
    /// Clock-skew leeway, seconds.
    pub leeway_secs: u64,
    /// Floor on the cache lifetime, regardless of what `Cache-Control` says.
    pub min_cache: Duration,
    /// Ceiling on the cache lifetime, regardless of what `Cache-Control` says.
    pub max_cache: Duration,
    /// Minimum interval between refetches triggered by an UNKNOWN `kid` (which
    /// is attacker-controlled - see the module docs).
    pub refetch_cooldown: Duration,
}

impl GoogleIdTokenConfig {
    /// Config for `audience`, accepting `allowed_emails`, with a 30s leeway, a
    /// 5-minute cache floor, a 24-hour cache ceiling, and a 60s unknown-`kid`
    /// refetch cooldown.
    #[must_use]
    pub fn new(audience: impl Into<String>, allowed_emails: Vec<String>) -> Self {
        Self {
            audience: audience.into(),
            allowed_emails: allowed_emails
                .into_iter()
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
            leeway_secs: 30,
            min_cache: Duration::from_secs(300),
            max_cache: Duration::from_secs(86_400),
            refetch_cooldown: Duration::from_secs(60),
        }
    }
}

/// Cached decoding keys plus when they go stale.
struct CertCache {
    keys: HashMap<String, DecodingKey>,
    /// When the cache stops being usable; `None` means "never fetched".
    expires_at: Option<Instant>,
    /// When an unknown-`kid` refetch was last attempted (drives the cooldown).
    last_unknown_refetch: Option<Instant>,
}

/// Verifies Google-issued OIDC ID tokens against Google's published certs.
pub struct GoogleIdTokenVerifier<S: GoogleCertsSource, C: MonoClock = SystemMonoClock> {
    source: S,
    config: GoogleIdTokenConfig,
    validation: Validation,
    cache: RwLock<CertCache>,
    clock: C,
}

impl<S: GoogleCertsSource> GoogleIdTokenVerifier<S, SystemMonoClock> {
    /// Build a verifier over the system monotonic clock. Keys are fetched
    /// lazily on the first verify.
    #[must_use]
    pub fn new(source: S, config: GoogleIdTokenConfig) -> Self {
        Self::with_clock(source, config, SystemMonoClock)
    }
}

impl<S: GoogleCertsSource, C: MonoClock> GoogleIdTokenVerifier<S, C> {
    /// Build a verifier with an injected monotonic clock (deterministic tests).
    #[must_use]
    pub fn with_clock(source: S, config: GoogleIdTokenConfig, clock: C) -> Self {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&GOOGLE_ISSUERS);
        validation.set_audience(std::slice::from_ref(&config.audience));
        validation.set_required_spec_claims(&["exp", "aud", "iss", "sub"]);
        validation.validate_aud = true;
        validation.leeway = config.leeway_secs;
        Self {
            source,
            config,
            validation,
            cache: RwLock::new(CertCache {
                keys: HashMap::new(),
                expires_at: None,
                last_unknown_refetch: None,
            }),
            clock,
        }
    }

    /// The configuration this verifier enforces.
    #[must_use]
    pub fn config(&self) -> &GoogleIdTokenConfig {
        &self.config
    }

    /// The underlying certs source (mirrors
    /// [`Verifier::provider`](crate::verifier::Verifier::provider)).
    #[must_use]
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Verify `token` and return its claims, having enforced every check listed
    /// in the module docs.
    ///
    /// # Errors
    /// [`AuthError::InvalidToken`] (malformed, missing/!RS256 header, missing
    /// `kid`), [`AuthError::UnknownKid`], [`AuthError::SignatureInvalid`],
    /// [`AuthError::Expired`], [`AuthError::IssuerRejected`],
    /// [`AuthError::AudienceRejected`], [`AuthError::JwksFetch`] (keys
    /// unavailable), or [`AuthError::Forbidden`] when the token is entirely
    /// valid but its `email` is unverified or not allowlisted.
    pub async fn verify(&self, token: &str) -> Result<GoogleIdTokenClaims> {
        let header = decode_header(token).map_err(|_| AuthError::InvalidToken)?;
        // Pin the algorithm from the header too, not just in `Validation`: an
        // explicit, early rejection of anything but RS256 is clearer than
        // relying on the decode to notice.
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::InvalidToken);
        }
        let kid = header.kid.ok_or(AuthError::InvalidToken)?;
        let now = self.clock.now();

        if self.needs_refetch(&kid, now) {
            self.refetch(now).await?;
        }

        let data = {
            let cache = self.cache.read().expect("google certs cache poisoned");
            let key = cache.keys.get(&kid).ok_or(AuthError::UnknownKid)?;
            decode::<GoogleIdTokenClaims>(token, key, &self.validation)
        };
        // Signature, exp/iat, iss and aud are all enforced here.
        let claims = data.map(|d| d.claims).map_err(|e| map_jwt_error(&e))?;

        // Caller identity. A token that got this far is a genuine, correctly
        // audienced Google token - it simply is not one of the identities this
        // endpoint accepts, which is a 403, not a 401.
        if claims.email_verified != Some(true) {
            return Err(AuthError::Forbidden);
        }
        let email = claims
            .email
            .as_deref()
            .ok_or(AuthError::Forbidden)?
            .trim()
            .to_ascii_lowercase();
        if !self.config.allowed_emails.contains(&email) {
            return Err(AuthError::Forbidden);
        }

        Ok(claims)
    }

    /// Whether the certs cache must be refetched before validating a token with
    /// this `kid`. Claims the unknown-`kid` cooldown slot atomically, so a flood
    /// of random `kid`s produces at most one upstream fetch per window.
    fn needs_refetch(&self, kid: &str, now: Instant) -> bool {
        {
            let cache = self.cache.read().expect("google certs cache poisoned");
            if let Some(expires_at) = cache.expires_at
                && cache.keys.contains_key(kid)
                && now < expires_at
            {
                return false;
            }
        }

        let mut cache = self.cache.write().expect("google certs cache poisoned");
        let Some(expires_at) = cache.expires_at else {
            // Never fetched: mandatory first fetch, not charged to the cooldown.
            return true;
        };
        if cache.keys.contains_key(kid) {
            // Known `kid`: refetch only to refresh an expired cache. Reachable
            // only with a token bearing a real Google key id.
            return now >= expires_at;
        }
        // Unknown `kid`: cooldown-gated regardless of freshness.
        let ready = cache
            .last_unknown_refetch
            .is_none_or(|at| now.duration_since(at) >= self.config.refetch_cooldown);
        if ready {
            cache.last_unknown_refetch = Some(now);
        }
        ready
    }

    async fn refetch(&self, now: Instant) -> Result<()> {
        let fetched = self.source.fetch().await?;
        let lifetime = fetched
            .max_age
            .unwrap_or(self.config.min_cache)
            .clamp(self.config.min_cache, self.config.max_cache);
        let mut keys = HashMap::with_capacity(fetched.jwks.keys.len());
        for jwk in &fetched.jwks.keys {
            let key = DecodingKey::from_rsa_components(&jwk.n, &jwk.e)
                .map_err(|_| AuthError::JwksFetch)?;
            keys.insert(jwk.kid.clone(), key);
        }
        let mut cache = self.cache.write().expect("google certs cache poisoned");
        cache.keys = keys;
        cache.expires_at = Some(now + lifetime);
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

/// Parse `max-age=<seconds>` out of a `Cache-Control` header value.
///
/// Returns `None` when the directive is absent or unparseable; the verifier
/// then falls back to its configured floor.
#[must_use]
pub fn parse_max_age(cache_control: &str) -> Option<Duration> {
    cache_control
        .split(',')
        .map(str::trim)
        .find_map(|directive| {
            let (name, value) = directive.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("max-age")
                .then(|| value.trim().trim_matches('"').parse::<u64>().ok())?
        })
        .map(Duration::from_secs)
}

/// reqwest-backed [`GoogleCertsSource`] against [`GOOGLE_CERTS_URI`].
///
/// rustls backend, no system OpenSSL - the same convention as
/// [`HttpJwksProvider`](crate::http_provider::HttpJwksProvider).
#[cfg(feature = "reqwest-provider")]
pub struct HttpGoogleCertsSource {
    client: reqwest::Client,
    certs_uri: String,
}

#[cfg(feature = "reqwest-provider")]
impl HttpGoogleCertsSource {
    /// Whole-request timeout, so a hung endpoint cannot stall a drain request.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
    /// Hard cap on the certs body. Google publishes a handful of RSA keys (a
    /// few KiB); 64 KiB is ample headroom while refusing an unbounded body.
    const MAX_BYTES: usize = 64 * 1024;

    /// Source against Google's production certs endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self::with_uri(GOOGLE_CERTS_URI)
    }

    /// Source against an arbitrary certs URI (tests, or a pinned mirror).
    #[must_use]
    pub fn with_uri(certs_uri: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Self::REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            client,
            certs_uri: certs_uri.into(),
        }
    }
}

#[cfg(feature = "reqwest-provider")]
impl Default for HttpGoogleCertsSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "reqwest-provider")]
impl std::fmt::Debug for HttpGoogleCertsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpGoogleCertsSource")
            .field("certs_uri", &self.certs_uri)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "reqwest-provider")]
#[async_trait]
impl GoogleCertsSource for HttpGoogleCertsSource {
    async fn fetch(&self) -> Result<CachedCerts> {
        let mut resp = self
            .client
            .get(&self.certs_uri)
            .send()
            .await
            .map_err(|_| AuthError::JwksFetch)?
            .error_for_status()
            .map_err(|_| AuthError::JwksFetch)?;

        let max_age = resp
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_max_age);

        if resp
            .content_length()
            .is_some_and(|len| len > Self::MAX_BYTES as u64)
        {
            return Err(AuthError::JwksFetch);
        }
        // Enforce the cap while streaming too: Content-Length may be absent or
        // untruthful.
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| AuthError::JwksFetch)? {
            if body.len() + chunk.len() > Self::MAX_BYTES {
                return Err(AuthError::JwksFetch);
            }
            body.extend_from_slice(&chunk);
        }

        let jwks = serde_json::from_slice::<RsaJwks>(&body).map_err(|_| AuthError::JwksFetch)?;
        Ok(CachedCerts { jwks, max_age })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fixed RSA-2048 private key in PKCS#1 DER (base64), used ONLY to sign
    /// test tokens. Generated once, offline, and embedded rather than generated
    /// per run: RSA keygen is slow, and a fixed key keeps the tests
    /// deterministic. It is a TEST key and protects nothing.
    const TEST_RSA_PKCS1_DER_B64: &str = concat!(
        "MIIEogIBAAKCAQEAtx9q8zOiQf3icYPO9sYyXwSx8uh82o99DP6+hxh5VUsVuFyQqYzLP6PbKm76",
        "LdqboYlClHfEaWz22+skQpe0Xz/uWQaXLI6vHez+BR9pFtUr8PUTKV4s5AOUyUqQH2fyYwM8Lf1C",
        "FoMhaxVfwAB5Djy1Tj977MxAE39A1Ic6zln3c7sH7QV/iu4BFiLpiTGfZdPLaBxnauau+BqNohOu",
        "H+3A8rGXw7RZCp7SF9xrlnjOKqcgIRGWI4+gG628bPmmqFkDrOr7mrirVPoZYN5ntZS2zvZBFt3L",
        "xSym/RU63Hx9GkFcb4exbODp2foV4GRph/aRL4H1GEIxn7Aft/7K5wIDAQABAoIBADfd8+kChawQ",
        "Yrcy+T88ND3c1tU+rGiZghMm3MWmZ83nvFakltXDSHa/7KtDdDvtB7H+iDt3Voq7XYy2bjeKxNKS",
        "VIsAgbpsRWyh0tWJQaxfmKpIenIScyuVFB1dDB8ovUALyp5VN4ZatyVOPgn7mvMG/mFC6PHTVnEc",
        "T5EHOJ+fOrww3hUBn2T9h5n2wdIk0EF9+kj3083LDLFlfxXWW6dF8LGuavrsCiwD1mhdbWsyMxYO",
        "NGa59cDgNteoEEFSjUxfOg9BqVcEOik4aq79JS0wmEKt3b/CCMS5VRJrBYm81nqIYOyzdwPuxAge",
        "m/k4ZS1FSudUBKFG/2W9yCny7OECgYEA314RIIC8SQrdhot33oHcVeFn7BRy/wfiEkHlJ+KKCfnV",
        "tUg8lxd4tN1u6RV7Yq+fozpkSgXQsbUZUeauIgkTX4jxVvooTpXYpIe37ExzbrJf3bclWH0MY4PR",
        "56FO7sdNLzxFv5MzZhClNsuj6kCHZPlI/McHRpmbSw3FcfDpzSkCgYEA0eAzLuvXdVv4p+zKzKMF",
        "DZ3zAchawbYUEq49/tbmJIZbwLD/lTs9I16B5oxhhiqjAP1oKqAgiy2SmwgfR+pulF6FbNomDTfq",
        "sJ0IJnsHsInaxGlMTnkYGnMOxalDIp07zEZSvK1cG7zakeiIDZAvzWcVIoxlHtg7fk/IVxLQyY8C",
        "gYAJnj1SjviMlKExV/QTargYIpqc+cxx2t8LNjct3n1fLJjdM30YbHRQJXDPLJOhjjprhuomndV8",
        "sXWjpzE8WfOxm2349tChgaZQf1+WFzypOZkTLP24r7nG1zhEpxeFBKP4Mro2I5qE/JaAo4gtYBX0",
        "bR+NcuyHR7n+I1urKlqG8QKBgECIFlTMEXC1ecxwY57uVSRtt4WJzFYZnSxtRQSxoQwD//xMsBl7",
        "oH3alkiES4xMWdh5Emlg2QDSX2yp36xmKeAW/CGPJfoUgmYRsqraU2/1wz13Pw4177JaRGWEeO8Q",
        "t41E5jW7fEHPHMmWXRcGiZw3oOkBkCoexJQ/BdO4JliDAoGAWoEHURKYddyS7nz/lB0oveFOfPUw",
        "BFL/Vyo1FY4lqfzfsCl9A3w3YWuXtI8PA+3o0WibE8wzI5okdIPzkmJJqJvyT6S9MVrmGHkpEh+b",
        "38Oxw+9xaqNrKipCDadrNdJalv7P1Wl0WYazgKF0KBx6gBJyP7+bc0ssDTiplwXF6gw=",
    );

    /// The matching public modulus / exponent, as Google would publish them.
    const TEST_N: &str = concat!(
        "tx9q8zOiQf3icYPO9sYyXwSx8uh82o99DP6-hxh5VUsVuFyQqYzLP6PbKm76LdqboYlClHfEaWz2",
        "2-skQpe0Xz_uWQaXLI6vHez-BR9pFtUr8PUTKV4s5AOUyUqQH2fyYwM8Lf1CFoMhaxVfwAB5Djy1",
        "Tj977MxAE39A1Ic6zln3c7sH7QV_iu4BFiLpiTGfZdPLaBxnauau-BqNohOuH-3A8rGXw7RZCp7S",
        "F9xrlnjOKqcgIRGWI4-gG628bPmmqFkDrOr7mrirVPoZYN5ntZS2zvZBFt3LxSym_RU63Hx9GkFc",
        "b4exbODp2foV4GRph_aRL4H1GEIxn7Aft_7K5w",
    );
    const TEST_E: &str = "AQAB";
    const TEST_KID: &str = "test-kid-1";

    const AUDIENCE: &str = "https://amicu-api-abc123-ew.a.run.app";
    const CALLER: &str = "amicu-scheduler@amicu-prod.iam.gserviceaccount.com";

    fn encoding_key() -> EncodingKey {
        let der = STANDARD
            .decode(TEST_RSA_PKCS1_DER_B64)
            .expect("embedded test key is valid base64");
        EncodingKey::from_rsa_der(&der)
    }

    fn now_unix() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_secs(),
        )
        .expect("fits in i64")
    }

    /// Mint a Google-shaped RS256 ID token from `claims`, signed with the test
    /// key under [`TEST_KID`].
    fn sign(claims: &serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(TEST_KID.to_string());
        jsonwebtoken::encode(&header, claims, &encoding_key()).expect("sign test token")
    }

    /// A well-formed token from the expected scheduler identity.
    fn good_claims() -> serde_json::Value {
        json!({
            "iss": "https://accounts.google.com",
            "aud": AUDIENCE,
            "sub": "1234567890",
            "exp": now_unix() + 3600,
            "iat": now_unix() - 10,
            "email": CALLER,
            "email_verified": true,
        })
    }

    /// An injected certs source: serves a fixed key set, counts fetches, and can
    /// be told to fail. No network, ever.
    struct FakeCerts {
        jwks: Mutex<RsaJwks>,
        fetches: AtomicUsize,
        fail: bool,
    }

    impl FakeCerts {
        fn with_test_key() -> Self {
            Self {
                jwks: Mutex::new(RsaJwks {
                    keys: vec![RsaJwk {
                        kid: TEST_KID.to_string(),
                        n: TEST_N.to_string(),
                        e: TEST_E.to_string(),
                    }],
                }),
                fetches: AtomicUsize::new(0),
                fail: false,
            }
        }

        fn empty() -> Self {
            Self {
                jwks: Mutex::new(RsaJwks::default()),
                fetches: AtomicUsize::new(0),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::empty()
            }
        }
    }

    #[async_trait]
    impl GoogleCertsSource for FakeCerts {
        async fn fetch(&self) -> Result<CachedCerts> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(AuthError::JwksFetch);
            }
            Ok(CachedCerts {
                jwks: self.jwks.lock().expect("fake certs poisoned").clone(),
                max_age: Some(Duration::from_secs(3600)),
            })
        }
    }

    /// How many times a verifier's fake source has been fetched.
    fn fetches(v: &GoogleIdTokenVerifier<FakeCerts>) -> usize {
        v.source().fetches.load(Ordering::SeqCst)
    }

    fn verifier(source: FakeCerts) -> GoogleIdTokenVerifier<FakeCerts> {
        GoogleIdTokenVerifier::new(
            source,
            GoogleIdTokenConfig::new(AUDIENCE, vec![CALLER.to_string()]),
        )
    }

    #[tokio::test]
    async fn accepts_a_well_formed_scheduler_token() {
        let claims = verifier(FakeCerts::with_test_key())
            .verify(&sign(&good_claims()))
            .await
            .expect("a correctly issued, audienced and allowlisted token must verify");
        assert_eq!(claims.aud, AUDIENCE);
        assert_eq!(claims.email.as_deref(), Some(CALLER));
    }

    /// The bare `accounts.google.com` spelling is equally valid.
    #[tokio::test]
    async fn accepts_the_bare_issuer_spelling() {
        let mut claims = good_claims();
        claims["iss"] = json!("accounts.google.com");
        verifier(FakeCerts::with_test_key())
            .verify(&sign(&claims))
            .await
            .expect("both documented issuer spellings must be accepted");
    }

    #[tokio::test]
    async fn rejects_a_wrong_audience() {
        let mut claims = good_claims();
        claims["aud"] = json!("https://someone-elses-service.a.run.app");
        let err = verifier(FakeCerts::with_test_key())
            .verify(&sign(&claims))
            .await
            .expect_err("a token minted for another audience must be refused");
        assert!(matches!(err, AuthError::AudienceRejected), "{err:?}");
    }

    #[tokio::test]
    async fn rejects_an_untrusted_issuer() {
        let mut claims = good_claims();
        claims["iss"] = json!("https://evil.example.test");
        let err = verifier(FakeCerts::with_test_key())
            .verify(&sign(&claims))
            .await
            .expect_err("only Google may issue these tokens");
        assert!(matches!(err, AuthError::IssuerRejected), "{err:?}");
    }

    /// A perfectly valid Google token from an identity we do not accept is a
    /// rejection of the CALLER - `Forbidden`, which the HTTP boundary renders
    /// 403, not 401.
    #[tokio::test]
    async fn rejects_an_unallowlisted_caller_email() {
        let mut claims = good_claims();
        claims["email"] = json!("someone-else@amicu-prod.iam.gserviceaccount.com");
        let err = verifier(FakeCerts::with_test_key())
            .verify(&sign(&claims))
            .await
            .expect_err("only the allowlisted scheduler identity may drain");
        assert!(matches!(err, AuthError::Forbidden), "{err:?}");
    }

    #[tokio::test]
    async fn rejects_an_unverified_or_absent_email() {
        let mut unverified = good_claims();
        unverified["email_verified"] = json!(false);
        assert!(matches!(
            verifier(FakeCerts::with_test_key())
                .verify(&sign(&unverified))
                .await,
            Err(AuthError::Forbidden)
        ));

        let mut absent = good_claims();
        absent
            .as_object_mut()
            .expect("object")
            .remove("email_verified");
        assert!(matches!(
            verifier(FakeCerts::with_test_key())
                .verify(&sign(&absent))
                .await,
            Err(AuthError::Forbidden)
        ));

        let mut no_email = good_claims();
        no_email.as_object_mut().expect("object").remove("email");
        assert!(matches!(
            verifier(FakeCerts::with_test_key())
                .verify(&sign(&no_email))
                .await,
            Err(AuthError::Forbidden)
        ));
    }

    /// Email comparison is case-insensitive and whitespace-tolerant, matching
    /// the allowlist normalization in [`GoogleIdTokenConfig::new`].
    #[tokio::test]
    async fn caller_email_comparison_is_case_insensitive() {
        let mut claims = good_claims();
        claims["email"] = json!(CALLER.to_ascii_uppercase());
        verifier(FakeCerts::with_test_key())
            .verify(&sign(&claims))
            .await
            .expect("case must not decide allowlist membership");
    }

    #[tokio::test]
    async fn rejects_an_expired_token() {
        let mut claims = good_claims();
        claims["exp"] = json!(now_unix() - 3600);
        let err = verifier(FakeCerts::with_test_key())
            .verify(&sign(&claims))
            .await
            .expect_err("an expired token must be refused");
        assert!(matches!(err, AuthError::Expired), "{err:?}");
    }

    /// An empty allowlist accepts NOBODY. A misconfigured deployment must fail
    /// closed, never open.
    #[tokio::test]
    async fn empty_allowlist_accepts_nobody() {
        let v = GoogleIdTokenVerifier::new(
            FakeCerts::with_test_key(),
            GoogleIdTokenConfig::new(AUDIENCE, Vec::new()),
        );
        assert!(matches!(
            v.verify(&sign(&good_claims())).await,
            Err(AuthError::Forbidden)
        ));
    }

    #[tokio::test]
    async fn rejects_a_kid_google_does_not_publish() {
        let err = verifier(FakeCerts::empty())
            .verify(&sign(&good_claims()))
            .await
            .expect_err("a kid outside the published key set must be refused");
        assert!(matches!(err, AuthError::UnknownKid), "{err:?}");
    }

    #[tokio::test]
    async fn surfaces_a_certs_fetch_failure() {
        let err = verifier(FakeCerts::failing())
            .verify(&sign(&good_claims()))
            .await
            .expect_err("no keys means no verification");
        assert!(matches!(err, AuthError::JwksFetch), "{err:?}");
    }

    #[tokio::test]
    async fn rejects_a_tampered_signature() {
        let token = sign(&good_claims());
        // Flip one character of the signature segment.
        let (body, sig) = token.rsplit_once('.').expect("three segments");
        let mut sig: Vec<char> = sig.chars().collect();
        sig[0] = if sig[0] == 'A' { 'B' } else { 'A' };
        let tampered = format!("{body}.{}", sig.into_iter().collect::<String>());
        let err = verifier(FakeCerts::with_test_key())
            .verify(&tampered)
            .await
            .expect_err("a tampered signature must be refused");
        assert!(matches!(err, AuthError::SignatureInvalid), "{err:?}");
    }

    /// An `alg` this verifier does not accept is refused up front - notably
    /// `none`, the classic algorithm-confusion attack.
    #[tokio::test]
    async fn rejects_a_non_rs256_algorithm() {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(TEST_KID.to_string());
        let token = jsonwebtoken::encode(&header, &good_claims(), &EncodingKey::from_secret(b"x"))
            .expect("sign HS256");
        let err = verifier(FakeCerts::with_test_key())
            .verify(&token)
            .await
            .expect_err("only RS256 is accepted");
        assert!(matches!(err, AuthError::InvalidToken), "{err:?}");
    }

    /// A cached, unexpired key set is reused: the second verify does not refetch
    /// (it would be one upstream request per drain otherwise).
    #[tokio::test]
    async fn caches_keys_between_verifications() {
        let v = verifier(FakeCerts::with_test_key());
        v.verify(&sign(&good_claims())).await.expect("first verify");
        v.verify(&sign(&good_claims()))
            .await
            .expect("second verify");
        assert_eq!(
            fetches(&v),
            1,
            "an unexpired key set must be served from cache"
        );
    }

    /// A flood of unknown `kid`s (attacker-controlled: read from an unverified
    /// header) must not amplify into one upstream fetch per request.
    #[tokio::test]
    async fn unknown_kid_refetches_are_cooldown_limited() {
        let v = verifier(FakeCerts::with_test_key());
        v.verify(&sign(&good_claims())).await.expect("prime cache");
        assert_eq!(fetches(&v), 1);

        for i in 0..20 {
            let mut header = Header::new(Algorithm::RS256);
            header.kid = Some(format!("bogus-{i}"));
            let token = jsonwebtoken::encode(&header, &good_claims(), &encoding_key())
                .expect("sign with a bogus kid");
            assert!(v.verify(&token).await.is_err());
        }
        assert_eq!(
            fetches(&v),
            2,
            "at most one unknown-kid refetch per cooldown window"
        );
    }

    #[test]
    fn parses_max_age_from_cache_control() {
        assert_eq!(
            parse_max_age("public, max-age=19621, must-revalidate, no-transform"),
            Some(Duration::from_secs(19_621))
        );
        assert_eq!(parse_max_age("max-age=60"), Some(Duration::from_secs(60)));
        assert_eq!(
            parse_max_age("Max-Age=\"90\""),
            Some(Duration::from_secs(90))
        );
    }

    #[test]
    fn missing_or_bad_max_age_is_none() {
        assert_eq!(parse_max_age("no-store"), None);
        assert_eq!(parse_max_age("max-age=soon"), None);
        assert_eq!(parse_max_age(""), None);
        // `s-maxage` is a different directive and must not be mistaken for it.
        assert_eq!(parse_max_age("s-maxage=100"), None);
    }

    #[test]
    fn config_normalizes_and_drops_empty_allowlist_entries() {
        let config = GoogleIdTokenConfig::new(
            "https://api.example.test",
            vec![
                "  Scheduler@Project.iam.gserviceaccount.com ".to_string(),
                "   ".to_string(),
            ],
        );
        assert_eq!(
            config.allowed_emails,
            vec!["scheduler@project.iam.gserviceaccount.com".to_string()]
        );
    }
}
