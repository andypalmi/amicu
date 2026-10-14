//! Algorithm-confusion regression coverage for [`Verifier`] (a security review
//! finding): the verifier is built
//! with `Validation::new(Algorithm::EdDSA)` (see `src/verifier.rs`), which
//! pins the single algorithm `jsonwebtoken::decode` will accept. This proves
//! two classic JWT algorithm-confusion attacks are rejected rather than
//! silently accepted or causing a panic:
//!
//! 1. A token whose header claims `alg: "HS256"`, HMAC-signed using the
//!    active key's PUBLIC bytes as the shared secret (the textbook
//!    RS/EdDSA-to-HMAC confusion attack: a public key is, well, public, so if
//!    a verifier only checked "does this kid resolve to a decoding key" and
//!    not "does the algorithm family match", an attacker could forge tokens
//!    using nothing but published JWKS material).
//! 2. A token whose header claims `alg: "none"` with an empty signature
//!    segment (the classic `none`-algorithm forgery).
//!
//! Neither crafted token is built via [`amicu_auth::issuer::TokenIssuer`]
//! (which only ever emits `EdDSA`) - both are hand-assembled with
//! `jsonwebtoken` directly, exactly as an attacker would.

use amicu_auth::claims::{Claims, Tier};
use amicu_auth::jwks::Jwks;
use amicu_auth::key::SigningKey;
use amicu_auth::tenant::{Role, WorkspaceId};
use amicu_auth::verifier::{StaticJwksProvider, Verifier, VerifierConfig};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

const ISS: &str = "https://auth.amicu.dev";
const AUD: &str = "amicu-api";

fn forged_claims() -> Claims {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_secs(),
    )
    .expect("current unix time fits in i64");
    // Content is irrelevant to what's under test - what matters is that the
    // SIGNATURE and ALGORITHM are attacker-forged, not that the claims
    // themselves are otherwise well-formed. Using the real active key's own
    // `kid` in the header (set by each test below) is what makes this an
    // algorithm-confusion attack rather than just an unknown-kid rejection.
    Claims {
        iss: ISS.to_string(),
        aud: AUD.to_string(),
        sub: "attacker".to_string(),
        exp: now + 300,
        iat: now,
        nbf: now,
        wsid: WorkspaceId::new(),
        wsrole: Role::Owner,
        tier: Tier::Enterprise,
        roles: vec!["admin".to_string()],
    }
}

/// Attack (i): HMAC-SHA256, "signed" using the active key's PUBLIC bytes
/// (exactly what's published at `/.well-known/jwks.json`) as the HMAC
/// secret. If the verifier trusted the header's `alg` instead of enforcing
/// its own configured algorithm, this token would verify successfully using
/// material anyone can fetch.
#[tokio::test]
async fn hs256_confusion_using_the_public_key_as_hmac_secret_is_rejected() {
    let active = SigningKey::generate().expect("generate active key");
    let jwks = Jwks {
        keys: vec![active.public_jwk()],
    };
    let verifier = Verifier::new(StaticJwksProvider::new(jwks), VerifierConfig::new(ISS, AUD));

    // The exact public-key bytes the real JWKS publishes for this kid.
    let public_key_bytes = URL_SAFE_NO_PAD
        .decode(&active.public_jwk().x)
        .expect("jwk x is valid base64url");

    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(active.kid().to_string());
    let claims = forged_claims();
    let forged = encode(
        &header,
        &claims,
        &EncodingKey::from_secret(&public_key_bytes),
    )
    .expect("craft HS256-forged token");

    let result = verifier.verify(&forged).await;
    assert!(
        result.is_err(),
        "an HS256 token 'signed' with the public key must never verify"
    );
}

/// Attack (ii): `alg: "none"` with an empty signature segment - the classic
/// unsigned-token forgery. `jsonwebtoken::Algorithm` has no `None` variant
/// (the crate deliberately does not support producing one), so this is
/// hand-assembled at the JSON/base64url level, exactly as a real attacker's
/// tooling would.
#[tokio::test]
async fn none_algorithm_forgery_with_empty_signature_is_rejected() {
    let active = SigningKey::generate().expect("generate active key");
    let jwks = Jwks {
        keys: vec![active.public_jwk()],
    };
    let verifier = Verifier::new(StaticJwksProvider::new(jwks), VerifierConfig::new(ISS, AUD));

    let claims = forged_claims();
    let header_json = format!(r#"{{"alg":"none","typ":"JWT","kid":"{}"}}"#, active.kid());
    let header_b64 = URL_SAFE_NO_PAD.encode(header_json.as_bytes());
    let payload_b64 =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("serialize forged claims"));
    // Empty signature segment: the defining trait of a "none"-alg forgery.
    let forged = format!("{header_b64}.{payload_b64}.");

    let result = verifier.verify(&forged).await;
    assert!(
        result.is_err(),
        "an alg=none token with an empty signature must never verify"
    );
}
