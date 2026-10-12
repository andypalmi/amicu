//! Sign-in backoff and security events at the service layer
//! (andypalmi/amicu-planning#194).

use std::sync::Arc;

use amicu_auth::error::{AuthError, Result};
use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::password::{MAX_PASSWORD_LEN, check_password_policy};
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::security::testing::capture;
use amicu_auth::service::AuthService;
use amicu_auth::throttle::{Admission, LoginThrottle};
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_core::secret::SecretValue;
use async_trait::async_trait;

fn service(throttle: Option<Arc<dyn LoginThrottle>>) -> AuthService {
    let svc = AuthService::new(
        Keyring::new(SigningKey::generate().unwrap()),
        TokenIssuer::new(IssuerConfig::new("https://auth.amicu.dev", "amicu-api")),
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        "https://auth.amicu.dev",
    );
    match throttle {
        Some(t) => svc.with_login_throttle(t),
        None => svc,
    }
}

fn pw(s: &str) -> SecretValue {
    SecretValue::new(s.as_bytes().to_vec())
}

struct BrokenThrottle;

#[async_trait]
impl LoginThrottle for BrokenThrottle {
    async fn begin_attempt(&self, _: &str) -> Result<Admission> {
        Err(AuthError::Backend)
    }
    async fn clear(&self, _: &str) -> Result<()> {
        Err(AuthError::Backend)
    }
}

#[tokio::test]
async fn throttle_failure_fails_open_and_emits_an_event() {
    let (events, _guard) = capture();
    let svc = service(Some(Arc::new(BrokenThrottle)));
    svc.sign_up("u@example.com", &pw("correct horse battery"))
        .await
        .unwrap();
    // Sign-in still works (fail open) ...
    svc.sign_in("u@example.com", &pw("correct horse battery"))
        .await
        .expect("sign-in must not depend on the throttle store");
    // ... and the degradation is visible to operators.
    let evs = events.security_events();
    assert!(
        evs.iter()
            .any(|e| e.name() == "login_throttle_unavailable" && e.get("reason") == Some("error"))
    );
}

#[tokio::test]
async fn successful_sign_in_resets_the_counter() {
    let svc = service(None);
    svc.sign_up("u@example.com", &pw("correct horse battery"))
        .await
        .unwrap();
    for _ in 0..2 {
        assert!(matches!(
            svc.sign_in("u@example.com", &pw("wrong-password")).await,
            Err(AuthError::InvalidCredentials)
        ));
    }
    svc.sign_in("u@example.com", &pw("correct horse battery"))
        .await
        .unwrap();
    // Counter was cleared: 3 more failures are all still plain failures.
    for _ in 0..3 {
        assert!(matches!(
            svc.sign_in("u@example.com", &pw("wrong-password")).await,
            Err(AuthError::InvalidCredentials)
        ));
    }
}

#[tokio::test]
async fn failed_reset_emits_no_password_changed_event() {
    let (events, _guard) = capture();
    let svc = service(None);
    svc.sign_up("u@example.com", &pw("correct horse battery"))
        .await
        .unwrap();
    // A bogus reset token fails without a password_changed event.
    let _ = svc
        .reset_password(&pw("bogus-token"), &pw("brand-new-password-1"))
        .await;
    assert!(
        !events
            .security_events()
            .iter()
            .any(|e| e.name() == "password_changed")
    );
}

#[test]
fn max_password_length_boundary() {
    assert!(check_password_policy(&pw(&"a".repeat(MAX_PASSWORD_LEN))).is_ok());
    assert!(matches!(
        check_password_policy(&pw(&"a".repeat(MAX_PASSWORD_LEN + 1))),
        Err(AuthError::PasswordTooLong)
    ));
}
