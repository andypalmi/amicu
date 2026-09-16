//! Real-socket smoke test.
//!
//! The in-process `oneshot` tests in `amicu-api` inject `ConnectInfo(peer)` by
//! hand, so they pass whether or not the production server wires connect-info.
//! This test is the one that cannot: it binds a real ephemeral port, starts the
//! actual [`amicu_server::serve`] (which mounts the router with
//! `into_make_service_with_connect_info::<SocketAddr>()`), and drives it with a
//! genuine HTTP client over a live TCP connection. If that wiring is ever
//! dropped, the peer address is `None`, the credential rate limiter fails open,
//! and the rate-limit assertion below fails - which no in-process test can
//! detect.

use amicu_server::Config;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// A dev config bound to loopback with a deliberately tiny rate-limit budget so
/// the limiter trips within the test.
fn test_config() -> Config {
    Config {
        bind: Some("127.0.0.1:0".parse().unwrap()),
        issuer: "http://localhost:8080".to_string(),
        audience: "amicu".to_string(),
        base_url: None,
        trusted_proxies: String::new(),
        min_tier: amicu_server::MinTier::Community,
        rate_limit_max: 2,
        rate_limit_window_secs: 60,
        rate_limit_timeout_ms: 1_000,
        max_workspaces_per_user: amicu_store::DEFAULT_MAX_WORKSPACES_PER_USER,
        environment: amicu_server::Environment::Development,
        db_max_connections: 5,
        db_min_connections: 0,
        db_acquire_timeout_secs: 10,
        readyz_timeout_ms: 2_000,
        signing_key_pem: None,
        signing_key_pem_file: None,
        signing_keys_retiring_pem: None,
        signing_keys_retiring_pem_file: None,
        internal_audience: None,
        internal_caller_emails: String::new(),
        outbox_drain_batch_size: amicu_api::DEFAULT_DRAIN_BATCH_SIZE,
        outbox_drain_budget_ms: amicu_api::DEFAULT_DRAIN_BUDGET_MS,
    }
}

#[tokio::test]
async fn real_socket_end_to_end() {
    let config = test_config();
    // `build_app_in_memory`, not `build_app`: this test must exercise the
    // zero-config in-memory stores regardless of any `DATABASE_URL` a
    // developer's shell happens to have exported (e.g. after following the
    // README's dev Postgres setup), so it can't be silently hijacked onto
    // Postgres.
    let app = amicu_server::build_app_in_memory(&config)
        .await
        .expect("assemble app")
        .router;

    // Bind a real ephemeral port and start the real server on it.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        amicu_server::serve(listener, app, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // 1. GET /openapi.json serves 200 + a JSON OpenAPI document.
    let resp = client
        .get(format!("{base}/openapi.json"))
        .send()
        .await
        .expect("openapi request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let doc: serde_json::Value = resp.json().await.expect("openapi json body");
    assert!(doc["openapi"].is_string(), "has an openapi version field");
    assert!(doc["paths"].is_object(), "has a paths object");

    // 2. An auth-gated engine route rejects an unauthenticated request (401).
    let resp = client
        .post(format!("{base}/plan"))
        .json(&serde_json::json!({ "config": "" }))
        .send()
        .await
        .expect("plan request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "no bearer token => 401"
    );

    // 3. THE connect-info assertion: the credential rate limiter engages over a
    //    real connection. The limiter keys on the peer IP, which is only present
    //    when the router was served with connect-info. `rate_limit_max` is 2, so
    //    the first two /token attempts are merely 401 (unknown user) and the
    //    third is throttled with 429. If connect-info were missing, the limiter
    //    would fail open and every attempt would stay 401 - failing this.
    for n in 0..2 {
        let resp = client
            .post(format!("{base}/token"))
            .json(&serde_json::json!({ "email": "rl@example.com", "password": "nope" }))
            .send()
            .await
            .expect("token request");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "attempt {n} within budget is a plain 401"
        );
    }
    let resp = client
        .post(format!("{base}/token"))
        .json(&serde_json::json!({ "email": "rl@example.com", "password": "nope" }))
        .send()
        .await
        .expect("token request");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "over budget from a real connection => 429 (proves connect-info is live)"
    );

    // 4. Graceful shutdown actually stops the server.
    shutdown_tx.send(()).expect("send shutdown");
    let joined = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("server task joins promptly after shutdown")
        .expect("server task did not panic");
    joined.expect("serve returned Ok");

    // The listener is gone, so a fresh connection is refused.
    let after = reqwest::Client::new()
        .get(format!("{base}/openapi.json"))
        .send()
        .await;
    assert!(after.is_err(), "server no longer accepts connections");
}

/// A bad trusted-proxy allowlist is surfaced as a build error, not a panic.
/// The proxy allowlist is parsed before any store construction, so this returns
/// early without needing a database even when `DATABASE_URL` is set.
#[tokio::test]
async fn rejects_bad_trusted_proxy() {
    let mut config = test_config();
    config.trusted_proxies = "not-a-cidr".to_string();
    assert!(matches!(
        amicu_server::build_app_in_memory(&config).await,
        Err(amicu_server::Error::Proxy(_))
    ));
}
