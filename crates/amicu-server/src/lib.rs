//! Runnable bootstrap for the Amicu Management API.
//!
//! `amicu-api` is a library - it hands back a [`Router`] from
//! `api_router(auth_state, engine_state, billing_state, org_state)` but binds no socket. This crate is the
//! thin glue that a deployment actually runs: it constructs the auth + engine
//! state, reads its configuration from the environment (or flags), binds a
//! `TcpListener`, and serves the router with graceful shutdown.
//!
//! # Cloud Run bootstrap (andypalmi/amicu-planning#139)
//!
//! [`build_app`] connects Postgres exactly ONCE per process and hands
//! `pool.clone()` to every store that needs it (the auth quartet, the org
//! console, tenancy, config, and the state backend) instead of opening its own
//! pool - see [`run_migrations`](amicu_store::run_migrations) and
//! [`amicu_store::PoolConfig`]. [`run`] honours the `PORT` env var Cloud Run
//! assigns (see [`Config::bind`]), serves `GET /healthz` / `GET /readyz`
//! (unauthenticated, see [`amicu_api::health`]) for the platform's liveness /
//! readiness probes, and closes the shared pool after [`serve`] returns.
//! `AMICU_ENV=production` (see [`Environment`]) turns a missing
//! `DATABASE_URL`, and a connected role that would bypass row-level security,
//! into a hard, non-zero-exit startup error instead of a silent dev fallback;
//! `development` (the default) only warns, so local runs and pre-database
//! self-host stay zero-config.
//!
//! ## Two-role production model
//!
//! In `production`, this bootstrap deliberately does NOT migrate at boot (see
//! [`connect_shared_pool`]'s docs): run `amicu-server migrate` (see
//! [`migrate`]) once, connected as a schema-OWNING role, before starting any
//! replica; the replicas themselves connect as a SEPARATE, non-owning role
//! that only ever does DML. `ops/sql/provision_app_role.sql` provisions both
//! roles. `development` keeps the old single-role, migrate-at-boot
//! convenience (`docker/postgres-init/01-app-role.sql`), and only warns
//! instead of refusing to start when the connecting role happens to own the
//! schema.
//!
//! ## Multi-replica signing key (andypalmi/amicu-planning#140)
//!
//! [`build_app`] no longer generates a signing key at boot. The ACTIVE key
//! (and any RETIRING keys, kept for verification only during rotation) are
//! loaded once from `AMICU_SIGNING_KEY_PEM` / `AMICU_SIGNING_KEYS_RETIRING_PEM`
//! (or their `_FILE` variants — see the `signing_key` module docs and
//! `docs/ops/signing-key-rotation.md`). Every replica that shares the same
//! secret-injected env therefore signs and verifies with the identical key
//! set and publishes an identical `/.well-known/jwks.json` — a token minted
//! by one Cloud Run replica verifies on any other. The engine-gate verifier
//! also no longer trusts a boot-time snapshot: it reads the SAME live,
//! in-process keyring the auth service signs with
//! ([`KeyringJwksProvider`](amicu_auth::verifier::KeyringJwksProvider)), so a
//! runtime rotation (`AuthService::rotate_keys`) propagates with NO restart
//! and NO network round-trip — bounded by the verifier's own cache, not
//! instant: a new active key is picked up by the first token that carries
//! it (worst case one `refetch_cooldown`, 60s, under a concurrent
//! unknown-`kid` flood), and a retired key stops verifying within one JWKS
//! `cache_ttl` (5 minutes) — well inside the 5-minute access-token lifetime
//! those tokens would expire at regardless. `development` (no
//! `AMICU_SIGNING_KEY_PEM` set) still
//! falls back to an ephemeral `SigningKey::generate()` per boot, with a
//! loud warning — correct for a single local process, wrong for anything
//! with more than one replica or that must survive a restart; `production`
//! refuses to start without a real key instead ([`Error::MissingSigningKey`]).
//!
//! Running MULTIPLE replicas of the SAME deployment concurrently on Cloud
//! Run (`autoscaling.knative.dev/maxScale` above `"1"`) is therefore now
//! safe with respect to signing keys, in `production`
//! (`AMICU_ENV=production`, which requires the active key to be set).
//!
//! # Dev / single-node bootstrap
//!
//! Absent a `DATABASE_URL`, state is wired with the **in-memory** auth stores
//! ([`InMemoryUserStore`](amicu_auth::user::InMemoryUserStore) and friends);
//! absent `AMICU_SIGNING_KEY_PEM`, the signing keyring falls back to a
//! freshly generated key each boot (see the "Multi-replica signing key"
//! section above), and the engine gate verifies tokens against the same live
//! keyring the auth service signs with. That is sufficient for a single
//! node / local end-to-end testing, but it means **all state is lost on
//! restart** and the process cannot be horizontally scaled without setting
//! `AMICU_SIGNING_KEY_PEM` (each replica would otherwise sign with a
//! different key and hold its own user table). This is the `development`
//! (default) `AMICU_ENV` path; `production` refuses to start without a real
//! `DATABASE_URL` or a real `AMICU_SIGNING_KEY_PEM` instead of silently
//! falling back to either. A distributed rate limiter remains future work -
//! see the `amicu-auth` store traits, which this bootstrap is the seam for.
//!
//! # Connect-info is load-bearing
//!
//! The credential-route rate limiter keys on the peer socket address, which is
//! only populated when the router is served with
//! [`into_make_service_with_connect_info`](axum::routing::IntoMakeServiceWithConnectInfo).
//! [`serve`] does exactly that; do not bypass it. Without it the limiter has no
//! client identity and fails open (see [`serve`] and the crate's smoke test).
//!
//! # Configuration
//!
//! Every field of [`Config`] is populated from a flag or the matching
//! environment variable (flags win). Defaults are dev-friendly, so the server
//! runs with no configuration at all for local end-to-end testing.
//!
//! | Field | Flag | Env var | Default |
//! |-------|------|---------|---------|
//! | `bind` | `--bind` | `AMICU_BIND` | `0.0.0.0:8080` - or `0.0.0.0:$PORT` when `PORT` is set and `AMICU_BIND`/`--bind` is not (the Cloud Run contract; see [`Config::bind`]) |
//! | `issuer` | `--issuer` | `AMICU_ISSUER` | `http://localhost:8080` |
//! | `audience` | `--audience` | `AMICU_AUDIENCE` | `amicu` |
//! | `base_url` | `--base-url` | `AMICU_BASE_URL` | the `issuer` value |
//! | `trusted_proxies` | `--trusted-proxies` | `AMICU_TRUSTED_PROXIES` | empty (trust none) |
//! | `min_tier` | `--min-tier` | `AMICU_MIN_TIER` | `community` |
//! | `rate_limit_max` | `--rate-limit-max` | `AMICU_RATE_LIMIT_MAX` | `30` |
//! | `rate_limit_window_secs` | `--rate-limit-window-secs` | `AMICU_RATE_LIMIT_WINDOW_SECS` | `60` |
//! | `email_rate_limit_max` | `--email-rate-limit-max` | `AMICU_EMAIL_RATE_LIMIT_MAX` | `5` - per normalized email, `/recover` + `/verify/resend` (#194) |
//! | `signin_email_rate_limit_max` | `--signin-email-rate-limit-max` | `AMICU_SIGNIN_EMAIL_RATE_LIMIT_MAX` | `100` - FAILED sign-ins per normalized email (#194) |
//! | `email_rate_limit_window_secs` | `--email-rate-limit-window-secs` | `AMICU_EMAIL_RATE_LIMIT_WINDOW_SECS` | `900` |
//! | (env only) | - | `AMICU_SECURITY_HASH_KEY` | none - secret key (>= 32 bytes) for hashing emails/IPs in `amicu::security` events and limiter/backoff keys; set the SAME value on every replica. REQUIRED in `AMICU_ENV=production` (#194) |
//! | `rate_limit_timeout_ms` | `--rate-limit-timeout-ms` | `AMICU_RATE_LIMIT_TIMEOUT_MS` | `1000` - Postgres path only; a timeout fails OPEN like any other database error |
//! | `max_workspaces_per_user` | `--max-workspaces-per-user` | `AMICU_MAX_WORKSPACES_PER_USER` | `50` |
//! | `environment` | `--environment` | `AMICU_ENV` | `development` |
//! | `db_max_connections` | `--db-max-connections` | `AMICU_DB_MAX_CONNECTIONS` | `5` |
//! | `db_min_connections` | `--db-min-connections` | `AMICU_DB_MIN_CONNECTIONS` | `0` |
//! | `db_acquire_timeout_secs` | `--db-acquire-timeout-secs` | `AMICU_DB_ACQUIRE_TIMEOUT_SECS` | `10` |
//! | `readyz_timeout_ms` | `--readyz-timeout-ms` | `AMICU_READYZ_TIMEOUT_MS` | `2000` |
//! | `signing_key_pem` | `--signing-key-pem` | `AMICU_SIGNING_KEY_PEM` | none (dev: ephemeral `generate()`; prod: hard error) |
//! | `signing_key_pem_file` | `--signing-key-pem-file` | `AMICU_SIGNING_KEY_PEM_FILE` | none |
//! | `signing_keys_retiring_pem` | `--signing-keys-retiring-pem` | `AMICU_SIGNING_KEYS_RETIRING_PEM` | none |
//! | `signing_keys_retiring_pem_file` | `--signing-keys-retiring-pem-file` | `AMICU_SIGNING_KEYS_RETIRING_PEM_FILE` | none |
//! | `internal_audience` | `--internal-audience` | `AMICU_INTERNAL_AUDIENCE` | the effective `base_url` |
//! | `internal_caller_emails` | `--internal-caller-emails` | `AMICU_INTERNAL_CALLER_EMAILS` | empty (OIDC drain auth OFF) |
//! | `outbox_drain_batch_size` | `--outbox-drain-batch-size` | `AMICU_OUTBOX_DRAIN_BATCH_SIZE` | `50` |
//! | `outbox_drain_budget_ms` | `--outbox-drain-budget-ms` | `AMICU_OUTBOX_DRAIN_BUDGET_MS` | `20000` |
//!
//! `environment` (see [`Environment`]) gates production safety checks: a
//! missing `DATABASE_URL` and an RLS-bypassing connected role are hard startup
//! errors in `production`, warnings in `development`. The `db_*` fields tune
//! the single shared Postgres pool (see [`amicu_store::PoolConfig`]); their
//! small defaults are sized for many autoscaled Cloud Run replicas sharing one
//! Cloud SQL connection ceiling, not a single beefy long-lived process.
//!
//! The signing-key fields split into an ACTIVE key (`signing_key_pem`/`_file`;
//! signs new tokens and is the primary JWKS entry) and zero or more RETIRING
//! keys (`signing_keys_retiring_pem`/`_file`; verify-only, kept published so
//! tokens they already signed keep verifying through rotation). See the
//! `signing_key` module docs and `docs/ops/signing-key-rotation.md` for the
//! full operational rotation procedure.
//!
//! `bind`'s default changed from `127.0.0.1:8080` to `0.0.0.0:8080` in
//! andypalmi/amicu-planning#139 - a deliberate, dev-facing change (not just a
//! Cloud Run one): loopback-only refused every container/VM/Docker Desktop
//! setup that reaches the process over a non-loopback interface, which is the
//! common case outside "cargo run on your own machine". Restore the old
//! loopback-only default explicitly with `AMICU_BIND=127.0.0.1:8080` (or
//! `--bind`) if you want it back.
//!
//! # Email outbox + the scheduler contract (andypalmi/amicu-planning#141)
//!
//! Cloud Run throttles a container's CPU to near-zero the moment a response is
//! written, so the old pattern of `tokio::spawn`-ing the SMTP send after the
//! response returned lost mail. Transactional email is now PERSISTED at request
//! time into `email_outbox` (see [`amicu_store::outbox`]) - on the Postgres
//! path the auth flows' notifier IS that queue - and actually delivered by
//! `POST /internal/outbox/drain`, INSIDE a request, where CPU is allocated.
//!
//! The scheduler contract, matching `amicu-infra/modules/scheduler`:
//!
//! | | |
//! |---|---|
//! | Method + path | `POST {service_url}/internal/outbox/drain` |
//! | Cadence | every minute (`* * * * *`), 30s attempt deadline |
//! | Auth | `Authorization: Bearer <Google-signed OIDC ID token>` |
//! | `iss` | `https://accounts.google.com` (or the bare `accounts.google.com`) |
//! | `aud` | `AMICU_INTERNAL_AUDIENCE`, defaulting to the effective `base_url` - the Cloud Run SERVICE URL, **not** the `/internal/outbox/drain` path |
//! | Caller | the token's verified `email` must be listed in `AMICU_INTERNAL_CALLER_EMAILS` |
//! | Response | `200` + JSON counts (`claimed`/`sent`/`failed`/`dead_lettered`/`budget_exhausted`) |
//!
//! A self-hoster with no Google identity can instead set the
//! `AMICU_INTERNAL_TOKEN` **environment variable** (env only - never a flag: a
//! flag value is visible in the process argument list) and present it as a
//! static bearer token; it is compared in constant time. Both mechanisms may be
//! configured at once. With NEITHER configured the endpoint answers `503` and
//! the bootstrap says so loudly at startup - in `production` it additionally
//! warns that queued mail will pile up undelivered. The route is deliberately
//! absent from the generated OpenAPI document, exactly like `/healthz`.
//!
//! `amicu-server outbox-drain` runs one pass from the command line through the
//! same [`amicu_api::drain_once`], for local development or a cron-driven
//! self-host. See [`outbox_drain`].
//!
//! # Billing (Stripe) - env only
//!
//! Billing is a real Stripe integration that activates only when its secrets are
//! present in the environment (there are no flags). Absent them, `/billing/*`
//! endpoints return a clean `503 { "error": "billing is not configured" }`.
//!
//! | Env var | Required | Purpose |
//! |---------|----------|---------|
//! | `STRIPE_SECRET_KEY` | yes | Server-side Stripe API key (`sk_…`). |
//! | `STRIPE_WEBHOOK_SECRET` | yes | Endpoint signing secret (`whsec_…`) for webhook verification. |
//! | `STRIPE_PRICE_PRO` | one price needed | Stripe price id granting the Pro tier. |
//! | `STRIPE_PRICE_ENTERPRISE` | optional | Price id granting Enterprise. |
//! | `STRIPE_PRICE_HOSTED` | optional | Price id granting Hosted. |
//! | `STRIPE_SUCCESS_URL` | optional | Checkout success redirect (defaults to localhost dev). |
//! | `STRIPE_CANCEL_URL` | optional | Checkout cancel redirect. |
//! | `STRIPE_PORTAL_RETURN_URL` | optional | Billing-portal return URL. |
//!
//! A subscription change takes effect on the user's **next token issuance**
//! (sign-in or refresh); live access JWTs are never revoked.
//!
//! # Cloudflare edge trust - env only (andypalmi/amicu-planning#142)
//!
//! Like the Stripe secrets above, `AMICU_EDGE_TOKEN` is read directly from the
//! environment (never a flag, so it never appears in `--help` output or a
//! process listing) rather than living on [`Config`]. `AMICU_REQUIRE_EDGE_TOKEN`
//! is a plain boolean, read the same permissive-token way
//! `AMICU_REQUIRE_EMAIL_VERIFICATION` is (`1`/`true`/`yes`/`on`,
//! case-insensitive; anything else, including unset, is `false`).
//!
//! | Env var | Required | Purpose |
//! |---------|----------|---------|
//! | `AMICU_EDGE_TOKEN` | only if `AMICU_REQUIRE_EDGE_TOKEN=true` | Shared secret the Cloudflare api-proxy Worker attaches as `X-Amicu-Edge-Token` on every request it forwards (`wrangler secret put EDGE_TOKEN` on the Worker side; the same value goes in Secret Manager's `AMICU_EDGE_TOKEN` for Cloud Run - see `docs/deploy/cloud-run.md`). Never logged. |
//! | `AMICU_REQUIRE_EDGE_TOKEN` | no (default `false`) | When `true`, every request without a valid `X-Amicu-Edge-Token` is rejected `403` before any handler runs (`GET /healthz`/`GET /readyz` exempt - Cloud Run's own probes carry no such header). When `false` (the default - this is "optional hardening" per the issue, not a hard requirement), a valid token still upgrades the credential rate limiter's client-IP source to `CF-Connecting-IP` (see below), but an invalid/missing one is simply ignored rather than rejected. |
//!
//! Default `false`, in EVERY environment including `AMICU_ENV=production`, not
//! just dev: unlike `DATABASE_URL`/RLS-bypass, turning this on requires an
//! operator to have actually provisioned the Worker + token pair first (it is
//! explicitly gated behind `optional_secrets_enabled.AMICU_EDGE_TOKEN` in
//! amicu-infra), so defaulting it to `true` would break the very first
//! production boot before that provisioning step ever runs. Booting with
//! `AMICU_REQUIRE_EDGE_TOKEN=true` and no `AMICU_EDGE_TOKEN` configured is a
//! hard startup error ([`Error::MissingEdgeToken`]) in every environment - such
//! a deployment could never serve a single public request, in dev or
//! production alike, so refusing to boot is strictly better than serving 403
//! to everything.
//!
//! Independent of the token: `AMICU_TRUSTED_PROXIES` (above) keeps governing
//! the non-Cloudflare `X-Forwarded-For` path - the peer Cloud Run sees is
//! always Google's own load balancer, which appends to `X-Forwarded-For`; the
//! Worker sets `X-Amicu-Edge-Token` and lets `CF-Connecting-IP` (set by the
//! Cloudflare edge itself, never the client) carry the real client IP instead
//! - see [`amicu_api::edge`]'s docs for the full request-chain rationale.

pub mod signing_key;
pub mod smtp;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use amicu_api::{
    AuthApiState, BillingApiState, ConfigApiState, DrainAuth, EdgeGateState, EdgeTrust,
    EngineApiState, HealthApiState, InMemoryOrgStore, InMemoryRateLimiter, OrgApiState, OrgStore,
    OutboxApiState, PostgresRateLimiter, PostgresTenancyStore, ProxyParseError, RateLimiter,
    SessionApiState, TenancyStore, TrustedProxies, api_router, edge_token_gate, health_router,
    outbox_router, stripe_config_from_env,
};
use amicu_auth::Tier;
use amicu_auth::google::{GoogleIdTokenConfig, GoogleIdTokenVerifier, HttpGoogleCertsSource};
use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::membership::MembershipStore;
use amicu_auth::notify::{LoggingNotifier, Notifier};
use amicu_auth::refresh::{InMemoryRefreshStore, RefreshStore};
use amicu_auth::reset::{InMemoryResetStore, ResetStore};
use amicu_auth::service::AuthService;
use amicu_auth::user::{InMemoryUserStore, UserStore};
use amicu_auth::verifier::{KeyringJwksProvider, Verifier, VerifierConfig};
use amicu_auth::verify::{InMemoryVerifyStore, VerifyStore};
use amicu_core::secret::SecretValue;
use amicu_state::{InMemoryStateBackend, StateBackendAdapter};
use amicu_store::config::ConfigStore;
use amicu_store::{
    InMemoryConfigStore, PgPool, PoolConfig, PostgresConfigStore, PostgresInviteStore,
    PostgresMembershipStore, PostgresRateLimitStore, PostgresStateBackend, PostgresWorkspaceStore,
};
use axum::Router;
use axum::middleware::from_fn_with_state;
use clap::{Parser, ValueEnum};
use tokio::net::TcpListener;

/// Amicu Management API server.
///
/// Every option is populated from a flag or the matching `AMICU_*` environment
/// variable (flags win); each lists its env var and default below. Runs with no
/// configuration at all for local end-to-end testing. Dev/single-node
/// bootstrap: state lives in in-memory stores and is lost on restart. See the
/// crate-level docs for the full configuration table.
#[derive(Debug, Clone, Parser)]
#[command(name = "amicu-server", version)]
pub struct Config {
    /// Socket address to bind (host:port).
    ///
    /// Precedence: an explicit `--bind`/`AMICU_BIND` value always wins;
    /// otherwise `PORT` (Cloud Run's runtime contract - the port the platform
    /// assigns and expects the container to listen on) binds `0.0.0.0:$PORT`;
    /// otherwise the default `0.0.0.0:8080`. Left unset (the default) so
    /// `PORT` is honoured; set this explicitly only to override both.
    #[arg(long, env = "AMICU_BIND")]
    pub bind: Option<SocketAddr>,

    /// Token `iss` the server issues and the engine gate requires.
    ///
    /// The dev default matches the web BFF's zero-config issuer
    /// (`API_URL` ≈ `http://localhost:8080`) so local end-to-end sign-in works
    /// with no env set; production overrides it with the real issuer URL.
    #[arg(long, env = "AMICU_ISSUER", default_value = "http://localhost:8080")]
    pub issuer: String,

    /// Token `aud` the server issues and the engine gate requires.
    ///
    /// The dev default matches the web BFF's zero-config audience (`amicu`) so
    /// its offline token verification accepts tokens this server issues.
    #[arg(long, env = "AMICU_AUDIENCE", default_value = "amicu")]
    pub audience: String,

    /// Public base URL advertised in the OIDC discovery document (its
    /// `jwks_uri` etc.). Defaults to the `issuer` value when unset.
    #[arg(long, env = "AMICU_BASE_URL")]
    pub base_url: Option<String>,

    /// Comma-separated trusted reverse-proxy allowlist (CIDRs or bare IPs). Only
    /// from these peers is `X-Forwarded-For` believed for rate-limit keying.
    /// Empty (the default) trusts no proxy.
    #[arg(long, env = "AMICU_TRUSTED_PROXIES", default_value = "")]
    pub trusted_proxies: String,

    /// Minimum access tier required to reach the engine endpoints.
    #[arg(long, env = "AMICU_MIN_TIER", value_enum, default_value_t = MinTier::Community)]
    pub min_tier: MinTier,

    /// Credential-route rate limit: max requests per window per client.
    #[arg(long, env = "AMICU_RATE_LIMIT_MAX", default_value_t = 30)]
    pub rate_limit_max: u32,

    /// Credential-route rate-limit window, in seconds.
    #[arg(long, env = "AMICU_RATE_LIMIT_WINDOW_SECS", default_value_t = 60)]
    pub rate_limit_window_secs: u64,

    /// How long the Postgres-backed rate limiter's `check` waits for the
    /// database hit before failing OPEN, in milliseconds. Deliberately short
    /// (and independent of `db_acquire_timeout_secs`, mirroring
    /// `readyz_timeout_ms`'s relationship to it): a saturated pool or a
    /// slow/hung database should fail a credential request's rate-limit
    /// check fast, not block it up to the full connection-acquire timeout.
    /// Only consulted on the Postgres path - the in-memory limiter never
    /// awaits, so it has nothing to time out.
    #[arg(long, env = "AMICU_RATE_LIMIT_TIMEOUT_MS", default_value_t = 1_000)]
    pub rate_limit_timeout_ms: u64,

    /// Per-EMAIL budget for `/recover` and `/verify/resend`, per
    /// `email_rate_limit_window_secs` (andypalmi/amicu-planning#194). On top of
    /// the per-IP limit; shared across replicas on the Postgres path.
    #[arg(long, env = "AMICU_EMAIL_RATE_LIMIT_MAX", default_value_t = 5)]
    pub email_rate_limit_max: u32,

    /// Per-EMAIL budget for `/signin` / `/token` per window. A loose ceiling;
    /// the per-account exponential backoff is the tight control.
    #[arg(long, env = "AMICU_SIGNIN_EMAIL_RATE_LIMIT_MAX", default_value_t = 100)]
    pub signin_email_rate_limit_max: u32,

    /// Window for both per-email limits, seconds.
    #[arg(
        long,
        env = "AMICU_EMAIL_RATE_LIMIT_WINDOW_SECS",
        default_value_t = 900
    )]
    pub email_rate_limit_window_secs: u64,

    /// Cap on the TOTAL number of workspaces a user may OWN (across every
    /// workspace, including the personal workspace sign-up provisions), enforced
    /// on `POST /workspaces`. There is no `DELETE /workspaces`, so this is a hard
    /// ceiling per owner, enforced inside the creating transaction
    /// (andypalmi/amicu-planning#132). Only reachable on the Postgres path
    /// (`DATABASE_URL` set); the in-memory dev path is single-personal-workspace
    /// by construction.
    #[arg(
        long,
        env = "AMICU_MAX_WORKSPACES_PER_USER",
        default_value_t = amicu_store::DEFAULT_MAX_WORKSPACES_PER_USER
    )]
    pub max_workspaces_per_user: u32,

    /// Deployment environment gate. In `production`, a missing `DATABASE_URL`
    /// and a connected Postgres role that would bypass row-level security are
    /// hard startup errors; in `development` (the default) both are only
    /// warnings, so local runs and pre-database self-host stay zero-config.
    #[arg(long, env = "AMICU_ENV", value_enum, default_value_t = Environment::Development)]
    pub environment: Environment,

    /// Maximum size of the single shared Postgres pool (see
    /// [`amicu_store::PoolConfig`]). Kept small by default: many autoscaled
    /// Cloud Run replicas, each holding one pool, share one Cloud SQL
    /// connection ceiling.
    #[arg(long, env = "AMICU_DB_MAX_CONNECTIONS", default_value_t = 5)]
    pub db_max_connections: u32,

    /// Minimum size of the single shared Postgres pool, kept warm even when
    /// idle. Default `0`: Cloud Run scales replicas to zero, so an idle
    /// replica should hold no idle connections either.
    #[arg(long, env = "AMICU_DB_MIN_CONNECTIONS", default_value_t = 0)]
    pub db_min_connections: u32,

    /// How long a caller waits for a pool connection to become available
    /// before giving up, in seconds.
    #[arg(long, env = "AMICU_DB_ACQUIRE_TIMEOUT_SECS", default_value_t = 10)]
    pub db_acquire_timeout_secs: u64,

    /// How long `GET /readyz` waits for the Postgres pool to answer `SELECT
    /// 1` before reporting `503`, in milliseconds. Deliberately short (and
    /// independent of `db_acquire_timeout_secs`): a saturated pool should
    /// fail the readiness probe fast, not block it up to the full connection
    /// acquire timeout.
    #[arg(long, env = "AMICU_READYZ_TIMEOUT_MS", default_value_t = 2_000)]
    pub readyz_timeout_ms: u64,

    /// PKCS#8 Ed25519 PEM for the ACTIVE signing key (mints access tokens and
    /// is the primary key advertised in JWKS). Injected from a secret manager;
    /// may contain either real newlines or literal `\n` escapes (both are
    /// normalised — see the `signing_key` module docs). Mutually exclusive
    /// with `signing_key_pem_file` (setting both is a startup error). In
    /// `production` a missing key is a hard startup error
    /// ([`Error::MissingSigningKey`]); in `development` (the default) it falls
    /// back to an ephemeral `SigningKey::generate()`, with a loud warning —
    /// unchanged dev UX.
    ///
    /// Wrapped in [`signing_key::RedactedEnvString`], not a bare `String`:
    /// `Config` derives `Debug`/`Clone` like every other field, and this one
    /// carries private key material — see that type's docs for why a bare
    /// `String` here would be a leak vector.
    #[arg(
        long,
        env = "AMICU_SIGNING_KEY_PEM",
        hide_env_values = true,
        hide = true
    )]
    pub signing_key_pem: Option<signing_key::RedactedEnvString>,

    /// Path to a file containing the ACTIVE signing key PEM (a mounted secret
    /// volume), as an alternative to `AMICU_SIGNING_KEY_PEM`. Mutually
    /// exclusive with it.
    #[arg(long, env = "AMICU_SIGNING_KEY_PEM_FILE", hide_env_values = true)]
    pub signing_key_pem_file: Option<String>,

    /// Zero or more PKCS#8 Ed25519 PEM blocks (concatenated, split on
    /// `-----BEGIN`) for RETIRING keys: verify-only, never sign, kept
    /// published in JWKS so tokens they already signed keep verifying until
    /// they expire. Optional; empty/unset means no retiring keys. See
    /// `docs/ops/signing-key-rotation.md`.
    #[arg(
        long,
        env = "AMICU_SIGNING_KEYS_RETIRING_PEM",
        hide_env_values = true,
        hide = true
    )]
    pub signing_keys_retiring_pem: Option<signing_key::RedactedEnvString>,

    /// Path to a file containing the RETIRING keys' concatenated PEM blocks,
    /// as an alternative to `AMICU_SIGNING_KEYS_RETIRING_PEM`. Mutually
    /// exclusive with it.
    #[arg(
        long,
        env = "AMICU_SIGNING_KEYS_RETIRING_PEM_FILE",
        hide_env_values = true
    )]
    pub signing_keys_retiring_pem_file: Option<String>,

    /// Required `aud` claim on the Google-issued OIDC ID token that
    /// authenticates `POST /internal/outbox/drain`
    /// (andypalmi/amicu-planning#141). Defaults to the effective `base_url`
    /// when unset, which is what `amicu-infra`'s `modules/scheduler` mints by
    /// default (the Cloud Run SERVICE URL - not the `/internal/outbox/drain`
    /// path - matching Google's own documented pattern).
    #[arg(long, env = "AMICU_INTERNAL_AUDIENCE")]
    pub internal_audience: Option<String>,

    /// Comma-separated allowlist of `email` claims accepted on that OIDC
    /// token: the Cloud Scheduler invoker service account(s), e.g.
    /// `amicu-scheduler@<project>.iam.gserviceaccount.com`. Empty (the
    /// default) disables the OIDC path entirely - it never means "accept
    /// anyone".
    #[arg(long, env = "AMICU_INTERNAL_CALLER_EMAILS", default_value = "")]
    pub internal_caller_emails: String,

    /// Maximum number of queued emails one `POST /internal/outbox/drain`
    /// request claims and attempts. Clamped to a sane range by
    /// [`amicu_api::OutboxApiState::with_batch_size`]; whatever is left over
    /// is picked up by the next invocation a minute later.
    #[arg(
        long,
        env = "AMICU_OUTBOX_DRAIN_BATCH_SIZE",
        default_value_t = amicu_api::DEFAULT_DRAIN_BATCH_SIZE
    )]
    pub outbox_drain_batch_size: u32,

    /// Wall-clock budget for one `POST /internal/outbox/drain` request, in
    /// milliseconds. The drain stops claiming and sending once this is spent
    /// and returns partial counts, so it always answers well inside Cloud
    /// Scheduler's 30s attempt deadline.
    #[arg(
        long,
        env = "AMICU_OUTBOX_DRAIN_BUDGET_MS",
        default_value_t = amicu_api::DEFAULT_DRAIN_BUDGET_MS
    )]
    pub outbox_drain_budget_ms: u64,
}

/// Deployment environment gate (`AMICU_ENV`): whether production safety
/// checks are enforced (a hard startup error) or merely warned about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Environment {
    /// Zero-config local/self-host default: a missing `DATABASE_URL` falls
    /// back to ephemeral in-memory stores, and an RLS-bypassing connected
    /// role (superuser, or the tenant tables' owner) only logs a warning.
    #[default]
    Development,
    /// Production safety gate: a missing `DATABASE_URL`, or a connected role
    /// that would bypass row-level security, is a hard non-zero-exit startup
    /// error instead of a silent fallback.
    Production,
}

/// The engine-gate minimum tier, as a CLI/env value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MinTier {
    /// Any authenticated user (open-core default).
    Community,
    /// Paid individual/team tier and above.
    Pro,
    /// Enterprise tier and above.
    Enterprise,
    /// Amicu-hosted tier only.
    Hosted,
}

impl From<MinTier> for Tier {
    fn from(t: MinTier) -> Self {
        match t {
            MinTier::Community => Tier::Community,
            MinTier::Pro => Tier::Pro,
            MinTier::Enterprise => Tier::Enterprise,
            MinTier::Hosted => Tier::Hosted,
        }
    }
}

/// A failure assembling or running the server.
#[derive(Debug)]
pub enum Error {
    /// A trusted-proxy allowlist entry was not a valid CIDR or IP.
    Proxy(ProxyParseError),
    /// `AMICU_ENV=production` but no active signing key was configured
    /// (`AMICU_SIGNING_KEY_PEM` / `AMICU_SIGNING_KEY_PEM_FILE`). A production
    /// deploy may never silently fall back to an ephemeral per-boot key that
    /// would differ across replicas and not survive a restart.
    MissingSigningKey,
    /// A signing-key PEM (active or retiring) failed to load: not valid
    /// PKCS#8 Ed25519, a `_FILE` path that could not be read, both the value
    /// and file variant of the same key set, or a duplicate `kid` across the
    /// active/retiring set. See the `signing_key` module docs.
    SigningKeyConfig(signing_key::SigningKeyConfigError),
    /// The persistent store could not connect or migrate (only reachable when
    /// `DATABASE_URL` is set).
    Store(amicu_store::StoreError),
    /// The SMTP notifier could not be configured (only reachable when the
    /// `AMICU_SMTP_*` environment is present).
    Smtp(smtp::SmtpConfigError),
    /// `PORT` was set but was not a valid `u16`. The offending value is
    /// echoed (a port number is not a secret).
    Port(String),
    /// `AMICU_ENV=production` but no `DATABASE_URL` was set. A production
    /// deploy may never silently fall back to ephemeral in-memory stores.
    MissingDatabaseUrl,
    /// `AMICU_ENV=production` but `AMICU_SECURITY_HASH_KEY` is unset or shorter
    /// than 32 bytes (andypalmi/amicu-planning#194). Without a secret key the
    /// pseudonyms in security events and rate-limit keys are guessable.
    MissingSecurityHashKey,
    /// `AMICU_ENV=production` and the connected Postgres role would bypass
    /// row-level security (superuser/BYPASSRLS, direct or inherited, or owns
    /// a `FORCE ROW LEVEL SECURITY` table).
    RlsBypass(String),
    /// `AMICU_ENV=production` and the schema has migrations the connected
    /// (non-owning) role cannot apply itself - run `amicu-server migrate` as
    /// the schema-owning role first (see the crate's two-role production
    /// model docs).
    PendingMigrations(usize),
    /// `amicu-server migrate` connected as a role with no `CREATE` privilege
    /// on the `public` schema - it cannot be the schema-owning migrator role
    /// (see `ops/sql/provision_app_role.sql`). The role name is echoed (not
    /// a secret).
    MigratorLacksCreate(String),
    /// `AMICU_REQUIRE_EDGE_TOKEN=true` but `AMICU_EDGE_TOKEN` is unset (in ANY
    /// environment, not just production - andypalmi/amicu-planning#142): a
    /// deployment in this state would reject every public request
    /// unconditionally, so refusing to boot is strictly better than serving
    /// `403` to everything.
    MissingEdgeToken,
    /// Binding or serving the socket failed.
    Io(std::io::Error),
    /// `amicu-server gen-signing-key` could not generate a fresh key. In
    /// practice unreachable outside an exhausted/broken OS RNG or a PKCS#8
    /// encoding failure.
    KeyGen(amicu_auth::error::AuthError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Proxy(e) => write!(f, "trusted-proxy config: {e}"),
            Error::MissingSigningKey => write!(
                f,
                "AMICU_ENV=production requires an active signing key: set AMICU_SIGNING_KEY_PEM or \
                 AMICU_SIGNING_KEY_PEM_FILE (production may not silently fall back to an ephemeral \
                 per-boot key)"
            ),
            Error::SigningKeyConfig(e) => write!(f, "signing key configuration: {e}"),
            Error::Store(e) => write!(f, "persistent store: {e}"),
            Error::Smtp(e) => write!(f, "smtp notifier config: {e}"),
            Error::Port(v) => write!(f, "PORT is not a valid port: {v}"),
            Error::MissingDatabaseUrl => write!(
                f,
                "AMICU_ENV=production requires DATABASE_URL to be set (production may not \
                 silently fall back to ephemeral in-memory stores)"
            ),
            Error::MissingSecurityHashKey => write!(
                f,
                "AMICU_ENV=production requires AMICU_SECURITY_HASH_KEY (at least 32 bytes, the \
                 same secret value on every replica)"
            ),
            Error::RlsBypass(msg) => write!(f, "RLS-bypass startup guard: {msg}"),
            Error::PendingMigrations(n) => write!(
                f,
                "{n} pending migration(s) - run `amicu-server migrate` with the schema-owning \
                 role's credentials before starting in production (see \
                 ops/sql/provision_app_role.sql)"
            ),
            Error::MigratorLacksCreate(role) => write!(
                f,
                "role '{role}' lacks CREATE on schema public - use the owner/migrator role's \
                 DATABASE_URL (see ops/sql/provision_app_role.sql)"
            ),
            Error::MissingEdgeToken => write!(
                f,
                "AMICU_REQUIRE_EDGE_TOKEN=true requires AMICU_EDGE_TOKEN to be set (this \
                 deployment would otherwise reject every public request with 403)"
            ),
            Error::Io(e) => write!(f, "server socket: {e}"),
            Error::KeyGen(e) => write!(f, "signing-key generation: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Proxy(e) => Some(e),
            Error::SigningKeyConfig(e) => Some(e),
            Error::Store(e) => Some(e),
            Error::Smtp(e) => Some(e),
            Error::Io(e) => Some(e),
            Error::KeyGen(e) => Some(e),
            Error::Port(_)
            | Error::MissingSigningKey
            | Error::MissingDatabaseUrl
            | Error::MissingSecurityHashKey
            | Error::RlsBypass(_)
            | Error::PendingMigrations(_)
            | Error::MigratorLacksCreate(_)
            | Error::MissingEdgeToken => None,
        }
    }
}

impl From<ProxyParseError> for Error {
    fn from(e: ProxyParseError) -> Self {
        Error::Proxy(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// The assembled app plus the shared Postgres pool, if one was connected
/// (`None` on the in-memory dev path).
///
/// A struct rather than a bare [`Router`] so [`run`] can close the pool after
/// [`serve`] returns (andypalmi/amicu-planning#139); the `pool` field is
/// crate-private because callers outside this crate only ever need `.router`
/// (see the smoke test).
#[derive(Debug)]
pub struct BuiltApp {
    /// The full API router, including the health/readiness routes. Pass to
    /// [`serve`].
    pub router: Router,
    pool: Option<PgPool>,
}

/// Assemble the full API [`Router`] from `config`, wiring the auth stores, a
/// freshly generated signing key, and the engine gate.
///
/// # Store selection and the shared pool
///
/// If `DATABASE_URL` is set, ONE Postgres pool is opened and migrated (see
/// [`amicu_store::connect_with`] / [`PoolConfig`], tuned from `config`'s
/// `db_*` fields) and shared - via `pool.clone()` - by every store that needs
/// it: the auth quartet (users, refresh tokens, reset tokens, membership), the
/// org console, tenancy, the config store, and the state backend. Otherwise
/// the dev/single-node in-memory stores are used so local runs, tests, and
/// pre-database self-host work with zero configuration.
///
/// # Production gate (`AMICU_ENV`)
///
/// In `production` (see [`Environment`]), a missing `DATABASE_URL` is a hard
/// startup error ([`Error::MissingDatabaseUrl`]) rather than a silent
/// in-memory fallback, and - once connected - a role that would bypass
/// row-level security (a superuser, or the owner of the tenant tables) is
/// also a hard startup error ([`Error::RlsBypass`]); the RLS design assumes
/// the app connects as the non-superuser, non-owning `amicu_app` role (see
/// [`amicu_store::probe_rls_bypass_risk`]). In `development` (the default)
/// both conditions are logged as warnings only.
///
/// # Email delivery
///
/// If ANY `AMICU_SMTP_*` variable is set, a real
/// [`SmtpNotifier`](smtp::SmtpNotifier) sends verification and reset emails over
/// TLS, and an incomplete required set (missing `AMICU_SMTP_HOST`,
/// `AMICU_SMTP_FROM`, or `AMICU_WEB_URL`) is a hard startup error
/// ([`Error::Smtp`]); never a silent fall back. Only when NO `AMICU_SMTP_*`
/// variable is set at all is the dev [`LoggingNotifier`] wired, which prints
/// reset/verify tokens to stderr and is explicitly NOT for production. Failing
/// loud on a partial configuration is what stops those raw tokens reaching the
/// logs of a deployment the operator believes is sending real SMTP email.
///
/// The engine gate verifies bearer tokens against a **live** [`KeyringJwksProvider`]
/// backed by the exact same keyring the auth service signs with (see
/// [`amicu_auth::service::AuthService::keyring_handle`]), so a runtime
/// rotation (`AuthService::rotate_keys`) propagates with no restart and no
/// JWKS round-trip — bounded by the verifier's own cache (worst case ~60s to
/// pick up a new active key under an unknown-`kid` flood; up to 5 minutes,
/// `VerifierConfig::cache_ttl`, for a retired key to stop verifying) rather
/// than instant (andypalmi/amicu-planning#140).
///
/// # Errors
/// Returns [`Error::Proxy`] if the trusted-proxy allowlist fails to parse,
/// [`Error::MissingSigningKey`] if `AMICU_ENV=production` and no active
/// signing key is configured, or [`Error::SigningKeyConfig`] if a configured
/// signing-key PEM is invalid, a `_FILE` path cannot be read, both a value
/// and a file source are set for the same key, or two keys share a `kid`,
/// [`Error::Store`] if `DATABASE_URL` is set but the pool cannot connect (or,
/// in `development`, migrate), [`Error::MissingDatabaseUrl`] if
/// `AMICU_ENV=production` but no `DATABASE_URL` is set,
/// [`Error::PendingMigrations`] if `AMICU_ENV=production` and the schema has
/// migrations not yet applied by a separate `amicu-server migrate` run,
/// [`Error::RlsBypass`] if `AMICU_ENV=production` and the connected role
/// would bypass row-level security, or [`Error::Smtp`] if any
/// `AMICU_SMTP_*` variable is set but the SMTP configuration is incomplete or
/// invalid.
pub async fn build_app(config: &Config) -> Result<BuiltApp, Error> {
    build_app_with_database_url(config, std::env::var("DATABASE_URL").ok().as_deref()).await
}

/// Like [`build_app`], but forces the in-memory stores regardless of any
/// `DATABASE_URL` set in the process environment.
///
/// `build_app` reads `DATABASE_URL` straight from the process environment,
/// which is the right default for `run` (a real deployment). It is the wrong
/// default for tests: `std::env::var` is ambient, process-wide state, so a
/// shell that happens to export `DATABASE_URL` (e.g. after following the dev
/// Postgres setup in the README) would silently flip in-process smoke/unit
/// tests onto Postgres instead of the zero-config in-memory stores they
/// expect. Tests that want the in-memory path unconditionally - i.e. nearly
/// all of them - should call this instead of `build_app` so an ambient
/// `DATABASE_URL` can never hijack them.
pub async fn build_app_in_memory(config: &Config) -> Result<BuiltApp, Error> {
    build_app_with_database_url(config, None).await
}

/// Applies the production gate, opens the single shared Postgres pool if
/// `database_url` is set, and runs the RLS-bypass startup guard against it
/// (andypalmi/amicu-planning#139).
///
/// # Production gate
/// In `production` (see [`Environment`]), a missing `database_url` is a hard
/// startup error ([`Error::MissingDatabaseUrl`]): a production deploy may
/// never silently fall back to ephemeral in-memory stores just because
/// `DATABASE_URL` was left unset by mistake. Checked FIRST, before any
/// connection attempt.
///
/// # Shared pool, and the two-role production model
/// Connects ONCE; the returned `PgPool` is a cheap `Arc`-backed handle
/// callers `clone()` for every store that needs it instead of each opening
/// its own pool. Sized from `config`'s `db_*` fields (see [`PoolConfig`]'s
/// docs): kept small by default, since many autoscaled Cloud Run replicas,
/// each holding one pool, share one Cloud SQL connection ceiling.
///
/// Whether migrations run here depends on `environment`:
/// - `development`: single-role convenience (see
///   `docker/postgres-init/01-app-role.sql`) - the one connecting role also
///   owns the schema, so migrations run right here, at boot, same as before
///   andypalmi/amicu-planning#139.
/// - `production`: two-role model (see [`migrate`] and
///   `ops/sql/provision_app_role.sql`) - a separate `amicu-server migrate`
///   run, connected as the schema-OWNING role, is expected to have already
///   applied every migration. This function does NOT migrate; it only
///   verifies nothing is pending ([`amicu_store::pending_migrations`]) and
///   fails fast ([`Error::PendingMigrations`]) otherwise, so a forgotten
///   migration step is a clear startup error instead of either racing
///   concurrent replicas against each other or serving against a stale
///   schema.
///
/// # RLS-bypass guard
/// The RLS design assumes the running service connects as a role with
/// neither `SUPERUSER` nor `BYPASSRLS` (direct or inherited) and that owns no
/// `FORCE ROW LEVEL SECURITY` table (andypalmi/amicu-planning#114); either
/// condition silently bypasses every FORCE RLS policy
/// ([`amicu_store::probe_rls_bypass_risk`]) - refuse to start on that in
/// production ([`Error::RlsBypass`]); only warn in development.
async fn connect_shared_pool(
    config: &Config,
    database_url: Option<&str>,
) -> Result<Option<PgPool>, Error> {
    if config.environment == Environment::Production && database_url.is_none() {
        return Err(Error::MissingDatabaseUrl);
    }

    let pool: Option<PgPool> = match database_url {
        Some(url) => {
            let pool_config = PoolConfig {
                max_connections: config.db_max_connections,
                min_connections: config.db_min_connections,
                acquire_timeout: Duration::from_secs(config.db_acquire_timeout_secs),
            };
            let pool = amicu_store::connect_with(url, pool_config)
                .await
                .map_err(Error::Store)?;

            match config.environment {
                // Single-role dev convenience: the one connecting role also
                // owns the schema (docker/postgres-init/01-app-role.sql), so
                // migrate right here, at boot.
                Environment::Development => {
                    amicu_store::run_migrations(&pool)
                        .await
                        .map_err(Error::Store)?;
                    eprintln!(
                        "amicu-server: connected + migrated the shared Postgres pool \
                         (development; max_connections={}, min_connections={}, \
                         acquire_timeout={}s).",
                        pool_config.max_connections,
                        pool_config.min_connections,
                        pool_config.acquire_timeout.as_secs(),
                    );
                }
                // Two-role production model: `amicu-server migrate`, run
                // separately as the schema-owning role, is responsible for
                // applying migrations - the runtime here connects as a
                // distinct, non-owning role and must NOT migrate at boot; it
                // only verifies the schema is already up to date.
                Environment::Production => {
                    let pending = amicu_store::pending_migrations(&pool)
                        .await
                        .map_err(Error::Store)?;
                    if !pending.is_empty() {
                        return Err(Error::PendingMigrations(pending.len()));
                    }
                    eprintln!(
                        "amicu-server: connected the shared Postgres pool (production; \
                         schema up to date, NOT migrating at boot - see `amicu-server \
                         migrate`; max_connections={}, min_connections={}, \
                         acquire_timeout={}s).",
                        pool_config.max_connections,
                        pool_config.min_connections,
                        pool_config.acquire_timeout.as_secs(),
                    );
                }
            }

            Some(pool)
        }
        None => None,
    };

    if let Some(pool) = pool.as_ref() {
        let risk = amicu_store::probe_rls_bypass_risk(pool)
            .await
            .map_err(Error::Store)?;
        if risk.bypasses_rls() {
            let mut reasons = Vec::new();
            if !risk.bypass_via_roles.is_empty() {
                reasons.push(format!(
                    "is a member of superuser/BYPASSRLS role(s) {:?}",
                    risk.bypass_via_roles
                ));
            }
            if !risk.owned_forced_rls_tables.is_empty() {
                reasons.push(format!(
                    "OWNS FORCE-RLS table(s) {:?}",
                    risk.owned_forced_rls_tables
                ));
            }
            let msg = format!(
                "connected role '{}' {}: row-level security would be bypassed. In production, \
                 connect as a separate, non-owning role with neither SUPERUSER nor BYPASSRLS \
                 (direct or inherited) - see ops/sql/provision_app_role.sql and \
                 andypalmi/amicu-planning#114.",
                risk.current_user,
                reasons.join(" and "),
            );
            match config.environment {
                Environment::Production => return Err(Error::RlsBypass(msg)),
                Environment::Development => {
                    eprintln!(
                        "amicu-server: WARNING - {msg} Allowed in development; this fails \
                         startup in production (AMICU_ENV=production)."
                    );
                }
            }
        }
    }

    Ok(pool)
}

/// Generates a fresh Ed25519 PKCS#8 signing key and prints ONLY its PEM to
/// stdout, then returns - `amicu-server gen-signing-key` (see `main.rs`'s
/// `Command::GenSigningKey`). Nothing else touches stdout: the output is
/// meant to be piped straight into a secret manager, e.g.
///
/// ```sh
/// amicu-server gen-signing-key | gcloud secrets versions add \
///     AMICU_SIGNING_KEY_PEM --data-file=-
/// ```
///
/// See `docs/ops/signing-key-rotation.md` for the full operational rotation
/// procedure (generate, set the old key as retiring, promote the new key to
/// active, drop the old one once it has aged out).
///
/// # Errors
/// Returns [`Error::KeyGen`] if key generation or PKCS#8 PEM encoding fails
/// (in practice unreachable outside an exhausted/broken OS RNG).
pub fn gen_signing_key() -> Result<(), Error> {
    let pem = amicu_auth::generate_pkcs8_pem().map_err(Error::KeyGen)?;
    let text = pem
        .expose_secret_str()
        .expect("generate_pkcs8_pem always returns valid UTF-8 PEM text");
    print!("{text}");
    Ok(())
}

/// Runs ONLY the embedded schema migrations against `DATABASE_URL`, as the
/// schema-OWNING role, and exits - no RLS-bypass guard (the migrator is
/// expected to own the schema; see the crate's "two-role production model"
/// docs and `ops/sql/provision_app_role.sql`). This is `amicu-server migrate`
/// (see `main.rs`'s `Command::Migrate`).
///
/// # Errors
/// Returns [`Error::MissingDatabaseUrl`] if `DATABASE_URL` is unset or empty,
/// or [`Error::Store`] if the connection or migration fails.
pub async fn migrate() -> Result<(), Error> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty());
    migrate_database_url(url.as_deref()).await
}

/// Like [`migrate`], but takes `database_url` explicitly rather than reading
/// it from the ambient environment - the same env-avoidance pattern
/// [`build_app_with_database_url`] uses, so tests can target a specific role
/// without mutating global process state (this workspace forbids `unsafe`
/// code, so `std::env::set_var` is unavailable to tests entirely).
///
/// # Errors
/// Returns [`Error::MissingDatabaseUrl`] if `database_url` is `None`,
/// [`Error::MigratorLacksCreate`] if the connected role has no `CREATE`
/// privilege on the `public` schema (it is not the schema-owning migrator
/// role - see `ops/sql/provision_app_role.sql`), or [`Error::Store`] if the
/// connection or migration fails.
pub async fn migrate_database_url(database_url: Option<&str>) -> Result<(), Error> {
    let Some(url) = database_url else {
        return Err(Error::MissingDatabaseUrl);
    };
    let pool = amicu_store::connect(url).await.map_err(Error::Store)?;

    // Fail with a specific, friendly error up front rather than a raw
    // permission-denied surfacing from deep inside the migration
    // transaction: a role with no CREATE on `public` cannot be the
    // schema-owning migrator this command expects to run as.
    let (current_user, can_create) = amicu_store::can_create_in_public_schema(&pool)
        .await
        .map_err(Error::Store)?;
    if !can_create {
        return Err(Error::MigratorLacksCreate(current_user));
    }

    let pending_count = amicu_store::pending_migrations(&pool)
        .await
        .map_err(Error::Store)?
        .len();
    amicu_store::run_migrations(&pool)
        .await
        .map_err(Error::Store)?;
    eprintln!(
        "amicu-server migrate: applied {pending_count} pending migration(s); schema now up to \
         date."
    );
    pool.close().await;
    Ok(())
}

/// Selects and constructs the credential-route rate limiter (see
/// [`build_app_with_database_url`]'s "Rate limiter selection" comment):
/// Postgres-backed and shared across every replica when `pool` is present,
/// in-memory (dev/tests/DB-less) otherwise. Split out of
/// [`build_app_with_database_url`] to keep that function under Clippy's
/// line-count lint.
fn build_rate_limiter(pool: Option<PgPool>, config: &Config) -> Arc<dyn RateLimiter> {
    let rate_limit_window = Duration::from_secs(config.rate_limit_window_secs);
    if let Some(pool) = pool {
        Arc::new(
            PostgresRateLimiter::new(
                PostgresRateLimitStore::new(pool),
                config.rate_limit_max,
                rate_limit_window,
            )
            .with_timeout(Duration::from_millis(config.rate_limit_timeout_ms)),
        )
    } else {
        Arc::new(InMemoryRateLimiter::new(
            config.rate_limit_max,
            rate_limit_window,
        ))
    }
}

/// The per-email limiters (`/recover` + `/verify/resend`, and `/signin`):
/// Postgres-backed (shared across replicas, fail-open on database error like
/// the per-IP limiter) when `pool` is present, in-memory otherwise.
fn build_email_limiters(
    pool: Option<&PgPool>,
    config: &Config,
) -> (Arc<dyn RateLimiter>, Arc<dyn RateLimiter>) {
    let window = Duration::from_secs(config.email_rate_limit_window_secs);
    let make = |max: u32| -> Arc<dyn RateLimiter> {
        if let Some(pool) = pool {
            Arc::new(
                PostgresRateLimiter::new(PostgresRateLimitStore::new(pool.clone()), max, window)
                    .with_timeout(Duration::from_millis(config.rate_limit_timeout_ms)),
            )
        } else {
            Arc::new(InMemoryRateLimiter::new(max, window))
        }
    };
    (
        make(config.email_rate_limit_max),
        make(config.signin_email_rate_limit_max),
    )
}

/// Shared implementation behind [`build_app`] and [`build_app_in_memory`].
/// `database_url` is an explicit override rather than an environment read, so
/// callers control store selection precisely: `Some(url)` selects the
/// Postgres stores, `None` selects the in-memory stores, with no ambient
/// environment lookup at all. Exposed (rather than crate-private) so tests
/// can exercise a specific `database_url` - including the two-role
/// production topology - through the exact same path `build_app` and
/// `build_app_in_memory` use, without mutating the ambient environment (this
/// workspace forbids `unsafe` code, so `std::env::set_var` is unavailable to
/// tests entirely).
pub async fn build_app_with_database_url(
    config: &Config,
    database_url: Option<&str>,
) -> Result<BuiltApp, Error> {
    /// The auth store quartet (user, refresh, reset, verify) plus the optional
    /// membership authority, selected together: all Postgres when `DATABASE_URL`
    /// is set, else all in-memory. The membership store is `None` in-memory - the
    /// service then derives each user's personal workspace from the user store
    /// (its `PersonalMembershipStore` default) - and the real multi-workspace
    /// `PostgresMembershipStore` on the database path.
    type AuthStores = (
        Arc<dyn UserStore>,
        Arc<dyn RefreshStore>,
        Arc<dyn ResetStore>,
        Arc<dyn VerifyStore>,
        Option<Arc<dyn MembershipStore>>,
    );

    // Production gate + single shared pool + RLS-bypass startup guard
    // (andypalmi/amicu-planning#139): see `connect_shared_pool`'s docs.
    let pool = connect_shared_pool(config, database_url).await?;

    let trusted = TrustedProxies::from_strs(config.trusted_proxies.split(','))?;

    // Cloudflare edge trust (andypalmi/amicu-planning#142): env-only, like the
    // Stripe/SMTP secrets above - see the crate docs' "Cloudflare edge trust"
    // section for why these two never become `Config` flags. Checked early,
    // before any store/signing-key work, alongside the other pure config
    // validation (trusted-proxy parsing) above.
    let edge_token = edge_token_from_env();
    let require_edge_token = env_flag_is_truthy("AMICU_REQUIRE_EDGE_TOKEN");
    if require_edge_token && edge_token.is_none() {
        return Err(Error::MissingEdgeToken);
    }
    let edge_trust = EdgeTrust::new(edge_token, require_edge_token);

    // Also the default `aud` the outbox-drain OIDC verifier enforces (see
    // `drain_auth`), so it is kept around rather than moved into the service.
    let effective_base_url = config
        .base_url
        .clone()
        .unwrap_or_else(|| config.issuer.clone());

    // Durable signing keyring (andypalmi/amicu-planning#140): the ACTIVE key
    // (and any RETIRING keys) come from AMICU_SIGNING_KEY_PEM /
    // AMICU_SIGNING_KEYS_RETIRING_PEM (or their _FILE variants) - identical
    // across every replica that shares the same secret-injected env, so every
    // replica signs and verifies with the same key set and advertises the
    // same JWKS. See `signing_key` module docs and
    // docs/ops/signing-key-rotation.md.
    let keyring = signing_key::load_keyring(config).map_err(|e| match e {
        signing_key::SigningKeyConfigError::MissingActiveKey => Error::MissingSigningKey,
        other => Error::SigningKeyConfig(other),
    })?;

    let issuer = TokenIssuer::new(IssuerConfig::new(&config.issuer, &config.audience));

    // Store selection. When the shared pool is present (DATABASE_URL was set),
    // use the durable Postgres stores over it; otherwise fall back to the
    // dev/single-node in-memory stores so local runs and tests need zero
    // configuration. The user store is shared between the auth service and the
    // billing surface (below), so a webhook-driven tier change lands on the
    // user the auth service re-reads on the next sign-in/refresh.
    // The auth store quartet (see `AuthStores`), selected together, all over
    // the one shared pool.
    let (users, refresh, resets, verify_store, memberships): AuthStores =
        if let Some(pool) = pool.clone() {
            (
                Arc::new(amicu_store::PostgresUserStore::new(pool.clone())),
                Arc::new(amicu_store::PostgresRefreshStore::new(pool.clone())),
                Arc::new(amicu_store::PostgresResetStore::new(pool.clone())),
                Arc::new(amicu_store::PostgresVerifyStore::new(pool.clone())),
                Some(Arc::new(amicu_store::PostgresMembershipStore::new(pool))),
            )
        } else {
            (
                Arc::new(InMemoryUserStore::default()),
                Arc::new(InMemoryRefreshStore::default()),
                Arc::new(InMemoryResetStore::default()),
                Arc::new(InMemoryVerifyStore::default()),
                None,
            )
        };

    // Two notifiers, deliberately: the MAILER that actually sends (see
    // `select_mailer`), and the notifier the AUTH FLOWS call, which on the
    // Postgres path is the durable outbox rather than the mailer (see
    // `select_auth_notifier`).
    //
    // The sign-in verification gate (AMICU_REQUIRE_EMAIL_VERIFICATION) is
    // deliberately NOT auto-enabled when SMTP is configured. Auto-on when SMTP is
    // configured is deferred; until then the operator opts in explicitly.
    let mailer = select_mailer()?;
    let notifier = select_auth_notifier(pool.as_ref(), &mailer);

    let mut service = AuthService::new(
        keyring,
        issuer,
        users.clone(),
        refresh,
        resets,
        verify_store,
        notifier,
        effective_base_url.clone(),
    );
    // On the Postgres path, install the real multi-workspace membership
    // authority so token issuance/refresh-revalidation/switch consult
    // `workspace_members`. In-memory, the service keeps its
    // `PersonalMembershipStore` default (each user's single personal workspace).
    if let Some(memberships) = memberships {
        service = service.with_membership_store(memberships);
    }
    // Per-account sign-in backoff (#194): shared across replicas on the
    // Postgres path; the in-memory default is process-local (dev/tests).
    if let Some(pool) = pool.clone() {
        service =
            service.with_login_throttle(Arc::new(amicu_store::PostgresLoginThrottle::new(pool)));
    }
    // Pseudonym key for security events + per-email limiter keys (#194):
    // REQUIRED (>= 32 bytes) in production; dev keeps a loud-warning fallback.
    match std::env::var("AMICU_SECURITY_HASH_KEY") {
        Ok(k) if k.len() >= 32 => {
            let _ = amicu_auth::security::set_hash_key(k.as_bytes());
        }
        _ if config.environment == Environment::Production => {
            return Err(Error::MissingSecurityHashKey);
        }
        _ => eprintln!(
            "amicu-server: WARNING - AMICU_SECURITY_HASH_KEY is not set (or < 32 bytes); \
             emails/IPs in security events are hashed with a PUBLIC default key (guessable for \
             low-entropy inputs). Set one secret value, identical on every replica."
        ),
    }
    // A live handle to the SAME keyring the service just took ownership of:
    // wiring this into KeyringJwksProvider (below) means the engine-gate
    // verifier observes rotate_keys()/retire() in-process, with no restart
    // and no JWKS round-trip (bounded by the verifier's own cache, not
    // instant - see KeyringJwksProvider's docs) - replacing the old
    // boot-time StaticJwksProvider snapshot (andypalmi/amicu-planning#140).
    let keyring_handle = service.keyring_handle();
    let service = Arc::new(service);

    // Rate limiter selection (andypalmi/amicu-planning#142): Postgres-backed
    // and shared across every replica when the shared pool is present,
    // in-memory (dev/tests/DB-less) otherwise. `AMICU_ENV=production` already
    // refused to reach this line without a pool (see `connect_shared_pool`'s
    // `Error::MissingDatabaseUrl` gate above), so production always gets the
    // Postgres limiter here - no separate check needed, mirroring how every
    // other Postgres-vs-in-memory store selection in this function works.
    let limiter = build_rate_limiter(pool.clone(), config);
    let (recover_limiter, signin_limiter) = build_email_limiters(pool.as_ref(), config);
    let auth_state = AuthApiState::new(service.clone(), limiter)
        .with_email_limiters(recover_limiter, signin_limiter)
        .with_trusted_proxies(trusted)
        .with_edge_trust(edge_trust.clone());

    let verifier = Arc::new(Verifier::new(
        KeyringJwksProvider::new(keyring_handle),
        VerifierConfig::new(&config.issuer, &config.audience),
    ));
    let engine_state = EngineApiState::new(verifier.clone()).with_min_tier(config.min_tier.into());

    // Session/workspace surface (list-my-workspaces + switch): authenticates via
    // the same verifier and mints switched pairs through the auth service. Its
    // tenancy store (for `POST /workspaces` and `POST /org/invites/accept`) is
    // installed below, once constructed on the Postgres path.
    let mut session_state = SessionApiState::new(service, verifier.clone());

    // Billing is a real Stripe integration, activated only when its secrets are
    // present in the environment. Absent them, the endpoints return a clean 503
    // ("billing is not configured") rather than faking success.
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let billing_state = if let Some(stripe) = stripe_config_from_env(&env) {
        BillingApiState::with_stripe(verifier.clone(), users.clone(), stripe)
    } else {
        eprintln!(
            "amicu-server: billing DISABLED (set STRIPE_SECRET_KEY, \
             STRIPE_WEBHOOK_SECRET, and STRIPE_PRICE_* to activate); \
             /billing/* endpoints will return 503."
        );
        BillingApiState::unconfigured(verifier.clone(), users)
    };

    // The Enterprise organization console (membership + RBAC + audit log). On the
    // Postgres path it runs on the durable, multi-workspace `workspace_members` +
    // `org_audit` tables via `PostgresMembershipStore` (which implements
    // `OrgStore`), so a mutation re-checks RBAC and writes its audit record inside
    // one workspace-scoped transaction (andypalmi/amicu-planning#130). In-memory
    // (no DATABASE_URL) it falls back to the single-node `InMemoryOrgStore`, state
    // lost on restart. Shares the one pool above, like every other store here.
    // SSO federation is deferred (post-v1) - see #92.
    let org_store: Arc<dyn OrgStore> = if let Some(pool) = pool.clone() {
        Arc::new(PostgresMembershipStore::new(pool))
    } else {
        Arc::new(InMemoryOrgStore::new())
    };
    // Multi-tenant provisioning + invites (andypalmi/amicu-planning#131):
    // `POST /workspaces` (additional-workspace provisioning),
    // `POST /org/invites` + `GET /org/invites` (mint/list), and
    // `POST /org/invites/accept` (redeem). These are inherently multi-tenant
    // (additional workspaces, shared invites), so they are wired ONLY on the
    // Postgres path; the in-memory dev path is single-personal-workspace by
    // construction and those routes return a clean 503. Shares the one pool
    // above, like every other store here.
    let tenancy: Option<Arc<dyn TenancyStore>> = if let Some(pool) = pool.clone() {
        Some(Arc::new(PostgresTenancyStore::new(
            PostgresWorkspaceStore::new(pool.clone())
                .with_max_workspaces_per_user(config.max_workspaces_per_user),
            PostgresInviteStore::new(pool),
        )))
    } else {
        None
    };

    let mut org_state = OrgApiState::new(verifier, org_store);
    if let Some(tenancy) = tenancy.clone() {
        org_state = org_state.with_tenancy(tenancy.clone());
        session_state = session_state.with_tenancy(tenancy);
    }

    // Config storage + version history (`/configs/*`): the same
    // Postgres-when-the-shared-pool-is-present-else-in-memory selection as the
    // auth store quartet above. Kept as its own small, self-contained block
    // (not folded into `AuthStores`) so it stays additive and easy to rebase
    // sibling store-wiring changes (e.g. the state-backend adapter) on top of.
    let config_store: Arc<dyn ConfigStore> = if let Some(pool) = pool.clone() {
        Arc::new(PostgresConfigStore::new(pool))
    } else {
        Arc::new(InMemoryConfigStore::new())
    };
    let config_state = ConfigApiState::new(config_store);

    // State backend (amicu-planning#125, part of the #116 epic): the same
    // Postgres-when-the-shared-pool-is-present-else-in-memory selection as the
    // config store above. `id` names the single state stream this
    // dev/single-node bootstrap manages; a multi-tenant deployment would scope
    // one instance per workspace/stack instead.
    //
    // TODO(amicu-planning#116): `amicu-api` does not yet expose an HTTP
    // surface for state, so there is no `StateApiState`/`state_router`
    // equivalent of `ConfigApiState`/`config_router` for this to attach to
    // yet - inventing one here would be new consumer plumbing ahead of any
    // actual consumer. Until that surface lands, this local variable is
    // intentionally unused (hence the `_` prefix): it still proves the backend
    // is selectable and constructible over the shared pool, and should be
    // threaded into `api_router`/`AppState` the same way `config_store` is
    // as soon as a route needs it.
    let _state_backend: Arc<dyn StateBackendAdapter> = if let Some(pool) = pool.clone() {
        Arc::new(PostgresStateBackend::new(pool, "default"))
    } else {
        Arc::new(InMemoryStateBackend::new("default"))
    };

    let health_state = HealthApiState::new(
        pool.clone(),
        Duration::from_millis(config.readyz_timeout_ms),
    );
    let api = api_router(
        auth_state,
        engine_state,
        billing_state,
        org_state,
        config_state,
        session_state,
    )
    // andypalmi/amicu-planning#142: the edge-token gate wraps the WHOLE API
    // surface - applied here, before `health_router` is merged below, so
    // `GET /healthz`/`GET /readyz` stay outside it (Cloud Run's own
    // liveness/readiness probes carry no `X-Amicu-Edge-Token`). A no-op layer
    // when `AMICU_REQUIRE_EDGE_TOKEN=false` (the default).
    .layer(from_fn_with_state(
        EdgeGateState { trust: edge_trust },
        edge_token_gate,
    ));
    let mut router = api.merge(health_router(health_state));

    // The internal outbox drain (andypalmi/amicu-planning#141), merged AFTER the
    // edge-token gate above so it sits outside it - see `mount_outbox_drain`.
    if let Some(pool) = pool.clone() {
        router = mount_outbox_drain(router, config, pool, &mailer, &effective_base_url);
    }

    Ok(BuiltApp { router, pool })
}

/// The MAILER: the transport that actually puts a message on the wire.
///
/// If ANY `AMICU_SMTP_*` variable is set, the operator has signaled intent to
/// send real email: build the real mailer, and treat an incomplete required set
/// (missing host or from) or a bad value as a HARD startup error
/// ([`Error::Smtp`]). It must NOT silently fall back to the dev
/// [`LoggingNotifier`] (which prints raw reset/verify tokens to stderr): that
/// fallback in a deploy the operator believes is production SMTP is an
/// account-takeover surface. Only when NO SMTP variable is set at all is the
/// `LoggingNotifier` used, for zero-config local runs and tests.
///
/// On the Postgres path this is NOT what the auth flows call - see
/// [`select_auth_notifier`]; it is what the outbox drain calls, later, inside a
/// request.
fn select_mailer() -> Result<Arc<dyn Notifier>, Error> {
    if smtp::any_smtp_var_set() {
        eprintln!("amicu-server: SMTP notifier configured; sending real verification/reset email.");
        Ok(Arc::new(
            smtp::SmtpNotifier::from_env().map_err(Error::Smtp)?,
        ))
    } else {
        eprintln!(
            "amicu-server: SMTP NOT configured (set AMICU_SMTP_HOST + AMICU_SMTP_FROM to \
             activate); using the dev LoggingNotifier, which prints reset/verify tokens to \
             stderr. NOT for production use."
        );
        Ok(Arc::new(LoggingNotifier))
    }
}

/// The notifier the AUTH FLOWS use, which is not necessarily the mailer.
///
/// On the Postgres path it is the durable outbox (andypalmi/amicu-planning#141):
/// the request path writes one `email_outbox` row - joined to the token-mint
/// transaction where the store supports it - and `mailer` is used later, by
/// `POST /internal/outbox/drain`, inside a request where Cloud Run actually
/// allocates CPU. Without a database there is nowhere to queue, so the auth
/// flows talk to the mailer directly, exactly as before.
fn select_auth_notifier(pool: Option<&PgPool>, mailer: &Arc<dyn Notifier>) -> Arc<dyn Notifier> {
    let Some(pool) = pool else {
        if smtp::any_smtp_var_set() {
            eprintln!(
                "amicu-server: WARNING - SMTP is configured but DATABASE_URL is not, so \
                 there is no email outbox to queue into: verification/reset mail is sent \
                 inline on the request path and is lost on restart. This combination is \
                 for local/self-host experimentation only (AMICU_ENV=production already \
                 refuses to start without DATABASE_URL)."
            );
        }
        return mailer.clone();
    };
    Arc::new(amicu_store::PostgresOutboxNotifier::new(pool.clone()))
}

/// Mount `POST /internal/outbox/drain` onto `router`
/// (andypalmi/amicu-planning#141).
///
/// Called only on the Postgres path - there is no queue without a database, so
/// the dev in-memory path simply does not route it (404). Where it IS routed
/// but no caller-authentication mechanism is configured, [`DrainAuth`] refuses
/// every request with a `503` rather than exposing an unauthenticated way to
/// make the deployment send mail; this warns about that at startup, loudly in
/// production.
///
/// Merged alongside `health_router` and never through `api_router`, so it stays
/// out of the generated OpenAPI document - and merged AFTER the edge-token gate
/// so, exactly like the health probes, it sits OUTSIDE it: Cloud Scheduler
/// calls the Cloud Run service URL directly rather than through the Cloudflare
/// Worker, so it carries no `X-Amicu-Edge-Token` and would be refused by that
/// gate under `AMICU_REQUIRE_EDGE_TOKEN=true`. It is not unauthenticated: it
/// enforces its own Google-OIDC / static-bearer check (see [`drain_auth`]).
fn mount_outbox_drain(
    router: Router,
    config: &Config,
    pool: PgPool,
    mailer: &Arc<dyn Notifier>,
    effective_base_url: &str,
) -> Router {
    let auth = drain_auth(config, effective_base_url);
    if !auth.is_configured() {
        let complaint = "amicu-server: outbox drain DISABLED (set \
             AMICU_INTERNAL_CALLER_EMAILS for the Cloud Scheduler OIDC path, or \
             AMICU_INTERNAL_TOKEN for a static-bearer cron caller); \
             POST /internal/outbox/drain will return 503 and NO queued email will \
             ever be delivered.";
        match config.environment {
            Environment::Production => eprintln!(
                "{complaint} WARNING: this is a production deploy - verification and \
                 password-reset email will silently pile up undelivered until one of \
                 those is set."
            ),
            Environment::Development => eprintln!(
                "{complaint} Meanwhile `amicu-server outbox-drain` runs one pass by \
                 hand, which is usually what you want locally."
            ),
        }
    }
    let outbox_state = OutboxApiState::new(
        Arc::new(amicu_store::PostgresOutboxStore::new(pool)),
        mailer.clone(),
        auth,
    )
    .with_batch_size(config.outbox_drain_batch_size)
    .with_budget(Duration::from_millis(config.outbox_drain_budget_ms));
    router.merge(outbox_router(outbox_state))
}

/// Assemble the caller-authentication policy for `POST /internal/outbox/drain`
/// (andypalmi/amicu-planning#141).
///
/// Two independent, optional mechanisms - configure either, or both:
///
/// - **Google OIDC** (production on Cloud Run). Enabled by a non-empty
///   `AMICU_INTERNAL_CALLER_EMAILS`; the required `aud` is
///   `AMICU_INTERNAL_AUDIENCE`, defaulting to `effective_base_url`. Cloud
///   Scheduler mints a Google-signed ID token as its invoker service account
///   and this verifies issuer, signature (against Google's live certs), `aud`,
///   and the allowlisted, verified `email`. An EMPTY allowlist leaves the path
///   OFF rather than accepting any Google identity - a misconfiguration must
///   fail closed.
/// - **A static bearer token**, `AMICU_INTERNAL_TOKEN`, for self-hosters
///   driving the drain from cron + curl. Read from the environment ONLY, never
///   a CLI flag: a flag value is visible in the process argument list. The
///   token is hashed immediately and compared in constant time; this function
///   never logs it and never retains the plaintext.
///
/// With neither configured the returned policy is
/// [`DrainAuth::is_configured`]`== false` and every drain request is refused
/// with `503`.
fn drain_auth(config: &Config, effective_base_url: &str) -> DrainAuth {
    let mut auth = DrainAuth::disabled();

    if let Some(token) = smtp::non_empty_env("AMICU_INTERNAL_TOKEN") {
        auth = auth.with_static_token(&token);
    }

    let allowed_emails: Vec<String> = config
        .internal_caller_emails
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect();
    if !allowed_emails.is_empty() {
        let audience = config
            .internal_audience
            .clone()
            .unwrap_or_else(|| effective_base_url.to_string());
        auth = auth.with_google_oidc(GoogleIdTokenVerifier::new(
            HttpGoogleCertsSource::new(),
            GoogleIdTokenConfig::new(audience, allowed_emails),
        ));
    }

    auth
}

/// Run ONE outbox drain pass against `DATABASE_URL` and exit - this is
/// `amicu-server outbox-drain` (see `main.rs`'s `Command::OutboxDrain`).
///
/// A thin wrapper around exactly the same [`amicu_api::drain_once`] the HTTP
/// endpoint calls, with the same mailer selection as [`build_app`], so a manual
/// or cron-driven drain can never diverge from the scheduled one. Runs no
/// migrations and no RLS-bypass guard: it is expected to connect as the same
/// non-owning runtime role the service uses.
///
/// # Errors
/// [`Error::MissingDatabaseUrl`] if `DATABASE_URL` is unset or empty,
/// [`Error::Store`] if the pool cannot connect or the queue cannot be read or
/// updated, or [`Error::Smtp`] if `AMICU_SMTP_*` is set but incomplete.
pub async fn outbox_drain(config: &Config) -> Result<(), Error> {
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or(Error::MissingDatabaseUrl)?;

    let pool = amicu_store::connect_with(
        &url,
        PoolConfig {
            max_connections: config.db_max_connections,
            min_connections: config.db_min_connections,
            acquire_timeout: Duration::from_secs(config.db_acquire_timeout_secs),
        },
    )
    .await
    .map_err(Error::Store)?;

    let mailer: Arc<dyn Notifier> = if smtp::any_smtp_var_set() {
        Arc::new(smtp::SmtpNotifier::from_env().map_err(Error::Smtp)?)
    } else {
        eprintln!(
            "amicu-server outbox-drain: SMTP NOT configured; queued messages will be \
             'delivered' by the dev LoggingNotifier (stderr). NOT for production use."
        );
        Arc::new(LoggingNotifier)
    };

    let store = amicu_store::PostgresOutboxStore::new(pool.clone());
    let report = amicu_api::drain_once(
        &store,
        mailer.as_ref(),
        config.outbox_drain_batch_size,
        Duration::from_millis(config.outbox_drain_budget_ms),
    )
    .await
    .map_err(Error::Store)?;
    pool.close().await;

    eprintln!(
        "amicu-server outbox-drain: claimed={} sent={} failed={} dead_lettered={} \
         budget_exhausted={}",
        report.claimed, report.sent, report.failed, report.dead_lettered, report.budget_exhausted,
    );
    Ok(())
}

/// Serve `app` on an already-bound `listener` until `shutdown` resolves.
///
/// **Critical:** the router is mounted with
/// [`into_make_service_with_connect_info::<SocketAddr>`](axum::Router::into_make_service_with_connect_info)
/// so `ConnectInfo<SocketAddr>` is populated on every request. The credential
/// rate limiter derives its per-client key from that peer address; without this
/// wiring it has no identity to key on and fails open (letting every credential
/// request through). All callers - including the smoke test - go through this
/// function so the connect-info path is always exercised over a real socket.
///
/// # Errors
/// Returns the underlying `std::io::Error` if the accept loop fails.
pub async fn serve<F>(listener: TcpListener, app: Router, shutdown: F) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// Default bind address when neither `--bind`/`AMICU_BIND` nor `PORT` is set:
/// `0.0.0.0:8080`, reachable on any interface out of the box (see the
/// Cloud Run contract note on [`Config::bind`]).
const DEFAULT_BIND: &str = "0.0.0.0:8080";

/// Resolves the effective bind address per [`Config::bind`]'s precedence: an
/// explicit `--bind`/`AMICU_BIND` value always wins; otherwise `PORT` (Cloud
/// Run's runtime contract) binds `0.0.0.0:$PORT`; otherwise [`DEFAULT_BIND`].
fn resolve_bind(explicit: Option<SocketAddr>) -> Result<SocketAddr, Error> {
    resolve_bind_with(explicit, |name| std::env::var(name).ok())
}

/// [`resolve_bind`] against an arbitrary lookup (test seam), mirroring the
/// `smtp` module's lookup-injection pattern so `PORT` precedence is
/// unit-testable without mutating the ambient process environment.
fn resolve_bind_with<F: Fn(&str) -> Option<String>>(
    explicit: Option<SocketAddr>,
    lookup: F,
) -> Result<SocketAddr, Error> {
    if let Some(addr) = explicit {
        return Ok(addr);
    }
    if let Some(port) = lookup("PORT") {
        let trimmed = port.trim();
        let port: u16 = trimmed
            .parse()
            .map_err(|_| Error::Port(trimmed.to_string()))?;
        // Port 0 asks the OS for an arbitrary ephemeral port, chosen only
        // after bind() - useless here, since Cloud Run (or any reverse proxy
        // expecting a fixed PORT) already decided which port to route to
        // before the container even starts. Reject it explicitly rather than
        // silently binding somewhere nothing will ever connect.
        if port == 0 {
            return Err(Error::Port(
                "0 (binds an OS-chosen ephemeral port; Cloud Run cannot route to it - use an \
                 explicit --bind/AMICU_BIND if you really want an ephemeral port)"
                    .to_string(),
            ));
        }
        return Ok(SocketAddr::from(([0, 0, 0, 0], port)));
    }
    Ok(DEFAULT_BIND
        .parse()
        .expect("DEFAULT_BIND is a valid socket address"))
}

/// The startup banner [`run`] prints once the socket is bound: the dev
/// NOT-for-production warning in [`Environment::Development`] (this
/// bootstrap's dev path can still fall back to ephemeral in-memory stores and
/// a fresh random signing key each start - tokens don't survive a restart, so
/// shout loud enough that a run facing real users can never mistake it for a
/// production deploy), or a one-line production-mode line in
/// [`Environment::Production`].
fn startup_banner(env: Environment) -> String {
    match env {
        Environment::Development => "amicu-server: WARNING - development server \
             (AMICU_ENV=development, the default). NOT for production use: a missing \
             DATABASE_URL silently falls back to ephemeral in-memory stores and a fresh \
             signing key each start. Set AMICU_ENV=production to make that fallback a hard \
             startup error instead."
            .to_string(),
        Environment::Production => {
            "amicu-server: production mode (AMICU_ENV=production).".to_string()
        }
    }
}

/// Build the app, bind the effective address (see [`Config::bind`]), and
/// serve it until SIGINT/SIGTERM, closing the shared Postgres pool (if any)
/// once [`serve`] returns.
///
/// # Errors
/// Returns [`Error`] if the state cannot be assembled, `PORT` is set but not
/// a valid port, the socket cannot be bound, or the accept loop fails.
pub async fn run(config: Config) -> Result<(), Error> {
    let built = build_app(&config).await?;
    let bind = resolve_bind(config.bind)?;
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    eprintln!("{}", startup_banner(config.environment));
    eprintln!("amicu-server listening on http://{addr}");
    serve(listener, built.router, shutdown_signal()).await?;
    if let Some(pool) = built.pool {
        /// Upper bound on how long shutdown waits for in-flight connections
        /// to drain before giving up on a clean pool close. Bounded so a
        /// stuck connection cannot hang the process past its SIGTERM grace
        /// period indefinitely.
        const POOL_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
        if tokio::time::timeout(POOL_CLOSE_TIMEOUT, pool.close())
            .await
            .is_err()
        {
            eprintln!(
                "amicu-server: WARNING - closing the shared Postgres pool did not finish \
                 within {POOL_CLOSE_TIMEOUT:?}; exiting anyway."
            );
        }
    }
    eprintln!("amicu-server shut down");
    Ok(())
}

/// A future that resolves on the first SIGINT (Ctrl-C) or, on Unix, SIGTERM -
/// the signal a container runtime sends to ask for a graceful stop.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

/// Reads `AMICU_EDGE_TOKEN` into a [`SecretValue`], or `None` if unset/empty.
/// Read once at boot (unlike `AMICU_REQUIRE_EMAIL_VERIFICATION`'s
/// read-on-every-call convention - see [`env_flag_is_truthy`]) and held inside
/// [`amicu_api::EdgeTrust`] for the life of the process; never echoed in any
/// log line (andypalmi/amicu-planning#142).
fn edge_token_from_env() -> Option<SecretValue> {
    std::env::var("AMICU_EDGE_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(SecretValue::new)
}

/// Whether env var `name` is set to a recognized truthy token (`1`, `true`,
/// `yes`, `on`, case-insensitive); anything else, including unset, is `false`.
/// Mirrors `amicu-auth`'s `require_email_verification` boolean-env convention
/// exactly, so every `AMICU_*` boolean flag in this codebase parses the same
/// way.
fn env_flag_is_truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("valid test socket address")
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// Neither `--bind`/`AMICU_BIND` nor `PORT` set: the default.
    #[test]
    fn resolve_bind_defaults_to_0_0_0_0_8080() {
        assert_eq!(resolve_bind_with(None, no_env).unwrap(), addr(DEFAULT_BIND));
    }

    /// `PORT` set, no explicit bind: binds `0.0.0.0:$PORT` (the Cloud Run
    /// contract).
    #[test]
    fn resolve_bind_honours_port_when_no_explicit_bind() {
        let resolved =
            resolve_bind_with(None, |name| (name == "PORT").then(|| "9090".to_string())).unwrap();
        assert_eq!(resolved, addr("0.0.0.0:9090"));
    }

    /// An explicit `--bind`/`AMICU_BIND` value always wins over `PORT`.
    #[test]
    fn resolve_bind_explicit_wins_over_port() {
        let explicit = addr("127.0.0.1:4000");
        let resolved = resolve_bind_with(Some(explicit), |name| {
            (name == "PORT").then(|| "9090".to_string())
        })
        .unwrap();
        assert_eq!(resolved, explicit);
    }

    /// A non-numeric `PORT` is a clear, typed startup error, not a panic.
    #[test]
    fn resolve_bind_rejects_invalid_port() {
        let err = resolve_bind_with(None, |name| {
            (name == "PORT").then(|| "not-a-port".to_string())
        })
        .unwrap_err();
        assert!(matches!(err, Error::Port(v) if v == "not-a-port"));
    }

    /// `PORT` is trimmed, matching the workspace's env-value trimming
    /// convention (see `smtp::non_empty_env` / `AMICU_SMTP_PORT`).
    #[test]
    fn resolve_bind_trims_port() {
        let resolved = resolve_bind_with(None, |name| {
            (name == "PORT").then(|| "  8081  ".to_string())
        })
        .unwrap();
        assert_eq!(resolved, addr("0.0.0.0:8081"));
    }

    /// The development banner warns loudly and is NOT for production use.
    #[test]
    fn startup_banner_dev_warns_not_for_production() {
        let banner = startup_banner(Environment::Development);
        assert!(banner.contains("NOT for production use"), "{banner}");
        assert!(banner.contains("WARNING"), "{banner}");
    }

    /// `PORT=0` asks the OS for an ephemeral port, which Cloud Run cannot
    /// route to - reject it explicitly instead of silently binding somewhere
    /// unreachable.
    #[test]
    fn resolve_bind_rejects_port_zero() {
        let err =
            resolve_bind_with(None, |name| (name == "PORT").then(|| "0".to_string())).unwrap_err();
        assert!(matches!(err, Error::Port(_)), "{err}");
    }

    /// An explicit `--bind`/`AMICU_BIND` ending in `:0` (an OS-chosen
    /// ephemeral port for local tests, e.g. the smoke test) is NOT rejected -
    /// only `PORT=0` is, since only `PORT` claims to be a fixed, routable
    /// port a proxy already decided on.
    #[test]
    fn resolve_bind_allows_explicit_ephemeral_port() {
        let explicit = addr("127.0.0.1:0");
        assert_eq!(resolve_bind_with(Some(explicit), no_env).unwrap(), explicit);
    }

    /// A non-empty caller allowlist switches the OIDC drain path on; an empty
    /// one leaves it off (it never means "accept any Google identity").
    ///
    /// Only the OIDC half is asserted here: the static-token half reads
    /// `AMICU_INTERNAL_TOKEN` from the ambient process environment, which a
    /// test cannot control (this workspace forbids `unsafe`, so
    /// `std::env::set_var` is unavailable). That half is covered by
    /// `amicu-api`'s `DrainAuth` unit tests, which take the token directly.
    #[test]
    fn drain_auth_oidc_requires_a_non_empty_caller_allowlist() {
        let mut config = Config::parse_from(["amicu-server"]);
        config.internal_caller_emails = "  ,  ".to_string();
        assert!(
            !drain_auth(&config, "https://api.example.test").is_configured()
                || std::env::var("AMICU_INTERNAL_TOKEN").is_ok(),
            "a blank allowlist must not enable the OIDC path"
        );

        config.internal_caller_emails =
            "amicu-scheduler@amicu-prod.iam.gserviceaccount.com".to_string();
        assert!(
            drain_auth(&config, "https://api.example.test").is_configured(),
            "an allowlisted caller must enable the OIDC path"
        );
    }

    /// The production banner is the exact one-line message, with no dev
    /// warning text.
    #[test]
    fn startup_banner_prod_is_one_line_production_mode() {
        let banner = startup_banner(Environment::Production);
        assert_eq!(
            banner,
            "amicu-server: production mode (AMICU_ENV=production)."
        );
        assert!(!banner.contains("NOT for production use"));
    }
}
