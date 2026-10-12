//! Live-database coverage for the Cloud Run bootstrap's Postgres-dependent
//! pieces (andypalmi/amicu-planning#139): `GET /readyz` against a real pool,
//! the production RLS-bypass startup guard
//! ([`amicu_store::probe_rls_bypass_risk`]), and the two-role production
//! topology (`amicu_server::migrate`/`migrate_database_url` as the
//! schema-owning role, then `build_app`/`build_app_with_database_url` as the
//! non-owning runtime role - see the crate docs' "Two-role production
//! model").
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
//! SOCK=/private/tmp/amicu-pg-rls
//!
//! # 3. Roles (cluster-wide). `amicu_app` / `amicu_owner` mirror
//! #    `ops/sql/provision_app_role.sql`'s two-role split; the `amicu_bypass_*`
//! #    pair exercises the guard's transitive-membership check generically
//! #    (this is exactly the shape of Cloud SQL's `cloudsqlsuperuser` or
//! #    Supabase's `postgres`/`supabase_admin` grants - no name is
//! #    hardcoded in the guard itself).
//! psql -h $SOCK -p 55442 -U postgres -d postgres -c "
//!     CREATE ROLE amicu_app LOGIN;
//!     CREATE ROLE amicu_owner LOGIN;
//!     CREATE ROLE amicu_fresh LOGIN;
//!     CREATE ROLE amicu_bypass_direct LOGIN BYPASSRLS;
//!     CREATE ROLE amicu_bypass_group NOLOGIN BYPASSRLS;
//!     CREATE ROLE amicu_bypass_member LOGIN IN ROLE amicu_bypass_group;
//! "
//!
//! # 4. Databases, one per scenario so the tests below never interfere with
//! #    each other's migration/ownership state.
//! createdb -h $SOCK -p 55442 -U postgres amicu                     # superuser owns; migrated
//! createdb -h $SOCK -p 55442 -U postgres -O amicu_app amicu_dev    # single-role dev topology
//! createdb -h $SOCK -p 55442 -U postgres -O amicu_owner amicu_prod # two-role prod topology
//! createdb -h $SOCK -p 55442 -U postgres -O amicu_fresh amicu_probe # nothing ever migrated
//!
//! # 5. amicu_prod: amicu_owner owns the schema; amicu_app gets DML-only
//! #    access via ALTER DEFAULT PRIVILEGES, same shape as
//! #    ops/sql/provision_app_role.sql (run against a DIFFERENT database, so
//! #    it does not collide with this script's own CREATE ROLE statements).
//! psql -h $SOCK -p 55442 -U postgres -d amicu_prod -c "
//!     ALTER SCHEMA public OWNER TO amicu_owner;
//!     GRANT USAGE ON SCHEMA public TO amicu_app;
//!     ALTER DEFAULT PRIVILEGES FOR ROLE amicu_owner IN SCHEMA public
//!         GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO amicu_app;
//!     ALTER DEFAULT PRIVILEGES FOR ROLE amicu_owner IN SCHEMA public
//!         GRANT USAGE, SELECT ON SEQUENCES TO amicu_app;
//! "
//!
//! # 6. Export every DSN this file's tests read (unix-socket URLs; libpq
//! #    accepts a %2F-escaped socket dir in the host position) and run them.
//! B=postgres://%2Fprivate%2Ftmp%2Famicu-pg-rls:55442
//! export DATABASE_URL="$B/amicu?user=postgres"
//! export AMICU_APP_DATABASE_URL="$B/amicu?user=amicu_app"
//! export AMICU_DEV_DATABASE_URL="$B/amicu_dev?user=amicu_app"
//! export AMICU_OWNER_DATABASE_URL="$B/amicu_prod?user=amicu_owner"
//! export AMICU_PROD_APP_DATABASE_URL="$B/amicu_prod?user=amicu_app"
//! export AMICU_BYPASS_DIRECT_DATABASE_URL="$B/amicu?user=amicu_bypass_direct"
//! export AMICU_BYPASS_MEMBER_DATABASE_URL="$B/amicu?user=amicu_bypass_member"
//! export AMICU_FRESH_DATABASE_URL="$B/amicu_probe?user=amicu_fresh"
//! export AMICU_SECURITY_HASH_KEY="$(openssl rand -hex 32)"  # required by AMICU_ENV=production boots
//! export AMICU_EDGE_TOKEN="$(openssl rand -hex 16)" AMICU_REQUIRE_EDGE_TOKEN=true  # also required in production
//! cargo test -p amicu-server --test rls_guard_live -- --ignored
//!
//! # 7. Tear down.
//! pg_ctl -D /private/tmp/amicu-pg-rls stop
//! rm -rf /private/tmp/amicu-pg-rls
//! ```
//!
//! [`two_role_production_topology`]'s first assertion requires `amicu_prod`
//! to have NEVER been migrated - true the first time you run this recipe,
//! but not on a second `cargo test ... -- --ignored` against the SAME
//! still-running cluster (the first run already migrated it). Re-running
//! against a persisted cluster needs a fresh `amicu_prod`-equivalent database
//! (repeat steps 4-5 under a new name and point `AMICU_OWNER_DATABASE_URL` /
//! `AMICU_PROD_APP_DATABASE_URL` at it) - or just tear down and start over,
//! which is the point of an ephemeral cluster.

use amicu_server::signing_key::RedactedEnvString;
use amicu_server::{Config, Environment, Error};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// A disposable, test-only Ed25519 PKCS#8 PEM fixture (same one
/// `signing_key_replicas.rs` uses as its "old" key) - andypalmi/amicu-planning#140's
/// durable-signing-key production gate ([`Error::MissingSigningKey`]) means
/// `two_role_production_topology`'s `Environment::Production` boots need a
/// real key now, not just a real database.
const TEST_SIGNING_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIAGwK8jzAuW3roJK6/gne5fnNmdODLMeRlQnkzMBP/cU\n-----END PRIVATE KEY-----\n";

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
        email_rate_limit_max: 5,
        signin_email_rate_limit_max: 100,
        email_rate_limit_window_secs: 900,
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
        outbox_inline_send: false,
        outbox_inline_budget_ms: amicu_api::DEFAULT_INLINE_BUDGET_MS,
        signups_enabled: true,
    }
}

fn env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set - see this file's module docs"))
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
    let url = env("DATABASE_URL");
    let pool = amicu_store::connect(&url).await.expect("connect");
    amicu_store::run_migrations(&pool)
        .await
        .expect("apply migrations (also makes this role the tables' owner)");

    let risk = amicu_store::probe_rls_bypass_risk(&pool)
        .await
        .expect("probe rls bypass risk");
    assert!(
        risk.bypass_via_roles.contains(&"postgres".to_string()),
        "the recipe's DATABASE_URL role must be the postgres superuser: {risk:?}"
    );
    assert!(
        risk.bypasses_rls(),
        "a superuser role must be reported as bypassing RLS"
    );
}

/// The non-superuser, non-owning `amicu_app` role (`AMICU_APP_DATABASE_URL`
/// in the recipe above) is reported as safe: it is not a member of any
/// superuser/BYPASSRLS role and owns no `FORCE ROW LEVEL SECURITY` table
/// (migrations above ran as the `postgres` superuser, which owns them
/// instead).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL and AMICU_APP_DATABASE_URL; see this file's module docs"]
async fn amicu_app_role_is_ok() {
    let admin_url = env("DATABASE_URL");
    let app_url = env("AMICU_APP_DATABASE_URL");

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
    assert!(
        risk.bypass_via_roles.is_empty(),
        "amicu_app must not be a member of any superuser/BYPASSRLS role: {risk:?}"
    );
    assert!(
        risk.owned_forced_rls_tables.is_empty(),
        "amicu_app must not own any FORCE-RLS table: {risk:?}"
    );
    assert!(
        !risk.bypasses_rls(),
        "amicu_app must be reported as safe: {risk:?}"
    );
}

/// A role with `BYPASSRLS` set directly (not `SUPERUSER`) is refused: the
/// guard must not only check `rolsuper`.
#[tokio::test]
#[ignore = "requires a live Postgres via AMICU_BYPASS_DIRECT_DATABASE_URL; see this file's module docs"]
async fn bypassrls_direct_role_is_refused() {
    let url = env("AMICU_BYPASS_DIRECT_DATABASE_URL");
    let pool = amicu_store::connect(&url).await.expect("connect");

    let risk = amicu_store::probe_rls_bypass_risk(&pool)
        .await
        .expect("probe rls bypass risk");
    assert!(
        risk.bypass_via_roles
            .contains(&"amicu_bypass_direct".to_string()),
        "amicu_bypass_direct must show up via its own BYPASSRLS: {risk:?}"
    );
    assert!(risk.bypasses_rls());
}

/// A role that is merely a MEMBER of a `BYPASSRLS` role (not `BYPASSRLS`
/// itself) is refused too: `pg_has_role(..., 'member')` resolves the grant
/// transitively, generically covering a managed-Postgres front role
/// (Cloud SQL's `cloudsqlsuperuser`, Supabase's `postgres`/`supabase_admin`)
/// without the guard hardcoding any role name.
#[tokio::test]
#[ignore = "requires a live Postgres via AMICU_BYPASS_MEMBER_DATABASE_URL; see this file's module docs"]
async fn bypassrls_via_group_membership_is_refused() {
    let url = env("AMICU_BYPASS_MEMBER_DATABASE_URL");
    let pool = amicu_store::connect(&url).await.expect("connect");

    let risk = amicu_store::probe_rls_bypass_risk(&pool)
        .await
        .expect("probe rls bypass risk");
    assert!(
        risk.bypass_via_roles
            .contains(&"amicu_bypass_group".to_string()),
        "amicu_bypass_member must show up via its BYPASSRLS group membership: {risk:?}"
    );
    assert!(risk.bypasses_rls());
}

/// Before anything has ever migrated the database, `owned_forced_rls_tables`
/// is empty (not an error): "no `FORCE RLS` table exists yet" must read as
/// "owns none", the same as "owns none because someone else owns them".
#[tokio::test]
#[ignore = "requires a live Postgres via AMICU_FRESH_DATABASE_URL; see this file's module docs"]
async fn probe_reports_no_owned_tables_before_any_migration() {
    let url = env("AMICU_FRESH_DATABASE_URL");
    let pool = amicu_store::connect(&url).await.expect("connect");

    let risk = amicu_store::probe_rls_bypass_risk(&pool)
        .await
        .expect("probe rls bypass risk on an unmigrated database must not error");
    assert!(
        risk.owned_forced_rls_tables.is_empty(),
        "no FORCE-RLS table exists yet, so ownership must read empty: {risk:?}"
    );
}

/// Single-role dev topology (`docker/postgres-init/01-app-role.sql`): the
/// SAME role (`amicu_app`, via `AMICU_DEV_DATABASE_URL`) owns the schema and
/// connects as the running service. In `development`,
/// `build_app_with_database_url` migrates at boot and only WARNS about the
/// ownership - it must still return `Ok`.
#[tokio::test]
#[ignore = "requires a live Postgres via AMICU_DEV_DATABASE_URL; see this file's module docs"]
async fn single_role_dev_topology_boots_and_only_warns() {
    let url = env("AMICU_DEV_DATABASE_URL");
    let mut config = test_config();
    config.environment = Environment::Development;

    let built = amicu_server::build_app_with_database_url(&config, Some(&url))
        .await
        .expect(
            "single-role dev topology must boot (guard only warns in development), even though \
             amicu_app owns the schema it just migrated",
        );
    drop(built);
}

/// Two-role production topology end to end, all exercised through the SAME
/// functions the real binary uses (`migrate_database_url` /
/// `build_app_with_database_url`), against the SAME `amicu_prod` database
/// (see the module docs) so the steps compose exactly like a real rollout:
///
/// 1. Before any migration, booting in `production` (as either role) is
///    refused with `Error::PendingMigrations` - it must not silently migrate.
/// 2. `migrate_database_url`, connected as the schema-OWNING `amicu_owner`
///    role, applies every migration and exits `Ok`.
/// 3. Booting in `production` as the non-owning `amicu_app` role now
///    succeeds: no pending migrations, and it neither is a member of a
///    superuser/BYPASSRLS role nor owns any FORCE-RLS table.
/// 4. The negative case: booting in `production` as `amicu_owner` itself -
///    the role that just migrated, and therefore OWNS every FORCE-RLS table -
///    is refused with `Error::RlsBypass`.
#[tokio::test]
#[ignore = "requires a live Postgres via AMICU_OWNER_DATABASE_URL and AMICU_PROD_APP_DATABASE_URL; see this file's module docs"]
async fn two_role_production_topology() {
    let owner_url = env("AMICU_OWNER_DATABASE_URL");
    let app_url = env("AMICU_PROD_APP_DATABASE_URL");

    let mut prod_config = test_config();
    prod_config.environment = Environment::Production;
    // `Environment::Production` refuses to boot without a real signing key
    // (andypalmi/amicu-planning#140's `Error::MissingSigningKey` gate) - this
    // test is about the RLS-bypass guard, not signing keys, so give it a
    // disposable fixture key rather than exercise that gate here too.
    prod_config.signing_key_pem = Some(RedactedEnvString::new(TEST_SIGNING_KEY_PEM));

    // 1. Nothing has migrated `amicu_prod` yet: production boot (as the
    //    eventual runtime role) must refuse with PendingMigrations, not
    //    silently migrate.
    let err = amicu_server::build_app_with_database_url(&prod_config, Some(&app_url))
        .await
        .expect_err("production boot before any migration must be refused");
    assert!(
        matches!(err, Error::PendingMigrations(n) if n > 0),
        "expected PendingMigrations, got {err}"
    );

    // 2. Migrate as the owner role - the real `amicu-server migrate` path.
    amicu_server::migrate_database_url(Some(&owner_url))
        .await
        .expect("migrate as the owner role");

    // 3. Positive case: production boot as the non-owning amicu_app role now
    //    succeeds.
    let built = amicu_server::build_app_with_database_url(&prod_config, Some(&app_url))
        .await
        .expect("production boot as amicu_app must succeed once migrated");
    drop(built);

    // 4. Negative case: production boot as amicu_owner - which now OWNS
    //    every FORCE-RLS table it just created - must be refused.
    let err = amicu_server::build_app_with_database_url(&prod_config, Some(&owner_url))
        .await
        .expect_err("production boot as the owning role must be refused");
    assert!(
        matches!(err, Error::RlsBypass(_)),
        "expected RlsBypass, got {err}"
    );
}
