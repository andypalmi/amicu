//! Live-database coverage for the Cloud Run bootstrap's Postgres-dependent
//! pieces (andypalmi/amicu-planning#139): `GET /readyz` against a real pool,
//! and the production RLS-bypass startup guard
//! ([`amicu_store::probe_rls_bypass_risk`]).
//!
//! `#[ignore]`d like the other live-DB tests in this workspace (see
//! `crates/amicu-store/tests/tenancy_stores.rs`). Docker is unavailable on
//! some dev machines, so unlike that file's docker-compose recipe, this one
//! is written against a throwaway `initdb`-created cluster you start and stop
//! by hand:
//!
//! ```sh
//! # 1. Initialize a scratch cluster with trust auth (no passwords needed for
//! #    a throwaway local instance) and a short unix-socket path (Postgres
//! #    caps the socket path at ~100 bytes, so this must live somewhere short
//! #    - /private/tmp/amicu-pg-rls on macOS, not a long project-relative
//! #    path).
//! initdb -D /private/tmp/amicu-pg-rls --auth-local=trust --auth-host=trust -U postgres
//!
//! # 2. Start it on a distinct, unlikely-to-collide port, socket-only (no TCP
//! #    needed since everything below connects over the unix socket).
//! pg_ctl -D /private/tmp/amicu-pg-rls -o "-p 55442 -k /private/tmp/amicu-pg-rls -h ''" \
//!     -l /private/tmp/amicu-pg-rls/log start
//!
//! # 3. Create the database plus the non-superuser amicu_app role the RLS
//! #    design assumes the app connects as (see andypalmi/amicu-planning#114).
//! #    Migrations run as the `postgres` superuser below, so it (not
//! #    amicu_app) ends up OWNING the tenant tables - exactly the shape a real
//! #    deploy should have.
//! createdb -h /private/tmp/amicu-pg-rls -p 55442 -U postgres amicu
//! psql -h /private/tmp/amicu-pg-rls -p 55442 -U postgres -d amicu \
//!     -c "CREATE ROLE amicu_app LOGIN;"
//!
//! # 4. Export both DSNs (unix-socket URLs; libpq accepts a %2F-escaped
//! #    socket dir in the host position) and run the ignored tests.
//! export DATABASE_URL="postgres://postgres@%2Fprivate%2Ftmp%2Famicu-pg-rls:55442/amicu"
//! export AMICU_APP_DATABASE_URL="postgres://amicu_app@%2Fprivate%2Ftmp%2Famicu-pg-rls:55442/amicu"
//! cargo test -p amicu-server --test rls_guard_live -- --ignored
//!
//! # 5. Tear down.
//! pg_ctl -D /private/tmp/amicu-pg-rls stop
//! rm -rf /private/tmp/amicu-pg-rls
//! ```

use amicu_server::{Config, Environment};
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
        max_workspaces_per_user: amicu_store::DEFAULT_MAX_WORKSPACES_PER_USER,
        environment: Environment::Development,
        db_max_connections: 5,
        db_min_connections: 0,
        db_acquire_timeout_secs: 10,
    }
}

/// `GET /readyz` against a real, reachable pool answers `200
/// {"status":"ok","database":"postgres"}`. `build_app` (not
/// `build_app_in_memory`) is used deliberately here: this test exists
/// precisely to exercise the ambient-`DATABASE_URL` Postgres path.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; see this file's module docs"]
async fn readyz_reports_ready_against_a_live_pool() {
    assert!(
        std::env::var("DATABASE_URL").is_ok(),
        "DATABASE_URL must be set for this ignored test - see the module docs"
    );

    let config = test_config();
    let built = amicu_server::build_app(&config)
        .await
        .expect("assemble app against a live Postgres");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        amicu_server::serve(listener, built.router, async move {
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
        serde_json::json!({ "status": "ok", "database": "postgres" })
    );

    shutdown_tx.send(()).expect("send shutdown");
    server
        .await
        .expect("server task did not panic")
        .expect("serve returned Ok");
}

/// A superuser `DATABASE_URL` (`postgres` in the recipe above) is refused: a
/// Postgres superuser always bypasses row-level security, so the production
/// startup guard must not let it connect.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; see this file's module docs"]
async fn superuser_role_is_refused() {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this ignored test");
    let pool = amicu_store::connect(&url).await.expect("connect");
    amicu_store::run_migrations(&pool)
        .await
        .expect("apply migrations (also makes this role the tables' owner)");

    let risk = amicu_store::probe_rls_bypass_risk(&pool)
        .await
        .expect("probe rls bypass risk");
    assert!(
        risk.is_superuser,
        "the recipe's DATABASE_URL role must be the postgres superuser"
    );
    assert!(
        risk.bypasses_rls(),
        "a superuser role must be reported as bypassing RLS"
    );
}

/// The non-superuser, non-owning `amicu_app` role (`AMICU_APP_DATABASE_URL`
/// in the recipe above) is reported as safe: it neither is a superuser nor
/// owns the tenant tables (migrations above ran as the `postgres` superuser,
/// which owns them instead).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL and AMICU_APP_DATABASE_URL; see this file's module docs"]
async fn amicu_app_role_is_ok() {
    let admin_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for this ignored test");
    let app_url = std::env::var("AMICU_APP_DATABASE_URL")
        .expect("AMICU_APP_DATABASE_URL must be set for this ignored test");

    // Migrate as the superuser first (idempotent), so `workspace_members`
    // exists and is owned by `postgres`, not `amicu_app`, before probing.
    let admin_pool = amicu_store::connect(&admin_url)
        .await
        .expect("connect admin");
    amicu_store::run_migrations(&admin_pool)
        .await
        .expect("apply migrations");

    let app_pool = amicu_store::connect(&app_url)
        .await
        .expect("connect amicu_app");
    let risk = amicu_store::probe_rls_bypass_risk(&app_pool)
        .await
        .expect("probe rls bypass risk");
    assert!(!risk.is_superuser, "amicu_app must not be a superuser");
    assert!(
        !risk.owns_tenant_tables,
        "amicu_app must not own workspace_members"
    );
    assert!(
        !risk.bypasses_rls(),
        "amicu_app must be reported as safe: {risk:?}"
    );
}
