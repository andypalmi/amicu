//! Service-level acceptance: sign-up/in, refresh rotation, revocation, key
//! rotation, and the private-key-never-leaks invariant.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use amicu_auth::claims::Tier;
use amicu_auth::error::AuthError;
use amicu_auth::issuer::{Clock, IssuerConfig, TokenIssuer};
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::service::{AuthService, SignUpOutcome};
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verifier::{StaticJwksProvider, Verifier, VerifierConfig};
use amicu_core::secret::SecretValue;

const ISS: &str = "https://auth.amicu.dev";
const AUD: &str = "amicu-api";

#[derive(Clone)]
struct TestClock(Arc<AtomicI64>);
impl TestClock {
    fn at(t: i64) -> Self {
        Self(Arc::new(AtomicI64::new(t)))
    }
    fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn pw(s: &str) -> SecretValue {
    SecretValue::new(s.as_bytes().to_vec())
}

// Access-token verification checks exp/nbf against the real system clock, so
// service tests that verify a token must issue relative to real "now".
fn real_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn build(clock: TestClock) -> AuthService<TestClock> {
    let keyring = Keyring::new(SigningKey::generate().unwrap());
    let issuer = TokenIssuer::with_clock(IssuerConfig::new(ISS, AUD), clock);
    AuthService::new(
        keyring,
        issuer,
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        "https://auth.amicu.dev",
    )
}

fn verifier_for(svc: &AuthService<TestClock>) -> Verifier<StaticJwksProvider> {
    Verifier::new(
        StaticJwksProvider::new(svc.jwks()),
        VerifierConfig::new(ISS, AUD),
    )
}

#[tokio::test]
async fn sign_up_then_sign_in_issues_verifiable_token() {
    let clock = TestClock::at(real_now());
    let svc = build(clock);
    let id = match svc
        .sign_up("Alice@Example.com ", &pw("correct horse"))
        .await
        .unwrap()
    {
        SignUpOutcome::Created(id) => id,
        SignUpOutcome::AlreadyRegistered => panic!("first sign-up should create"),
    };

    let pair = svc
        .sign_in("alice@example.com", &pw("correct horse"))
        .await
        .unwrap();
    assert_eq!(pair.token_type, "Bearer");
    assert!(pair.expires_in > 0);

    let claims = verifier_for(&svc).verify(&pair.access_token).await.unwrap();
    assert_eq!(claims.sub, id);
    assert_eq!(claims.tier, Tier::Community);
}

#[tokio::test]
async fn wrong_password_and_unknown_user_rejected() {
    let svc = build(TestClock::at(real_now()));
    svc.sign_up("bob@example.com", &pw("s3cret-pass"))
        .await
        .unwrap();

    assert!(matches!(
        svc.sign_in("bob@example.com", &pw("wrong"))
            .await
            .unwrap_err(),
        AuthError::InvalidCredentials
    ));
    assert!(matches!(
        svc.sign_in("nobody@example.com", &pw("whatever"))
            .await
            .unwrap_err(),
        AuthError::InvalidCredentials
    ));
}

#[tokio::test]
async fn empty_credentials_rejected() {
    let svc = build(TestClock::at(real_now()));
    assert!(matches!(
        svc.sign_up("x@example.com", &pw("")).await.unwrap_err(),
        AuthError::InvalidCredentials
    ));
    assert!(matches!(
        svc.sign_up("", &pw("nonempty")).await.unwrap_err(),
        AuthError::InvalidCredentials
    ));
}

#[tokio::test]
async fn duplicate_sign_up_is_not_enumerable() {
    let svc = build(TestClock::at(real_now()));
    // First sign-up creates the account.
    assert!(matches!(
        svc.sign_up("dup@example.com", &pw("p1-password"))
            .await
            .unwrap(),
        SignUpOutcome::Created(_)
    ));
    // A second sign-up for the same email does NOT error (no enumeration): it
    // reports AlreadyRegistered, which the HTTP layer renders identically to a
    // fresh registration.
    assert_eq!(
        svc.sign_up("dup@example.com", &pw("p2-password"))
            .await
            .unwrap(),
        SignUpOutcome::AlreadyRegistered
    );
    // The original credentials still work; the duplicate did not overwrite.
    svc.sign_in("dup@example.com", &pw("p1-password"))
        .await
        .unwrap();
    // The duplicate's password was never installed.
    assert!(matches!(
        svc.sign_in("dup@example.com", &pw("p2-password"))
            .await
            .unwrap_err(),
        AuthError::InvalidCredentials
    ));
}

#[tokio::test]
async fn sign_up_rejects_weak_password() {
    let svc = build(TestClock::at(real_now()));
    assert!(matches!(
        svc.sign_up("weak@example.com", &pw("short"))
            .await
            .unwrap_err(),
        AuthError::WeakPassword
    ));
    // A weak password must not have created an account (so a subsequent strong
    // sign-up is a genuine Create, not an AlreadyRegistered).
    assert!(matches!(
        svc.sign_up("weak@example.com", &pw("strong-enough"))
            .await
            .unwrap(),
        SignUpOutcome::Created(_)
    ));
}

#[tokio::test]
async fn refresh_reuse_revokes_whole_family() {
    let svc = build(TestClock::at(real_now()));
    svc.sign_up("theft@example.com", &pw("family-secret"))
        .await
        .unwrap();
    let pair = svc
        .sign_in("theft@example.com", &pw("family-secret"))
        .await
        .unwrap();

    // Legitimate rotation: the presented token is revoked, a successor issued.
    let first = pair.refresh_token.clone();
    let rotated = svc.refresh(&first).await.unwrap();

    // An attacker replays the OLD (already-revoked) token -> reuse detected.
    assert!(matches!(
        svc.refresh(&first).await.unwrap_err(),
        AuthError::RefreshInvalid
    ));

    // Reuse detection revoked the WHOLE family: the victim's live successor
    // token (which had NOT been used yet) is now also dead.
    assert!(matches!(
        svc.refresh(&rotated.refresh_token).await.unwrap_err(),
        AuthError::RefreshInvalid
    ));
}

#[tokio::test]
async fn refresh_chain_rotates_across_multiple_hops() {
    let svc = build(TestClock::at(real_now()));
    svc.sign_up("chain@example.com", &pw("chain-secret"))
        .await
        .unwrap();
    let mut current = svc
        .sign_in("chain@example.com", &pw("chain-secret"))
        .await
        .unwrap()
        .refresh_token;
    // Several honest rotations in a row all succeed within one family.
    for _ in 0..4 {
        current = svc.refresh(&current).await.unwrap().refresh_token;
    }
    // Then a single replay of a stale token anywhere still trips detection.
    let stale = current.clone();
    let next = svc.refresh(&current).await.unwrap().refresh_token;
    assert!(matches!(
        svc.refresh(&stale).await.unwrap_err(),
        AuthError::RefreshInvalid
    ));
    // ...and it took down the family, so even the freshest token is dead.
    assert!(matches!(
        svc.refresh(&next).await.unwrap_err(),
        AuthError::RefreshInvalid
    ));
}

#[tokio::test]
async fn refresh_rotates_and_old_token_is_revoked() {
    let svc = build(TestClock::at(real_now()));
    svc.sign_up("rot@example.com", &pw("rotate-me-pw"))
        .await
        .unwrap();
    let pair = svc
        .sign_in("rot@example.com", &pw("rotate-me-pw"))
        .await
        .unwrap();

    let first_refresh = pair.refresh_token.clone();
    let rotated = svc.refresh(&first_refresh).await.unwrap();

    // The rotated access token is valid.
    verifier_for(&svc)
        .verify(&rotated.access_token)
        .await
        .unwrap();

    // The new refresh token works (honest rotation continues the family).
    svc.refresh(&rotated.refresh_token).await.unwrap();

    // Reusing the original, already-rotated token fails (it was revoked on
    // rotation). Family-wide revocation on reuse is covered separately by
    // `refresh_reuse_revokes_whole_family`.
    assert!(matches!(
        svc.refresh(&first_refresh).await.unwrap_err(),
        AuthError::RefreshInvalid
    ));
}

#[tokio::test]
async fn logout_revokes_refresh_token() {
    let svc = build(TestClock::at(real_now()));
    svc.sign_up("out@example.com", &pw("logout-pass"))
        .await
        .unwrap();
    let pair = svc
        .sign_in("out@example.com", &pw("logout-pass"))
        .await
        .unwrap();

    svc.logout(&pair.refresh_token).await.unwrap();
    assert!(matches!(
        svc.refresh(&pair.refresh_token).await.unwrap_err(),
        AuthError::RefreshInvalid
    ));
    // logout is idempotent.
    svc.logout(&pair.refresh_token).await.unwrap();
}

#[tokio::test]
async fn expired_refresh_token_rejected() {
    let clock = TestClock::at(real_now());
    let svc = build(clock.clone());
    svc.sign_up("exp@example.com", &pw("expire-pass"))
        .await
        .unwrap();
    let pair = svc
        .sign_in("exp@example.com", &pw("expire-pass"))
        .await
        .unwrap();

    // Advance beyond the 30-day refresh TTL.
    clock.advance(31 * 24 * 60 * 60);
    assert!(matches!(
        svc.refresh(&pair.refresh_token).await.unwrap_err(),
        AuthError::RefreshInvalid
    ));
}

#[tokio::test]
async fn key_rotation_publishes_two_kids_and_old_tokens_verify() {
    let svc = build(TestClock::at(real_now()));
    svc.sign_up("kr@example.com", &pw("keyrotate-pw"))
        .await
        .unwrap();
    let old_pair = svc
        .sign_in("kr@example.com", &pw("keyrotate-pw"))
        .await
        .unwrap();

    // Rotate in a new active signing key.
    svc.rotate_keys(SigningKey::generate().unwrap());
    let jwks = svc.jwks();
    assert_eq!(jwks.keys.len(), 2, "both kids published after rotation");

    // A verifier with the post-rotation JWKS still accepts the old-kid token.
    let verifier = Verifier::new(StaticJwksProvider::new(jwks), VerifierConfig::new(ISS, AUD));
    verifier.verify(&old_pair.access_token).await.unwrap();

    // New sign-ins use the new active key and also verify.
    let new_pair = svc
        .sign_in("kr@example.com", &pw("keyrotate-pw"))
        .await
        .unwrap();
    verifier_for(&svc)
        .verify(&new_pair.access_token)
        .await
        .unwrap();
}

#[tokio::test]
async fn private_key_never_appears_in_serialized_or_logged_output() {
    let svc = build(TestClock::at(real_now()));
    let key = SigningKey::generate().unwrap();

    // Debug is redacted.
    let dbg = format!("{key:?}");
    assert!(dbg.contains("redacted"));
    assert!(!dbg.contains("PRIVATE"));
    assert!(!dbg.contains("BEGIN"));

    // The JWKS carries only the public key: OKP `x`, never the private `d`.
    let json = serde_json::to_string(&svc.jwks()).unwrap();
    assert!(json.contains("\"kty\":\"OKP\""));
    assert!(json.contains("\"x\":"));
    assert!(
        !json.contains("\"d\":"),
        "JWKS must not contain a private scalar"
    );
    assert!(!json.contains("PRIVATE"));
}
