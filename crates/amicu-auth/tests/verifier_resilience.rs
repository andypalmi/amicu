//! Verifier resilience (amicu-planning#95): bounded refetch on unknown `kid`s
//! and serve-stale-on-failure when a JWKS refetch blips.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use amicu_auth::claims::Tier;
use amicu_auth::error::AuthError;
use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::jwks::{Jwk, Jwks};
use amicu_auth::key::SigningKey;
use amicu_auth::verifier::{JwksProvider, MonoClock, Verifier, VerifierConfig};
use async_trait::async_trait;

const ISS: &str = "https://auth.amicu.dev";
const AUD: &str = "amicu-api";

/// Advanceable monotonic clock — deterministic, no real sleeps. Cloned into the
/// verifier while the test keeps a handle to advance it.
#[derive(Clone)]
struct TestClock {
    base: Instant,
    offset_ms: Arc<AtomicU64>,
}
impl TestClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            offset_ms: Arc::new(AtomicU64::new(0)),
        }
    }
    fn advance(&self, d: Duration) {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        self.offset_ms.fetch_add(ms, Ordering::SeqCst);
    }
}
impl MonoClock for TestClock {
    fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
    }
}

/// Provider that serves a JWKS, counts fetches, and can be toggled to fail.
struct FlakyProvider {
    jwks: Mutex<Jwks>,
    fetches: AtomicUsize,
    fail: AtomicBool,
}
impl FlakyProvider {
    fn new(jwks: Jwks) -> Self {
        Self {
            jwks: Mutex::new(jwks),
            fetches: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }
    }
    fn set_fail(&self, v: bool) {
        self.fail.store(v, Ordering::SeqCst);
    }
    fn count(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl JwksProvider for FlakyProvider {
    async fn fetch(&self) -> Result<Jwks, AuthError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(AuthError::JwksFetch);
        }
        Ok(self.jwks.lock().unwrap().clone())
    }
}

fn jwks_of(keys: &[&SigningKey]) -> Jwks {
    Jwks {
        keys: keys.iter().map(|k| k.public_jwk()).collect::<Vec<Jwk>>(),
    }
}

// Tokens verify exp/nbf against the real system clock, so issue at real "now".
fn issuer() -> TokenIssuer {
    TokenIssuer::new(IssuerConfig::new(ISS, AUD))
}

fn token_for(key: &SigningKey) -> String {
    issuer()
        .issue_access(key, "u", Tier::Community, vec![])
        .unwrap()
}

/// A perpetually-unknown `kid` triggers at most one refetch per cooldown window.
#[tokio::test]
async fn unknown_kid_refetches_at_most_once_per_cooldown() {
    let served = SigningKey::generate().unwrap();
    let provider = FlakyProvider::new(jwks_of(&[&served]));
    let clock = TestClock::new();
    let verifier = Verifier::with_clock(provider, VerifierConfig::new(ISS, AUD), clock.clone());

    // Prime the cache with the one served key (mandatory first fetch).
    verifier.verify(&token_for(&served)).await.unwrap();
    assert_eq!(verifier.provider().count(), 1);

    // First unknown `kid`: one refetch (which does not resolve it) -> rejected.
    let ghost1 = token_for(&SigningKey::generate().unwrap());
    assert!(matches!(
        verifier.verify(&ghost1).await.unwrap_err(),
        AuthError::UnknownKid
    ));
    assert_eq!(
        verifier.provider().count(),
        2,
        "first unknown kid refetches"
    );

    // More unknown `kid`s within the cooldown: no further upstream fetches.
    for _ in 0..5 {
        let ghost = token_for(&SigningKey::generate().unwrap());
        assert!(matches!(
            verifier.verify(&ghost).await.unwrap_err(),
            AuthError::UnknownKid
        ));
    }
    assert_eq!(
        verifier.provider().count(),
        2,
        "unknown kids within cooldown must not refetch"
    );

    // After the cooldown elapses (still within cache TTL), one more is allowed.
    clock.advance(Duration::from_secs(61));
    let ghost_after = token_for(&SigningKey::generate().unwrap());
    assert!(matches!(
        verifier.verify(&ghost_after).await.unwrap_err(),
        AuthError::UnknownKid
    ));
    assert_eq!(
        verifier.provider().count(),
        3,
        "cooldown elapsed -> exactly one more refetch"
    );
}

/// A transient provider error must not reject a token whose `kid` is cached and
/// still valid: the stale-but-usable key is served.
#[tokio::test]
async fn transient_error_serves_stale_for_cached_kid() {
    let key = SigningKey::generate().unwrap();
    let provider = FlakyProvider::new(jwks_of(&[&key]));
    let clock = TestClock::new();
    let verifier = Verifier::with_clock(provider, VerifierConfig::new(ISS, AUD), clock.clone());

    let token = issuer()
        .issue_access(&key, "u", Tier::Pro, vec!["admin".into()])
        .unwrap();

    // Prime the cache successfully.
    verifier.verify(&token).await.unwrap();
    assert_eq!(verifier.provider().count(), 1);

    // Expire the cache (TTL 300s) and make the provider start failing.
    clock.advance(Duration::from_secs(301));
    verifier.provider().set_fail(true);

    // A refetch is attempted and fails, but the cached key still verifies.
    let claims = verifier.verify(&token).await.unwrap();
    assert_eq!(claims.sub, "u");
    assert_eq!(claims.tier, Tier::Pro);
    assert!(
        verifier.provider().count() >= 2,
        "a refetch was attempted on the stale cache"
    );
}

/// When a refetch fails and no usable key is cached, verification hard-fails
/// (fail-closed on availability; there is nothing safe to serve).
#[tokio::test]
async fn refetch_failure_with_no_cached_key_hard_fails() {
    let key = SigningKey::generate().unwrap();
    let provider = FlakyProvider::new(jwks_of(&[&key]));
    provider.set_fail(true);
    let verifier = Verifier::with_clock(provider, VerifierConfig::new(ISS, AUD), TestClock::new());

    // Empty cache + failing provider -> no key to serve -> fetch error surfaces.
    let err = verifier.verify(&token_for(&key)).await.unwrap_err();
    assert!(matches!(err, AuthError::JwksFetch));
}

/// A *concurrent* flood of unknown `kid`s triggers at most one refetch per
/// cooldown window: the cooldown check-and-set is atomic, so parallel tasks
/// cannot all observe the slot as free before any claims it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_unknown_kids_refetch_at_most_once_per_cooldown() {
    let served = SigningKey::generate().unwrap();
    let provider = FlakyProvider::new(jwks_of(&[&served]));
    let verifier = Arc::new(Verifier::with_clock(
        provider,
        VerifierConfig::new(ISS, AUD),
        TestClock::new(),
    ));

    // Prime the cache (mandatory first fetch is not the unknown-kid path).
    verifier.verify(&token_for(&served)).await.unwrap();
    assert_eq!(verifier.provider().count(), 1);

    // Fire a burst of concurrent verifies for the same perpetually-unknown kid.
    let ghost = token_for(&SigningKey::generate().unwrap());
    let mut handles = Vec::new();
    for _ in 0..64 {
        let v = Arc::clone(&verifier);
        let t = ghost.clone();
        handles.push(tokio::spawn(async move {
            // All are rejected (the kid never resolves); we only assert on the
            // number of upstream fetches the burst provoked.
            assert!(matches!(v.verify(&t).await, Err(AuthError::UnknownKid)));
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(
        verifier.provider().count(),
        2,
        "a concurrent unknown-kid flood must cause at most one refetch per window"
    );
}

/// Serve-stale has a ceiling: once the cache ages past `max_stale` and the
/// provider is still failing, even a cached `kid` is refused (fail closed), so a
/// key rotated out / revoked during a prolonged outage stops being honoured.
#[tokio::test]
async fn cached_key_past_max_stale_is_rejected() {
    let key = SigningKey::generate().unwrap();
    let provider = FlakyProvider::new(jwks_of(&[&key]));
    let clock = TestClock::new();
    let cfg = VerifierConfig::new(ISS, AUD);
    let max_stale = cfg.max_stale;
    let verifier = Verifier::with_clock(provider, cfg, clock.clone());

    let token = token_for(&key);
    verifier.verify(&token).await.unwrap();
    assert_eq!(verifier.provider().count(), 1);

    // Age the cache just past max_stale and keep the provider down.
    clock.advance(max_stale + Duration::from_secs(1));
    verifier.provider().set_fail(true);

    // The kid is still cached, but the cache is too old to trust: fail closed.
    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::JwksFetch));
}
