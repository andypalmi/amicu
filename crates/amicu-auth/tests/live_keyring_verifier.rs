//! Proves the exact machinery `amicu-server` wires together for
//! a [`KeyringJwksProvider`] backed by
//! [`AuthService::keyring_handle`] observes a runtime `rotate_keys` call
//! immediately, with no restart and no JWKS round-trip, because it reads the
//! same `Arc<RwLock<Keyring>>` the service mutates.

use std::sync::Arc;

use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::service::AuthService;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verifier::{KeyringJwksProvider, Verifier, VerifierConfig};
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_core::secret::SecretValue;

const ISS: &str = "https://auth.amicu.dev";
const AUD: &str = "amicu-api";

fn pw(s: &str) -> SecretValue {
    SecretValue::new(s.as_bytes().to_vec())
}

fn build_service() -> AuthService {
    let keyring = Keyring::new(SigningKey::generate().unwrap());
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));
    AuthService::new(
        keyring,
        issuer,
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        "https://auth.amicu.dev",
    )
}

#[tokio::test]
async fn rotation_via_rotate_keys_is_observed_by_engine_verifier_without_restart() {
    let service = build_service();
    let handle = service.keyring_handle();
    let verifier = Verifier::new(
        KeyringJwksProvider::new(handle),
        VerifierConfig::new(ISS, AUD),
    );

    // Mint a token signed by the ORIGINAL active key and confirm it verifies.
    service
        .sign_up("rotation@example.com", &pw("correct horse battery staple"))
        .await
        .expect("sign up");
    let first_pair = service
        .sign_in("rotation@example.com", &pw("correct horse battery staple"))
        .await
        .expect("sign in before rotation");
    verifier
        .verify(&first_pair.access_token)
        .await
        .expect("pre-rotation token verifies");

    // Rotate: a brand-new active key, no restart of anything.
    service.rotate_keys(SigningKey::generate().unwrap());

    // Mint a SECOND token, now signed by the NEW active key, and confirm the
    // SAME verifier instance (no rebuild) accepts it too - proving the
    // KeyringJwksProvider observed the rotation live.
    let second_pair = service
        .sign_in("rotation@example.com", &pw("correct horse battery staple"))
        .await
        .expect("sign in after rotation");
    verifier
        .verify(&second_pair.access_token)
        .await
        .expect("post-rotation token verifies through the same verifier");

    // The pre-rotation token must still verify: `rotate_in` retains the old
    // key as retiring, not drop it.
    verifier
        .verify(&first_pair.access_token)
        .await
        .expect("pre-rotation token still verifies after rotation (old key retained as retiring)");
}
