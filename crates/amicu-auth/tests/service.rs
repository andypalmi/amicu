//! Service-level acceptance: sign-up/in, refresh rotation, revocation, key
//! rotation, and the private-key-never-leaks invariant.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use amicu_auth::claims::Tier;
use amicu_auth::error::AuthError;
use amicu_auth::issuer::{Clock, IssuerConfig, TokenIssuer};
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::membership::{MembershipContext, MembershipStore, WorkspaceMembership};
use amicu_auth::notify::{LoggingNotifier, Notification, NotificationKind, Notifier};
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::service::{AuthService, SignUpOutcome};
use amicu_auth::user::{InMemoryUserStore, User, UserStore};
use amicu_auth::verifier::{StaticJwksProvider, Verifier, VerifierConfig};
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_auth::{Role, WorkspaceId};
use amicu_core::secret::SecretValue;
use async_trait::async_trait;

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
    build_with_notifier(clock, Arc::new(LoggingNotifier))
}

fn build_with_notifier(clock: TestClock, notifier: Arc<dyn Notifier>) -> AuthService<TestClock> {
    build_with_users(clock, Arc::new(InMemoryUserStore::default()), notifier)
}

/// Build a service over a caller-provided user store, so a test can inspect the
/// stored `email_verified` flag directly (rather than through the env-gated
/// sign-in path, which would race the process-global env var across tests).
fn build_with_users(
    clock: TestClock,
    users: Arc<dyn UserStore>,
    notifier: Arc<dyn Notifier>,
) -> AuthService<TestClock> {
    let keyring = Keyring::new(SigningKey::generate().unwrap());
    let issuer = TokenIssuer::with_clock(IssuerConfig::new(ISS, AUD), clock);
    AuthService::new(
        keyring,
        issuer,
        users,
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        notifier,
        "https://auth.amicu.dev",
    )
}

/// A [`Notifier`] that records every message it is handed, so tests can assert
/// what was sent (and read out the reset token that a real mailer would embed in
/// a link).
#[derive(Default)]
struct CapturingNotifier {
    sent: std::sync::Mutex<Vec<(String, CapturedKind)>>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum CapturedKind {
    PasswordReset(String),
    VerifyEmail(String),
    AccountExists,
}

impl CapturingNotifier {
    fn captured(&self) -> Vec<(String, CapturedKind)> {
        self.sent.lock().unwrap().clone()
    }

    /// Wait for, then return, the token from the most recent password-reset
    /// message.
    async fn last_reset_token(&self) -> SecretValue {
        for _ in 0..10_000 {
            if let Some(t) = self
                .sent
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find_map(|(_, k)| match k {
                    CapturedKind::PasswordReset(t) => Some(t.clone()),
                    CapturedKind::VerifyEmail(_) | CapturedKind::AccountExists => None,
                })
            {
                return SecretValue::new(t.into_bytes());
            }
            tokio::task::yield_now().await;
        }
        panic!("timed out waiting for a password-reset notification");
    }

    /// Wait for, then return, the token from the most recent email-verification
    /// message.
    async fn last_verify_token(&self) -> SecretValue {
        for _ in 0..10_000 {
            if let Some(t) = self
                .sent
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find_map(|(_, k)| match k {
                    CapturedKind::VerifyEmail(t) => Some(t.clone()),
                    CapturedKind::PasswordReset(_) | CapturedKind::AccountExists => None,
                })
            {
                return SecretValue::new(t.into_bytes());
            }
            tokio::task::yield_now().await;
        }
        panic!("timed out waiting for an email-verification notification");
    }

    /// Count captured messages of a given predicate.
    fn count(&self, pred: impl Fn(&CapturedKind) -> bool) -> usize {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, k)| pred(k))
            .count()
    }
}

#[async_trait]
impl Notifier for CapturingNotifier {
    async fn notify(&self, message: Notification) -> Result<(), AuthError> {
        let kind = match message.kind {
            NotificationKind::PasswordReset { reset_token } => {
                CapturedKind::PasswordReset(reset_token.expose_secret_str().unwrap().to_string())
            }
            NotificationKind::VerifyEmail { verify_token } => {
                CapturedKind::VerifyEmail(verify_token.expose_secret_str().unwrap().to_string())
            }
            NotificationKind::AccountExists => CapturedKind::AccountExists,
        };
        self.sent.lock().unwrap().push((message.recipient, kind));
        Ok(())
    }
}

#[tokio::test]
async fn password_reset_happy_path_updates_password_and_revokes_sessions() {
    let notifier = Arc::new(CapturingNotifier::default());
    let svc = build_with_notifier(TestClock::at(real_now()), notifier.clone());
    svc.sign_up("reset@example.com", &pw("old-password"))
        .await
        .unwrap();
    // Establish a live session whose refresh token must die on reset.
    let session = svc
        .sign_in("reset@example.com", &pw("old-password"))
        .await
        .unwrap();

    // Request a reset; the token is delivered out-of-band on a detached task
    // (captured here once it lands).
    svc.request_password_reset("Reset@Example.com ")
        .await
        .unwrap();
    let token = notifier.last_reset_token().await;

    // Consume it to set a new password.
    svc.reset_password(&token, &pw("brand-new-password"))
        .await
        .unwrap();

    // New password works; old one no longer does.
    svc.sign_in("reset@example.com", &pw("brand-new-password"))
        .await
        .unwrap();
    assert!(matches!(
        svc.sign_in("reset@example.com", &pw("old-password"))
            .await
            .unwrap_err(),
        AuthError::InvalidCredentials
    ));

    // The pre-reset session's refresh token was revoked (forced re-login).
    assert!(matches!(
        svc.refresh(&session.refresh_token).await.unwrap_err(),
        AuthError::RefreshInvalid
    ));
}

#[tokio::test]
async fn reset_token_is_single_use() {
    let notifier = Arc::new(CapturingNotifier::default());
    let svc = build_with_notifier(TestClock::at(real_now()), notifier.clone());
    svc.sign_up("once@example.com", &pw("first-password"))
        .await
        .unwrap();
    svc.request_password_reset("once@example.com")
        .await
        .unwrap();
    let token = notifier.last_reset_token().await;

    svc.reset_password(&token, &pw("second-password"))
        .await
        .unwrap();
    // A replay of the same token is rejected generically.
    assert!(matches!(
        svc.reset_password(&token, &pw("third-password"))
            .await
            .unwrap_err(),
        AuthError::ResetInvalid
    ));
}

#[tokio::test]
async fn expired_reset_token_is_rejected() {
    let notifier = Arc::new(CapturingNotifier::default());
    let clock = TestClock::at(real_now());
    let svc = build_with_notifier(clock.clone(), notifier.clone());
    svc.sign_up("slow@example.com", &pw("slow-password"))
        .await
        .unwrap();
    svc.request_password_reset("slow@example.com")
        .await
        .unwrap();
    let token = notifier.last_reset_token().await;

    // Advance past the 1-hour reset TTL.
    clock.advance(3_600 + 1);
    assert!(matches!(
        svc.reset_password(&token, &pw("too-late-password"))
            .await
            .unwrap_err(),
        AuthError::ResetInvalid
    ));
}

#[tokio::test]
async fn unknown_reset_token_is_rejected() {
    let svc = build(TestClock::at(real_now()));
    let bogus = SecretValue::new(b"not-a-real-reset-token".to_vec());
    assert!(matches!(
        svc.reset_password(&bogus, &pw("whatever-password"))
            .await
            .unwrap_err(),
        AuthError::ResetInvalid
    ));
}

#[tokio::test]
async fn reset_enforces_password_policy() {
    let notifier = Arc::new(CapturingNotifier::default());
    let svc = build_with_notifier(TestClock::at(real_now()), notifier.clone());
    svc.sign_up("weakreset@example.com", &pw("old-password"))
        .await
        .unwrap();
    svc.request_password_reset("weakreset@example.com")
        .await
        .unwrap();
    let token = notifier.last_reset_token().await;

    // A weak new password is rejected up front (and must not burn the token).
    assert!(matches!(
        svc.reset_password(&token, &pw("short")).await.unwrap_err(),
        AuthError::WeakPassword
    ));
    // The token survives the weak-password rejection and still works.
    svc.reset_password(&token, &pw("strong-enough-now"))
        .await
        .unwrap();
}

#[tokio::test]
async fn recover_notifies_only_when_account_exists() {
    let notifier = Arc::new(CapturingNotifier::default());
    let svc = build_with_notifier(TestClock::at(real_now()), notifier.clone());
    svc.sign_up("real@example.com", &pw("real-password"))
        .await
        .unwrap();

    // Existing account: a password-reset notification is dispatched (on a
    // detached task).
    svc.request_password_reset("real@example.com")
        .await
        .unwrap();
    // Unknown account: still Ok (anti-enumeration); its detached task looks up
    // the (missing) account and sends nothing.
    svc.request_password_reset("ghost@example.com")
        .await
        .unwrap();

    // Wait for the real account's reset message to land (sign-up also emits a
    // verify notification on its own detached task; we count only resets here).
    let _ = notifier.last_reset_token().await;
    let resets: Vec<_> = notifier
        .captured()
        .into_iter()
        .filter(|(_, k)| matches!(k, CapturedKind::PasswordReset(_)))
        .collect();
    assert_eq!(
        resets.len(),
        1,
        "exactly one reset message: only the real account"
    );
    assert_eq!(resets[0].0, "real@example.com");
}

#[tokio::test]
async fn resend_verification_notifies_only_unverified_existing_accounts() {
    let notifier = Arc::new(CapturingNotifier::default());
    // A caller-provided store so we can seed accounts directly (bypassing
    // sign-up, which would itself emit a verification link and muddy the count)
    // and mark one already-verified.
    let users = Arc::new(InMemoryUserStore::default());
    let svc = build_with_users(TestClock::at(real_now()), users.clone(), notifier.clone());

    // An existing, still-unverified account.
    users
        .create(User::new(
            "usr_unverified".to_string(),
            "unverified@example.com".to_string(),
            "hash".to_string(),
            Tier::Community,
        ))
        .await
        .unwrap();
    // An existing account whose email is already verified.
    users
        .create(User::new(
            "usr_verified".to_string(),
            "verified@example.com".to_string(),
            "hash".to_string(),
            Tier::Community,
        ))
        .await
        .unwrap();
    users.set_email_verified("usr_verified").await.unwrap();

    // All three requests return the SAME Ok outcome (anti-enumeration): the HTTP
    // boundary renders one identical response regardless.
    svc.resend_verification("Unverified@Example.com ") // existing + unverified
        .await
        .unwrap();
    svc.resend_verification("verified@example.com") // existing + already verified
        .await
        .unwrap();
    svc.resend_verification("ghost@example.com") // unknown account
        .await
        .unwrap();

    // Wait for the one account that warrants a link to receive it; this positive
    // fence also gives the other two detached tasks ample time to have run (and
    // sent nothing).
    let _ = notifier.last_verify_token().await;
    let verifies: Vec<_> = notifier
        .captured()
        .into_iter()
        .filter(|(_, k)| matches!(k, CapturedKind::VerifyEmail(_)))
        .collect();
    assert_eq!(
        verifies.len(),
        1,
        "exactly one verification link: only the existing unverified account \
         (already-verified and unknown accounts send nothing)"
    );
    // Normalized recipient: the mixed-case/whitespace input is lowercased/trimmed.
    assert_eq!(verifies[0].0, "unverified@example.com");
}

#[tokio::test]
async fn duplicate_signup_notifies_account_owner() {
    let notifier = Arc::new(CapturingNotifier::default());
    let svc = build_with_notifier(TestClock::at(real_now()), notifier.clone());
    // First sign-up creates: it emits a verification link (detached), but never
    // an account-exists notice.
    svc.sign_up("owner@example.com", &pw("owner-password"))
        .await
        .unwrap();
    let _ = notifier.last_verify_token().await; // the created account's verify link
    assert_eq!(
        notifier.count(|k| matches!(k, CapturedKind::AccountExists)),
        0,
        "a fresh account must not get an account-exists notice"
    );

    // A duplicate sign-up nudges the real owner out-of-band (detached task) and
    // emits NO verification link.
    svc.sign_up("owner@example.com", &pw("attacker-guess"))
        .await
        .unwrap();
    for _ in 0..10_000 {
        if notifier.count(|k| matches!(k, CapturedKind::AccountExists)) >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    let existing: Vec<_> = notifier
        .captured()
        .into_iter()
        .filter(|(_, k)| matches!(k, CapturedKind::AccountExists))
        .collect();
    assert_eq!(existing.len(), 1, "exactly one account-exists notice");
    assert_eq!(existing[0].0, "owner@example.com");
    // The duplicate emitted exactly one verify link total (from the first, real
    // sign-up); the collision path never mints another.
    assert_eq!(
        notifier.count(|k| matches!(k, CapturedKind::VerifyEmail(_))),
        1,
        "only the created account emits a verification link"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_refresh_yields_exactly_one_successor() {
    // Two tasks racing to rotate the SAME valid token must not both succeed:
    // the compare-and-revoke is atomic, so exactly one wins and the other is
    // rejected (tripping family compromise). Never two live successors.
    //
    // Looped to actually exercise the interleaving window under real
    // parallelism - the pre-fix `get`/check/`revoke` split would let some
    // iteration mint two live successors.
    let svc = Arc::new(build(TestClock::at(real_now())));
    svc.sign_up("race@example.com", &pw("race-secret-pw"))
        .await
        .unwrap();

    for _ in 0..16 {
        // A fresh independent family per iteration.
        let token = svc
            .sign_in("race@example.com", &pw("race-secret-pw"))
            .await
            .unwrap()
            .refresh_token;

        let (s1, t1) = (svc.clone(), token.clone());
        let (s2, t2) = (svc.clone(), token.clone());
        let h1 = tokio::spawn(async move { s1.refresh(&t1).await });
        let h2 = tokio::spawn(async move { s2.refresh(&t2).await });
        let (r1, r2) = (h1.await.unwrap(), h2.await.unwrap());

        let successors: Vec<_> = [r1, r2].into_iter().filter_map(Result::ok).collect();
        assert_eq!(
            successors.len(),
            1,
            "exactly one concurrent refresh may succeed, never two live successors"
        );
        // The single surviving successor is itself dead: the losing racer
        // flagged the family compromised, so even the winner's token no longer
        // rotates (consume rejects any token in a compromised family).
        assert!(matches!(
            svc.refresh(&successors[0].refresh_token).await.unwrap_err(),
            AuthError::RefreshInvalid
        ));
    }
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
async fn sign_up_issues_verification_and_verify_email_flips_flag_once() {
    let notifier = Arc::new(CapturingNotifier::default());
    // Keep a handle to the user store so we can read `email_verified` directly,
    // sidestepping the process-global env gate (which would race other tests).
    let users = Arc::new(InMemoryUserStore::default());
    let svc = build_with_users(TestClock::at(real_now()), users.clone(), notifier.clone());

    svc.sign_up("verify@example.com", &pw("verify-me-please"))
        .await
        .unwrap();
    // The verification link is dispatched on a detached task; grab its token.
    let token = notifier.last_verify_token().await;

    // The flag starts false.
    let before = users
        .get_by_email("verify@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(!before.email_verified);

    // Consuming the token flips the flag.
    svc.verify_email(&token).await.unwrap();
    let after = users
        .get_by_email("verify@example.com")
        .await
        .unwrap()
        .unwrap();
    assert!(after.email_verified);

    // Single-use: replaying the same token is rejected generically.
    assert!(matches!(
        svc.verify_email(&token).await.unwrap_err(),
        AuthError::VerifyInvalid
    ));

    // An entirely unknown token is likewise rejected generically.
    assert!(matches!(
        svc.verify_email(&SecretValue::new(b"not-a-real-verify-token".to_vec()))
            .await
            .unwrap_err(),
        AuthError::VerifyInvalid
    ));
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

// ---------------------------------------------------------------------------
// andypalmi/amicu-planning#129: workspace claim in tokens.
//
// A controllable membership authority so a test can seed / mutate / revoke a
// user's membership in an arbitrary workspace and assert the issuer's
// server-side resolution, switch authorization, and refresh re-validation.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockMembership {
    // (workspace, user) -> (role, workspace tier)
    rows: Mutex<HashMap<(WorkspaceId, String), (Role, Tier)>>,
}

impl MockMembership {
    fn set(&self, ws: WorkspaceId, user: &str, role: Role, tier: Tier) {
        self.rows
            .lock()
            .unwrap()
            .insert((ws, user.to_string()), (role, tier));
    }
    fn remove(&self, ws: WorkspaceId, user: &str) {
        self.rows.lock().unwrap().remove(&(ws, user.to_string()));
    }
}

#[async_trait]
impl MembershipStore for MockMembership {
    async fn context(
        &self,
        workspace: WorkspaceId,
        user_id: &str,
    ) -> Result<Option<MembershipContext>, AuthError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .get(&(workspace, user_id.to_string()))
            .map(|&(role, tier)| MembershipContext { role, tier }))
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<WorkspaceMembership>, AuthError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|((_, u), _)| u == user_id)
            .map(|((ws, _), &(role, _))| WorkspaceMembership {
                workspace_id: *ws,
                name: "workspace".to_string(),
                role,
            })
            .collect())
    }
}

fn build_with_memberships(
    clock: TestClock,
    users: Arc<dyn UserStore>,
    memberships: Arc<dyn MembershipStore>,
) -> AuthService<TestClock> {
    let keyring = Keyring::new(SigningKey::generate().unwrap());
    let issuer = TokenIssuer::with_clock(IssuerConfig::new(ISS, AUD), clock);
    AuthService::new(
        keyring,
        issuer,
        users,
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        "https://auth.amicu.dev",
    )
    .with_membership_store(memberships)
}

/// A fresh sign-in mints a token scoped to the user's PERSONAL workspace, as
/// Owner, carrying that workspace's tier - all resolved server-side.
#[tokio::test]
async fn sign_in_scopes_token_to_personal_workspace_as_owner() {
    let users = Arc::new(InMemoryUserStore::default());
    let svc = build_with_users(
        TestClock::at(real_now()),
        users.clone(),
        Arc::new(LoggingNotifier),
    );
    svc.sign_up("solo@example.com", &pw("correct horse staple"))
        .await
        .unwrap();
    let user = users
        .get_by_email("solo@example.com")
        .await
        .unwrap()
        .expect("user present");

    let pair = svc
        .sign_in("solo@example.com", &pw("correct horse staple"))
        .await
        .unwrap();
    let claims = verifier_for(&svc).verify(&pair.access_token).await.unwrap();

    assert_eq!(claims.wsid, user.personal_workspace_id);
    assert_eq!(claims.wsrole, Role::Owner);
    assert_eq!(claims.tier, Tier::Community);
}

/// Switching to a workspace you are NOT a member of is refused (403-mapped
/// `Forbidden`) and mints no token; switching to one you ARE a member of mints a
/// token carrying that workspace's role and tier - both re-checked server-side.
#[tokio::test]
async fn switch_authorization_is_membership_bound() {
    let users = Arc::new(InMemoryUserStore::default());
    let mock = Arc::new(MockMembership::default());
    let svc = build_with_memberships(TestClock::at(real_now()), users.clone(), mock.clone());

    let id = match svc
        .sign_up("multi@example.com", &pw("correct horse staple"))
        .await
        .unwrap()
    {
        SignUpOutcome::Created(id) => id,
        SignUpOutcome::AlreadyRegistered => panic!("first sign-up should create"),
    };
    let personal = users
        .get_by_email("multi@example.com")
        .await
        .unwrap()
        .unwrap()
        .personal_workspace_id;
    mock.set(personal, &id, Role::Owner, Tier::Community);

    // A workspace the user is NOT a member of: refused, no token.
    let stranger = WorkspaceId::new();
    assert!(matches!(
        svc.switch_workspace(&id, stranger).await,
        Err(AuthError::Forbidden)
    ));

    // A workspace the user IS a member of (Admin, Pro): mints a scoped token.
    let shared = WorkspaceId::new();
    mock.set(shared, &id, Role::Admin, Tier::Pro);
    let pair = svc.switch_workspace(&id, shared).await.unwrap();
    let claims = verifier_for(&svc).verify(&pair.access_token).await.unwrap();
    assert_eq!(claims.wsid, shared);
    assert_eq!(claims.wsrole, Role::Admin);
    assert_eq!(claims.tier, Tier::Pro);
}

/// Refresh rotation re-validates membership in the token's workspace: a role
/// change is reflected in the new token; a removal fails the refresh outright
/// (the session dies within the access TTL).
#[tokio::test]
async fn refresh_revalidates_membership_on_rotation() {
    let users = Arc::new(InMemoryUserStore::default());
    let mock = Arc::new(MockMembership::default());
    let svc = build_with_memberships(TestClock::at(real_now()), users.clone(), mock.clone());

    let id = match svc
        .sign_up("rot@example.com", &pw("correct horse staple"))
        .await
        .unwrap()
    {
        SignUpOutcome::Created(id) => id,
        SignUpOutcome::AlreadyRegistered => panic!("first sign-up should create"),
    };
    let ws = WorkspaceId::new();
    mock.set(ws, &id, Role::Member, Tier::Pro);

    // Open a session scoped to `ws` (Member).
    let pair = svc.switch_workspace(&id, ws).await.unwrap();
    let c0 = verifier_for(&svc).verify(&pair.access_token).await.unwrap();
    assert_eq!(c0.wsrole, Role::Member);

    // Role change: the rotated token carries the NEW role/tier, same workspace.
    mock.set(ws, &id, Role::Owner, Tier::Enterprise);
    let rotated = svc.refresh(&pair.refresh_token).await.unwrap();
    let c1 = verifier_for(&svc)
        .verify(&rotated.access_token)
        .await
        .unwrap();
    assert_eq!(c1.wsid, ws);
    assert_eq!(c1.wsrole, Role::Owner);
    assert_eq!(c1.tier, Tier::Enterprise);

    // Removal: the next refresh of this session fails (removed member).
    mock.remove(ws, &id);
    assert!(matches!(
        svc.refresh(&rotated.refresh_token).await,
        Err(AuthError::RefreshInvalid)
    ));
}

/// `list_workspaces` returns exactly the caller's own memberships (id + role).
#[tokio::test]
async fn list_workspaces_returns_only_callers_memberships() {
    let users = Arc::new(InMemoryUserStore::default());
    let mock = Arc::new(MockMembership::default());
    let svc = build_with_memberships(TestClock::at(real_now()), users.clone(), mock.clone());

    let id = match svc
        .sign_up("lister@example.com", &pw("correct horse staple"))
        .await
        .unwrap()
    {
        SignUpOutcome::Created(id) => id,
        SignUpOutcome::AlreadyRegistered => panic!("first sign-up should create"),
    };
    let (a, b) = (WorkspaceId::new(), WorkspaceId::new());
    mock.set(a, &id, Role::Owner, Tier::Community);
    mock.set(b, &id, Role::Viewer, Tier::Pro);
    // Another user's membership must never appear in this caller's list.
    mock.set(WorkspaceId::new(), "someone-else", Role::Owner, Tier::Pro);

    let mut mine = svc.list_workspaces(&id).await.unwrap();
    mine.sort_by_key(|m| m.workspace_id);
    assert_eq!(mine.len(), 2);
    let ids: Vec<_> = mine.iter().map(|m| m.workspace_id).collect();
    assert!(ids.contains(&a) && ids.contains(&b));
}
