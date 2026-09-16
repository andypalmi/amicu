//! Health/readiness and production-gate coverage for the Cloud Run bootstrap
//! (andypalmi/amicu-planning#139).
//!
//! Real-socket style, matching `smoke.rs`: a genuine HTTP client over a live
//! TCP connection, so these exercise the actual served routes rather than an
//! in-process router shortcut.

use amicu_server::{Config, Environment, Error};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn test_config() -> Config {
    Config {
        bind: Some("127.0.0.1:0".parse().unwrap()),
        issuer: "http://localhost:8080".to_string(),
        audience: "amicu".to_string(),
        base_url: None,
        trusted_proxies: String::new(),
        min_tier: amicu_server::MinTier::Community,
        rate_limit_max: 30,
        rate_limit_window_secs: 60,
        rate_limit_timeout_ms: 1_000,
        max_workspaces_per_user: amicu_store::DEFAULT_MAX_WORKSPACES_PER_USER,
        environment: Environment::Development,
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

/// `GET /healthz` is unauthenticated, checks no dependency, and always
/// answers `200 {"status":"ok"}`.
#[tokio::test]
async fn healthz_returns_200_ok() {
    let config = test_config();
    let app = amicu_server::build_app_in_memory(&config)
        .await
        .expect("assemble app")
        .router;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        amicu_server::serve(listener, app, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .expect("healthz request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("healthz json body");
    assert_eq!(body, serde_json::json!({ "status": "ok" }));

    shutdown_tx.send(()).expect("send shutdown");
    server
        .await
        .expect("server task did not panic")
        .expect("serve returned Ok");
}

/// `GET /readyz` on the in-memory dev path (no `DATABASE_URL`) is trivially
/// `200`, with a body naming the `in-memory` database mode - there is no pool
/// to probe.
#[tokio::test]
async fn readyz_in_memory_returns_200() {
    let config = test_config();
    let app = amicu_server::build_app_in_memory(&config)
        .await
        .expect("assemble app")
        .router;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        amicu_server::serve(listener, app, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/readyz"))
        .send()
        .await
        .expect("readyz request");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("readyz json body");
    assert_eq!(
        body,
        serde_json::json!({ "status": "ok", "database": "in-memory" })
    );

    shutdown_tx.send(()).expect("send shutdown");
    server
        .await
        .expect("server task did not panic")
        .expect("serve returned Ok");
}

/// `AMICU_ENV=production` with no `DATABASE_URL` is a hard startup error, not
/// a silent in-memory fallback. `build_app_in_memory` forces `database_url =
/// None` regardless of the ambient environment, which is exactly the
/// scenario under test here.
#[tokio::test]
async fn production_without_database_url_is_hard_error() {
    let mut config = test_config();
    config.environment = Environment::Production;

    let err = amicu_server::build_app_in_memory(&config)
        .await
        .expect_err("production with no DATABASE_URL must fail");
    assert!(
        matches!(err, Error::MissingDatabaseUrl),
        "expected MissingDatabaseUrl, got {err:?}"
    );
}
