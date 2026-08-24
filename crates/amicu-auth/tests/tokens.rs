//! Acceptance tests: issue/verify, tampering, expiry/nbf, unknown-kid refetch,
//! rotation, and wrong aud/iss.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use amicu_auth::claims::Tier;
use amicu_auth::error::AuthError;
use amicu_auth::issuer::{Clock, IssuerConfig, TokenIssuer};
use amicu_auth::jwks::{Jwk, Jwks};
use amicu_auth::key::SigningKey;
use amicu_auth::verifier::{JwksProvider, StaticJwksProvider, Verifier, VerifierConfig};
use amicu_auth::{Role, WorkspaceId};
use async_trait::async_trait;

const ISS: &str = "https://auth.amicu.dev";
const AUD: &str = "amicu-api";

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        self.0
    }
}

fn jwks_of(keys: &[&SigningKey]) -> Jwks {
    Jwks {
        keys: keys.iter().map(|k| k.public_jwk()).collect::<Vec<Jwk>>(),
    }
}

/// Provider that serves a mutable JWKS and counts fetches.
struct CountingProvider {
    jwks: Mutex<Jwks>,
    fetches: AtomicUsize,
}

impl CountingProvider {
    fn new(jwks: Jwks) -> Self {
        Self {
            jwks: Mutex::new(jwks),
            fetches: AtomicUsize::new(0),
        }
    }
    fn set(&self, jwks: Jwks) {
        *self.jwks.lock().unwrap() = jwks;
    }
    fn count(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl JwksProvider for CountingProvider {
    async fn fetch(&self) -> Result<Jwks, AuthError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(self.jwks.lock().unwrap().clone())
    }
}

fn issuer() -> TokenIssuer {
    TokenIssuer::new(IssuerConfig::new(ISS, AUD))
}

fn verifier_config() -> VerifierConfig {
    VerifierConfig::new(ISS, AUD)
}

#[tokio::test]
async fn issue_and_verify_happy_path() {
    let key = SigningKey::generate().unwrap();
    let token = issuer()
        .issue_access(
            &key,
            "usr_1",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Pro,
            vec!["admin".into()],
        )
        .unwrap();

    let verifier = Verifier::new(StaticJwksProvider::new(jwks_of(&[&key])), verifier_config());
    let claims = verifier.verify(&token).await.unwrap();
    assert_eq!(claims.sub, "usr_1");
    assert_eq!(claims.tier, Tier::Pro);
    assert_eq!(claims.roles, vec!["admin".to_string()]);
    assert_eq!(claims.iss, ISS);
    assert_eq!(claims.aud, AUD);
}

#[tokio::test]
async fn tampered_signature_rejected() {
    let key = SigningKey::generate().unwrap();
    let token = issuer()
        .issue_access(
            &key,
            "usr_1",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Community,
            vec![],
        )
        .unwrap();

    // Flip the first char of the signature segment (a full byte) to keep the
    // token structurally valid but break the signature.
    let (head, sig) = token.rsplit_once('.').unwrap();
    let mut sig_chars: Vec<char> = sig.chars().collect();
    sig_chars[0] = if sig_chars[0] == 'A' { 'B' } else { 'A' };
    let tampered = format!("{head}.{}", sig_chars.into_iter().collect::<String>());

    let verifier = Verifier::new(StaticJwksProvider::new(jwks_of(&[&key])), verifier_config());
    let err = verifier.verify(&tampered).await.unwrap_err();
    assert!(matches!(err, AuthError::SignatureInvalid));
}

#[tokio::test]
async fn expired_token_rejected() {
    let key = SigningKey::generate().unwrap();
    // Sign at a fixed time far in the past so exp is already elapsed.
    let past = TokenIssuer::with_clock(IssuerConfig::new(ISS, AUD), FixedClock(1_000));
    let token = past
        .issue_access(
            &key,
            "usr_1",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Community,
            vec![],
        )
        .unwrap();

    let verifier = Verifier::new(StaticJwksProvider::new(jwks_of(&[&key])), verifier_config());
    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::Expired));
}

#[tokio::test]
async fn not_yet_valid_token_rejected() {
    let key = SigningKey::generate().unwrap();
    // Sign far in the future so nbf has not arrived.
    let future = TokenIssuer::with_clock(IssuerConfig::new(ISS, AUD), FixedClock(4_000_000_000));
    let token = future
        .issue_access(
            &key,
            "usr_1",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Community,
            vec![],
        )
        .unwrap();

    let verifier = Verifier::new(StaticJwksProvider::new(jwks_of(&[&key])), verifier_config());
    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::NotYetValid));
}

#[tokio::test]
async fn unknown_kid_triggers_single_refetch() {
    let key1 = SigningKey::generate().unwrap();
    let provider = CountingProvider::new(jwks_of(&[&key1]));
    let verifier = Verifier::new(provider, verifier_config());

    // First verify: cache empty -> 1 fetch.
    let t1 = issuer()
        .issue_access(
            &key1,
            "u",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Community,
            vec![],
        )
        .unwrap();
    verifier.verify(&t1).await.unwrap();
    assert_eq!(verifier.provider().count(), 1);

    // Same kid within TTL: no refetch.
    verifier.verify(&t1).await.unwrap();
    assert_eq!(verifier.provider().count(), 1);

    // New key/kid appears; a token with the unknown kid triggers one refetch.
    let key2 = SigningKey::generate().unwrap();
    verifier.provider().set(jwks_of(&[&key1, &key2]));
    let t2 = issuer()
        .issue_access(
            &key2,
            "u",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Community,
            vec![],
        )
        .unwrap();
    verifier.verify(&t2).await.unwrap();
    assert_eq!(verifier.provider().count(), 2);
}

#[tokio::test]
async fn key_rotation_old_kid_still_verifies() {
    let old = SigningKey::generate().unwrap();
    let new = SigningKey::generate().unwrap();
    assert_ne!(old.kid(), new.kid());

    // Token signed by the retiring key.
    let old_token = issuer()
        .issue_access(
            &old,
            "u",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Enterprise,
            vec![],
        )
        .unwrap();

    // JWKS publishes both kids after rotation.
    let jwks = jwks_of(&[&new, &old]);
    assert_eq!(jwks.keys.len(), 2);

    let verifier = Verifier::new(StaticJwksProvider::new(jwks), verifier_config());
    // Old-kid token still verifies until it expires.
    let claims = verifier.verify(&old_token).await.unwrap();
    assert_eq!(claims.tier, Tier::Enterprise);
    // New-kid token verifies too.
    let new_token = issuer()
        .issue_access(
            &new,
            "u",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Enterprise,
            vec![],
        )
        .unwrap();
    verifier.verify(&new_token).await.unwrap();
}

#[tokio::test]
async fn wrong_audience_rejected() {
    let key = SigningKey::generate().unwrap();
    let token = issuer()
        .issue_access(
            &key,
            "u",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Community,
            vec![],
        )
        .unwrap();

    let verifier = Verifier::new(
        StaticJwksProvider::new(jwks_of(&[&key])),
        VerifierConfig::new(ISS, "some-other-service"),
    );
    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::AudienceRejected));
}

#[tokio::test]
async fn wrong_issuer_rejected() {
    let key = SigningKey::generate().unwrap();
    let token = issuer()
        .issue_access(
            &key,
            "u",
            WorkspaceId::new(),
            Role::Owner,
            Tier::Community,
            vec![],
        )
        .unwrap();

    let verifier = Verifier::new(
        StaticJwksProvider::new(jwks_of(&[&key])),
        VerifierConfig::new("https://evil.example", AUD),
    );
    let err = verifier.verify(&token).await.unwrap_err();
    assert!(matches!(err, AuthError::IssuerRejected));
}
