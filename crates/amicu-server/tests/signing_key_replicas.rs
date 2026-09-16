//! Real-socket, multi-replica coverage for the durable signing keyring
//! (andypalmi/amicu-planning#140): proves that replicas sharing the same
//! `AMICU_SIGNING_KEY_PEM` env cross-verify each other's tokens and publish
//! identical JWKS, that a retiring key still verifies while a new active key
//! gets a new `kid`, and that dropping a key that signed a still-live token
//! makes that token unverifiable.
//!
//! Real-socket style, matching `smoke.rs`: a genuine HTTP client over a live
//! TCP connection, spawning `amicu_server::serve` directly.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use amicu_server::Config;
use amicu_server::signing_key::RedactedEnvString;

/// A disposable, test-only Ed25519 PKCS#8 PEM fixture ("old" key).
const OLD_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIAGwK8jzAuW3roJK6/gne5fnNmdODLMeRlQnkzMBP/cU\n-----END PRIVATE KEY-----\n";
/// A distinct disposable, test-only Ed25519 PKCS#8 PEM fixture ("new" key).
const NEW_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIBinCJ18h+LO/AdMnriwJVdk7ionOYmeAsHnxB6+qNZM\n-----END PRIVATE KEY-----\n";

fn test_config(signing_key_pem: Option<&str>, signing_keys_retiring_pem: Option<&str>) -> Config {
    Config {
        bind: Some("127.0.0.1:0".parse().unwrap()),
        issuer: "http://localhost:8080".to_string(),
        audience: "amicu".to_string(),
        base_url: None,
        trusted_proxies: String::new(),
        min_tier: amicu_server::MinTier::Community,
        // Generous: several signup/signin/plan calls across a test must never
        // trip the credential rate limiter.
        rate_limit_max: 1000,
        rate_limit_window_secs: 60,
        rate_limit_timeout_ms: 1_000,
        max_workspaces_per_user: amicu_store::DEFAULT_MAX_WORKSPACES_PER_USER,
        environment: amicu_server::Environment::Development,
        db_max_connections: 5,
        db_min_connections: 0,
        db_acquire_timeout_secs: 10,
        readyz_timeout_ms: 2_000,
        signing_key_pem: signing_key_pem.map(RedactedEnvString::new),
        signing_key_pem_file: None,
        signing_keys_retiring_pem: signing_keys_retiring_pem.map(RedactedEnvString::new),
        signing_keys_retiring_pem_file: None,
    }
}

async fn spawn_app(
    config: Config,
) -> (
    String,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let router = amicu_server::build_app_in_memory(&config)
        .await
        .expect("assemble app")
        .router;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        amicu_server::serve(listener, router, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    (format!("http://{addr}"), shutdown_tx, server)
}

async fn shutdown(tx: oneshot::Sender<()>, server: tokio::task::JoinHandle<std::io::Result<()>>) {
    let _ = tx.send(());
    server
        .await
        .expect("server task did not panic")
        .expect("serve returned Ok");
}

/// A long, policy-compliant password (the auth password policy has a minimum
/// length; this comfortably clears it).
const TEST_PASSWORD: &str = "correct horse battery staple 42";

async fn signup_and_get_token(base: &str, email: &str) -> String {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/signup"))
        .json(&serde_json::json!({ "email": email, "password": TEST_PASSWORD }))
        .send()
        .await
        .expect("signup request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::ACCEPTED,
        "signup should succeed"
    );

    let resp = client
        .post(format!("{base}/token"))
        .json(&serde_json::json!({ "email": email, "password": TEST_PASSWORD }))
        .send()
        .await
        .expect("token request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "sign-in should succeed"
    );
    let body: serde_json::Value = resp.json().await.expect("token json body");
    body["access_token"]
        .as_str()
        .expect("access_token present")
        .to_string()
}

/// Decode a JWT's header and return its `kid` claim.
fn decode_kid(jwt: &str) -> String {
    let header_b64 = jwt.split('.').next().expect("jwt has a header segment");
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .expect("header is valid base64url");
    let header: serde_json::Value =
        serde_json::from_slice(&header_bytes).expect("header is valid json");
    header["kid"]
        .as_str()
        .expect("header has a kid")
        .to_string()
}

/// Two replicas configured with the SAME active-key PEM cross-verify each
/// other's tokens and publish byte-for-byte-equivalent JWKS documents.
#[tokio::test]
async fn same_pem_cross_replica_verification_and_identical_jwks() {
    let (base_a, tx_a, server_a) = spawn_app(test_config(Some(OLD_PEM), None)).await;
    let (base_b, tx_b, server_b) = spawn_app(test_config(Some(OLD_PEM), None)).await;

    let token_from_a = signup_and_get_token(&base_a, "replica-a@example.test").await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_b}/plan"))
        .bearer_auth(&token_from_a)
        .json(&serde_json::json!({ "config": "" }))
        .send()
        .await
        .expect("plan request against replica B");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "replica B must accept a token signed by replica A (same shared key) and plan the \
         (empty, but valid) config successfully"
    );

    let jwks_a: serde_json::Value = client
        .get(format!("{base_a}/.well-known/jwks.json"))
        .send()
        .await
        .expect("jwks A request")
        .json()
        .await
        .expect("jwks A json");
    let jwks_b: serde_json::Value = client
        .get(format!("{base_b}/.well-known/jwks.json"))
        .send()
        .await
        .expect("jwks B request")
        .json()
        .await
        .expect("jwks B json");
    assert_eq!(jwks_a, jwks_b, "identical env must publish identical JWKS");

    shutdown(tx_a, server_a).await;
    shutdown(tx_b, server_b).await;
}

/// A rotated deployment (new active key + old key retiring) still verifies
/// tokens signed by the retiring key, mints new tokens under a distinct
/// `kid`, and publishes both keys in JWKS.
#[tokio::test]
async fn retiring_key_still_verifies_new_key_gets_new_kid_jwks_lists_both() {
    let (base_old, tx_old, server_old) = spawn_app(test_config(Some(OLD_PEM), None)).await;
    let old_token = signup_and_get_token(&base_old, "old-era@example.test").await;
    let old_kid = decode_kid(&old_token);
    shutdown(tx_old, server_old).await;

    let (base_c, tx_c, server_c) = spawn_app(test_config(Some(NEW_PEM), Some(OLD_PEM))).await;

    // (a) The retiring key still verifies the old-era token.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_c}/plan"))
        .bearer_auth(&old_token)
        .json(&serde_json::json!({ "config": "" }))
        .send()
        .await
        .expect("plan request with old token");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "the retiring key must still verify tokens it signed, and the (empty, but valid) config \
         must plan successfully"
    );

    // (b) A fresh token minted on C is signed by the NEW active key: a
    // different kid.
    let new_token = signup_and_get_token(&base_c, "new-era@example.test").await;
    let new_kid = decode_kid(&new_token);
    assert_ne!(
        old_kid, new_kid,
        "a token minted after rotation must carry the new active key's kid"
    );

    // (c) JWKS lists exactly 2 keys, including both kids.
    let jwks: serde_json::Value = client
        .get(format!("{base_c}/.well-known/jwks.json"))
        .send()
        .await
        .expect("jwks request")
        .json()
        .await
        .expect("jwks json");
    let keys = jwks["keys"].as_array().expect("keys array");
    assert_eq!(keys.len(), 2, "jwks must list active + retiring: {jwks}");
    let kids: Vec<&str> = keys.iter().map(|k| k["kid"].as_str().unwrap()).collect();
    assert!(kids.contains(&old_kid.as_str()), "{kids:?}");
    assert!(kids.contains(&new_kid.as_str()), "{kids:?}");

    shutdown(tx_c, server_c).await;
}

/// Once the old key is dropped entirely (not even retiring), a token it
/// signed is rejected.
#[tokio::test]
async fn without_the_old_key_old_tokens_are_rejected() {
    let (base_old, tx_old, server_old) = spawn_app(test_config(Some(OLD_PEM), None)).await;
    let old_token = signup_and_get_token(&base_old, "throwaway@example.test").await;
    shutdown(tx_old, server_old).await;

    let (base_d, tx_d, server_d) = spawn_app(test_config(Some(NEW_PEM), None)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base_d}/plan"))
        .bearer_auth(&old_token)
        .json(&serde_json::json!({ "config": "" }))
        .send()
        .await
        .expect("plan request with orphaned old token");
    // `auth_gate` (crates/amicu-api/src/engine.rs) maps every verification
    // failure - including AuthError::UnknownKid, which is exactly what an
    // orphaned kid produces - to 401 Unauthorized.
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a token signed by a key no longer published anywhere must be rejected"
    );

    shutdown(tx_d, server_d).await;
}
