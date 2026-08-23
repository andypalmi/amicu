//! Runnable bootstrap for the Amicu Management API.
//!
//! `amicu-api` is a library - it hands back a [`Router`] from
//! `api_router(auth_state, engine_state, billing_state, org_state)` but binds no socket. This crate is the
//! thin glue that a deployment actually runs: it constructs the auth + engine
//! state, reads its configuration from the environment (or flags), binds a
//! `TcpListener`, and serves the router with graceful shutdown.
//!
//! # Dev / single-node bootstrap
//!
//! State is wired with the **in-memory** auth stores
//! ([`InMemoryUserStore`](amicu_auth::user::InMemoryUserStore) and friends) and
//! a freshly generated signing key, and the engine gate verifies tokens against
//! a snapshot of that key taken at startup. That is deliberate and sufficient
//! for a single node / local end-to-end testing, but it means **all state is
//! lost on restart** and the process cannot be horizontally scaled (each replica
//! would sign with a different key and hold its own user table). Persistent,
//! shared-store backing (Postgres user/refresh/reset stores, a KMS-held or
//! rotated keyring, a distributed rate limiter) is future work - see the
//! `amicu-auth` store traits, which this bootstrap is the seam for.
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
//! | `bind` | `--bind` | `AMICU_BIND` | `127.0.0.1:8080` |
//! | `issuer` | `--issuer` | `AMICU_ISSUER` | `http://localhost:8080` |
//! | `audience` | `--audience` | `AMICU_AUDIENCE` | `amicu` |
//! | `base_url` | `--base-url` | `AMICU_BASE_URL` | the `issuer` value |
//! | `trusted_proxies` | `--trusted-proxies` | `AMICU_TRUSTED_PROXIES` | empty (trust none) |
//! | `min_tier` | `--min-tier` | `AMICU_MIN_TIER` | `community` |
//! | `rate_limit_max` | `--rate-limit-max` | `AMICU_RATE_LIMIT_MAX` | `30` |
//! | `rate_limit_window_secs` | `--rate-limit-window-secs` | `AMICU_RATE_LIMIT_WINDOW_SECS` | `60` |
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

pub mod smtp;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use amicu_api::{
    AuthApiState, BillingApiState, ConfigApiState, EngineApiState, InMemoryOrgStore, OrgApiState,
    ProxyParseError, RateLimiter, TrustedProxies, api_router, stripe_config_from_env,
};
use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::notify::{LoggingNotifier, Notifier};
use amicu_auth::refresh::{InMemoryRefreshStore, RefreshStore};
use amicu_auth::reset::{InMemoryResetStore, ResetStore};
use amicu_auth::service::AuthService;
use amicu_auth::user::{InMemoryUserStore, UserStore};
use amicu_auth::verifier::{StaticJwksProvider, Verifier, VerifierConfig};
use amicu_auth::verify::{InMemoryVerifyStore, VerifyStore};
use amicu_auth::{AuthError, Tier};
use amicu_state::{InMemoryStateBackend, StateBackendAdapter};
use amicu_store::config::ConfigStore;
use amicu_store::{InMemoryConfigStore, PostgresConfigStore, PostgresStateBackend};
use axum::Router;
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
    #[arg(long, env = "AMICU_BIND", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,

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
    /// The startup signing key could not be generated.
    Key(AuthError),
    /// The persistent store could not connect or migrate (only reachable when
    /// `DATABASE_URL` is set).
    Store(amicu_store::StoreError),
    /// The SMTP notifier could not be configured (only reachable when the
    /// `AMICU_SMTP_*` environment is present).
    Smtp(smtp::SmtpConfigError),
    /// Binding or serving the socket failed.
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Proxy(e) => write!(f, "trusted-proxy config: {e}"),
            Error::Key(e) => write!(f, "signing-key generation: {e}"),
            Error::Store(e) => write!(f, "persistent store: {e}"),
            Error::Smtp(e) => write!(f, "smtp notifier config: {e}"),
            Error::Io(e) => write!(f, "server socket: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Proxy(e) => Some(e),
            Error::Key(e) => Some(e),
            Error::Store(e) => Some(e),
            Error::Smtp(e) => Some(e),
            Error::Io(e) => Some(e),
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

/// Assemble the full API [`Router`] from `config`, wiring the auth stores, a
/// freshly generated signing key, and the engine gate.
///
/// # Store selection
///
/// If `DATABASE_URL` is set, durable Postgres stores are constructed (connect +
/// run migrations) and used for users, refresh tokens, and reset tokens;
/// otherwise the dev/single-node in-memory stores are used so local runs, tests,
/// and pre-database self-host work with zero configuration. The config store
/// and the state backend ([`PostgresStateBackend`] /
/// [`InMemoryStateBackend`](amicu_state::InMemoryStateBackend), see
/// amicu-planning#125) follow the same selection, each over its own
/// independently connected pool.
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
/// The engine gate verifies bearer tokens against a **snapshot** of the freshly
/// generated JWKS taken here at startup. Because this bootstrap never rotates
/// the key, the snapshot always matches what the auth service signs with; a
/// deployment that rotates keys would back the verifier with a live JWKS
/// provider instead.
///
/// # Errors
/// Returns [`Error::Proxy`] if the trusted-proxy allowlist fails to parse,
/// [`Error::Key`] if the signing key cannot be generated, [`Error::Store`]
/// if `DATABASE_URL` is set but the store cannot connect or migrate, or
/// [`Error::Smtp`] if any `AMICU_SMTP_*` variable is set but the SMTP
/// configuration is incomplete or invalid.
pub async fn build_app(config: &Config) -> Result<Router, Error> {
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
pub async fn build_app_in_memory(config: &Config) -> Result<Router, Error> {
    build_app_with_database_url(config, None).await
}

/// Shared implementation behind [`build_app`] and [`build_app_in_memory`].
/// `database_url` is an explicit override rather than an environment read, so
/// callers control store selection precisely: `Some(url)` selects the
/// Postgres stores, `None` selects the in-memory stores, with no ambient
/// environment lookup at all.
async fn build_app_with_database_url(
    config: &Config,
    database_url: Option<&str>,
) -> Result<Router, Error> {
    /// The auth store quartet (user, refresh, reset, verify), selected together:
    /// all Postgres when `DATABASE_URL` is set, else all in-memory.
    type AuthStores = (
        Arc<dyn UserStore>,
        Arc<dyn RefreshStore>,
        Arc<dyn ResetStore>,
        Arc<dyn VerifyStore>,
    );

    let trusted = TrustedProxies::from_strs(config.trusted_proxies.split(','))?;
    let base_url = config
        .base_url
        .clone()
        .unwrap_or_else(|| config.issuer.clone());

    // Fresh signing key; snapshot its public JWKS before the keyring moves into
    // the service, so the engine verifier trusts exactly what we sign with.
    let key = SigningKey::generate().map_err(Error::Key)?;
    let keyring = Keyring::new(key);
    let jwks = keyring.jwks();

    let issuer = TokenIssuer::new(IssuerConfig::new(&config.issuer, &config.audience));

    // Store selection. When DATABASE_URL is set, use the durable Postgres stores
    // (connect + migrate); otherwise fall back to the dev/single-node in-memory
    // stores so local runs and tests need zero configuration. The user store is
    // shared between the auth service and the billing surface (below), so a
    // webhook-driven tier change lands on the user the auth service re-reads on
    // the next sign-in/refresh.
    // The auth store quartet (see `AuthStores`), selected together so the
    // Postgres path shares one pool and the in-memory path stays zero-config.
    let (users, refresh, resets, verify_store): AuthStores =
        if let Some(database_url) = database_url {
            let pool = amicu_store::connect(database_url)
                .await
                .map_err(Error::Store)?;
            amicu_store::run_migrations(&pool)
                .await
                .map_err(Error::Store)?;
            eprintln!("amicu-server: using Postgres auth stores (DATABASE_URL set).");
            (
                Arc::new(amicu_store::PostgresUserStore::new(pool.clone())),
                Arc::new(amicu_store::PostgresRefreshStore::new(pool.clone())),
                Arc::new(amicu_store::PostgresResetStore::new(pool.clone())),
                Arc::new(amicu_store::PostgresVerifyStore::new(pool)),
            )
        } else {
            (
                Arc::new(InMemoryUserStore::default()),
                Arc::new(InMemoryRefreshStore::default()),
                Arc::new(InMemoryResetStore::default()),
                Arc::new(InMemoryVerifyStore::default()),
            )
        };

    // Notifier selection. If ANY AMICU_SMTP_* variable is set, the operator has
    // signaled intent to send real email: build the real mailer, and treat an
    // incomplete required set (missing host or from) or a bad value as a HARD
    // startup error via Error::Smtp. It must NOT silently fall back to the dev
    // LoggingNotifier (which prints raw reset/verify tokens to stderr): that
    // fallback in a deploy the operator believes is production SMTP is an
    // account-takeover surface. Only when NO SMTP variable is set at all do we
    // use the LoggingNotifier, for zero-config local runs and tests.
    //
    // The sign-in verification gate (AMICU_REQUIRE_EMAIL_VERIFICATION) is
    // deliberately NOT auto-enabled when SMTP is configured. Verify tokens still
    // live in the in-memory store (lost on restart) and there is no resend
    // endpoint yet, so auto-on could lock users out after a restart. Auto-on when
    // SMTP is configured is deferred until a persistent verify store and a resend
    // endpoint exist; until then the operator opts in explicitly via the gate.
    let notifier: Arc<dyn Notifier> = if smtp::any_smtp_var_set() {
        eprintln!("amicu-server: SMTP notifier configured; sending real verification/reset email.");
        Arc::new(smtp::SmtpNotifier::from_env().map_err(Error::Smtp)?)
    } else {
        eprintln!(
            "amicu-server: SMTP NOT configured (set AMICU_SMTP_HOST + AMICU_SMTP_FROM to \
             activate); using the dev LoggingNotifier, which prints reset/verify tokens to \
             stderr. NOT for production use."
        );
        Arc::new(LoggingNotifier)
    };

    let service = Arc::new(AuthService::new(
        keyring,
        issuer,
        users.clone(),
        refresh,
        resets,
        verify_store,
        notifier,
        base_url,
    ));
    let limiter = Arc::new(RateLimiter::new(
        config.rate_limit_max,
        Duration::from_secs(config.rate_limit_window_secs),
    ));
    let auth_state = AuthApiState::new(service, limiter).with_trusted_proxies(trusted);

    let verifier = Arc::new(Verifier::new(
        StaticJwksProvider::new(jwks),
        VerifierConfig::new(&config.issuer, &config.audience),
    ));
    let engine_state = EngineApiState::new(verifier.clone()).with_min_tier(config.min_tier.into());

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

    // The Enterprise organization console (membership + RBAC + audit log). The
    // store is in-memory and single-node like the auth/billing stores: state is
    // lost on restart. The first Enterprise caller bootstraps the org as its
    // Owner; everyone after is invited. SSO federation is deliberately deferred
    // (post-v1) - see andypalmi/amicu-planning#92.
    let org_store = Arc::new(InMemoryOrgStore::new());
    let org_state = OrgApiState::new(verifier, org_store);

    // Config storage + version history (`/configs/*`): the same
    // Postgres-when-`DATABASE_URL`-else-in-memory selection as the auth store
    // quartet above, but connected independently (its own pool) since
    // `ConfigStore` is a single trait object, not a quartet sharing one
    // constructor call. Kept as its own small, self-contained block (not
    // folded into `AuthStores`) so it stays additive and easy to rebase
    // sibling store-wiring changes (e.g. the state-backend adapter) on top of.
    let config_store: Arc<dyn ConfigStore> = if let Some(database_url) = database_url {
        let pool = amicu_store::connect(database_url)
            .await
            .map_err(Error::Store)?;
        amicu_store::run_migrations(&pool)
            .await
            .map_err(Error::Store)?;
        eprintln!("amicu-server: using the Postgres config store (DATABASE_URL set).");
        Arc::new(PostgresConfigStore::new(pool))
    } else {
        Arc::new(InMemoryConfigStore::new())
    };
    let config_state = ConfigApiState::new(config_store);

    // State backend (amicu-planning#125, part of the #116 epic): the same
    // Postgres-when-`DATABASE_URL`-else-in-memory selection as the config
    // store above, connected independently (its own pool), for the same
    // rebase-ability reason documented on that block. `id` names the single
    // state stream this dev/single-node bootstrap manages; a multi-tenant
    // deployment would scope one instance per workspace/stack instead.
    //
    // TODO(amicu-planning#116): `amicu-api` does not yet expose an HTTP
    // surface for state, so there is no `StateApiState`/`state_router`
    // equivalent of `ConfigApiState`/`config_router` for this to attach to
    // yet - inventing one here would be new consumer plumbing ahead of any
    // actual consumer. Until that surface lands, this local variable is
    // intentionally unused (hence the `_` prefix): it still runs migrations
    // and proves the backend is selectable and constructible, and should be
    // threaded into `api_router`/`AppState` the same way `config_store` is
    // as soon as a route needs it.
    let _state_backend: Arc<dyn StateBackendAdapter> = if let Some(database_url) = database_url {
        let pool = amicu_store::connect(database_url)
            .await
            .map_err(Error::Store)?;
        amicu_store::run_migrations(&pool)
            .await
            .map_err(Error::Store)?;
        eprintln!("amicu-server: using the Postgres state backend (DATABASE_URL set).");
        Arc::new(PostgresStateBackend::new(pool, "default"))
    } else {
        Arc::new(InMemoryStateBackend::new("default"))
    };

    Ok(api_router(
        auth_state,
        engine_state,
        billing_state,
        org_state,
        config_state,
    ))
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

/// Build the app, bind [`Config::bind`], and serve it until SIGINT/SIGTERM.
///
/// # Errors
/// Returns [`Error`] if the state cannot be assembled, the socket cannot be
/// bound, or the accept loop fails.
pub async fn run(config: Config) -> Result<(), Error> {
    let app = build_app(&config).await?;
    let listener = TcpListener::bind(config.bind).await?;
    let addr = listener.local_addr()?;
    // This bootstrap is a DEV/single-node target: in-memory stores, a fresh
    // random signing key each start (tokens don't survive a restart), and a
    // LoggingNotifier that writes password-reset tokens to stderr. Shout so a
    // run that faces real users can never mistake it for a production deploy.
    eprintln!(
        "amicu-server: WARNING - development server (in-memory state, ephemeral \
         signing key, reset tokens logged to stderr). NOT for production use."
    );
    eprintln!("amicu-server listening on http://{addr}");
    serve(listener, app, shutdown_signal()).await?;
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
