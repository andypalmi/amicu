//! HTTP surface for the auth service: `.well-known` discovery plus the
//! sign-up / token / refresh / logout endpoints, with rate limiting on the
//! credential-taking routes.

use std::net::SocketAddr;
use std::sync::Arc;

use amicu_auth::error::AuthError;
use amicu_auth::jwks::{Jwks, OpenIdConfiguration};
use amicu_auth::security::{self, SecurityEvent, Severity};
use amicu_auth::service::{AuthService, TokenPair};
use amicu_auth::throttle::{InMemoryKnownNetworks, KnownNetworks};
use amicu_core::secret::SecretValue;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use crate::edge::EdgeTrust;
use crate::ratelimit::{InMemoryRateLimiter, RateLimiter, TrustedProxies};

/// Maximum request body on the auth routes (JSON credentials / tokens only).
pub const MAX_AUTH_BODY_BYTES: usize = 16 * 1024;

/// Default per-email budget for `/recover` and `/verify/resend`: 5 per 15 min.
const DEFAULT_EMAIL_LIMIT: (u32, u64) = (5, 900);
/// Default per-email budget of FAILED `/signin` / `/token` attempts: 100 per
/// 15 min (a loose ceiling; the network-scoped account backoff is the tight
/// control).
const DEFAULT_SIGNIN_EMAIL_LIMIT: (u32, u64) = (100, 900);

/// Operator-visible signal that the shared failed-sign-in budget store is down
/// and the per-replica fallback is in effect.
fn budget_unavailable() {
    SecurityEvent::new("login_throttle_unavailable", Severity::Warn)
        .reason("email_budget_error")
        .emit();
}

/// Pseudonym of the verified client IP for this request (`None` when the
/// request carried no trustworthy identity). Inserted by the rate-limit
/// middleware so handlers can attach it to security events without ever
/// touching the raw address.
#[derive(Clone, Debug)]
pub(crate) struct ClientFingerprint {
    /// Pseudonym of the client IP, for security events.
    ip: Option<String>,
    /// Pseudonym of the client's network (IPv4 /24, IPv6 /64), used to scope
    /// the per-account sign-in backoff.
    net: Option<String>,
}

/// The IPv4 /24 or IPv6 /64 containing `ip`, textual.
fn network_prefix(ip: std::net::IpAddr) -> String {
    // IPv4-mapped IPv6 (::ffff:a.b.c.d) is the same host as the IPv4 address.
    match ip.to_canonical() {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

/// Shared HTTP state.
#[derive(Clone)]
pub struct AuthApiState {
    service: Arc<AuthService>,
    limiter: Arc<dyn RateLimiter>,
    /// Per-email limiter for `/recover` and `/verify/resend`.
    email_limiter: Arc<dyn RateLimiter>,
    /// Per-email limiter for `/signin` and `/token`.
    signin_email_limiter: Arc<dyn RateLimiter>,
    /// Per-replica stand-in for `signin_email_limiter` when it errors or times
    /// out: same limits, process-local, so the failed-sign-in budget degrades
    /// to per-replica enforcement instead of to nothing.
    signin_fallback: Arc<InMemoryRateLimiter>,
    /// Networks that recently signed in successfully, per account.
    known_networks: Arc<dyn KnownNetworks>,
    trusted_proxies: TrustedProxies,
    edge_trust: EdgeTrust,
}

impl AuthApiState {
    /// Assemble state from a service and a rate limiter. No proxies are trusted
    /// and no Cloudflare edge token is configured by default; see
    /// [`AuthApiState::with_trusted_proxies`] and
    /// [`AuthApiState::with_edge_trust`].
    #[must_use]
    pub fn new(service: Arc<AuthService>, limiter: Arc<dyn RateLimiter>) -> Self {
        Self {
            service,
            limiter,
            email_limiter: Arc::new(InMemoryRateLimiter::new(
                DEFAULT_EMAIL_LIMIT.0,
                std::time::Duration::from_secs(DEFAULT_EMAIL_LIMIT.1),
            )),
            signin_email_limiter: Arc::new(InMemoryRateLimiter::new(
                DEFAULT_SIGNIN_EMAIL_LIMIT.0,
                std::time::Duration::from_secs(DEFAULT_SIGNIN_EMAIL_LIMIT.1),
            )),
            signin_fallback: Arc::new(InMemoryRateLimiter::new(
                DEFAULT_SIGNIN_EMAIL_LIMIT.0,
                std::time::Duration::from_secs(DEFAULT_SIGNIN_EMAIL_LIMIT.1),
            )),
            known_networks: Arc::new(InMemoryKnownNetworks::default()),
            trusted_proxies: TrustedProxies::default(),
            edge_trust: EdgeTrust::default(),
        }
    }

    /// Install the per-EMAIL limiters (andypalmi/amicu-planning#194), applied
    /// in addition to the per-IP limiter: `recover_and_resend` guards
    /// `/recover` and `/verify/resend`, `signin` guards `/signin` and `/token`.
    /// Keys are pseudonyms of the normalized email, so unknown emails are
    /// limited exactly like registered ones (no enumeration signal). Production
    /// passes Postgres-backed limiters so the budget is shared across replicas.
    #[must_use]
    pub fn with_email_limiters(
        mut self,
        recover_and_resend: Arc<dyn RateLimiter>,
        signin: Arc<dyn RateLimiter>,
    ) -> Self {
        self.email_limiter = recover_and_resend;
        self.signin_email_limiter = signin;
        self
    }

    /// Set the limits of the per-replica fallback for the failed-sign-in budget
    /// (should equal the shared limiter's `max` and `window`).
    #[must_use]
    pub fn with_signin_fallback(mut self, max: u32, window: std::time::Duration) -> Self {
        self.signin_fallback = Arc::new(InMemoryRateLimiter::new(max, window));
        self
    }

    /// Install the shared "known networks" store (Postgres-backed in production).
    #[must_use]
    pub fn with_known_networks(mut self, known: Arc<dyn KnownNetworks>) -> Self {
        self.known_networks = known;
        self
    }

    /// Key for the failed-sign-in budget of `email`.
    fn signin_budget_key(email: &str) -> Option<String> {
        let normalized = amicu_auth::service::normalize_email(email);
        (!normalized.is_empty()).then(|| {
            format!(
                "email|signin_failed|{}",
                security::pseudonym("email-rl", &normalized)
            )
        })
    }

    /// Pre-check of the failed-sign-in budget (no hit counted). When it is
    /// exhausted, only a network that recently signed in successfully to this
    /// account may still attempt; everyone else is refused BEFORE any password
    /// check. Identical for unknown emails (the budget is keyed by email, and an
    /// unknown email never has a known network). Store errors fail open.
    ///
    /// Trade-off: during an active distributed attack the owner on a brand-new
    /// network is refused until the budget window passes.
    async fn signin_budget_refuses(&self, email: &str, net: Option<&str>) -> bool {
        let Some(key) = Self::signin_budget_key(email) else {
            return false;
        };
        let exhausted = if let Ok(e) = self.signin_email_limiter.try_is_exhausted(&key).await {
            e
        } else {
            budget_unavailable();
            self.signin_fallback.exhausted(&key)
        };

        if !exhausted {
            return false;
        }
        let known = tokio::time::timeout(
            std::time::Duration::from_millis(1_000),
            self.known_networks.is_known(&Self::known_key(email, net)),
        )
        .await;
        !matches!(known, Ok(Ok(true)))
    }

    /// Count one FAILED sign-in against the budget; true when it is now over.
    /// A backend error or timeout switches to the per-replica fallback.
    async fn signin_budget_consume(&self, email: &str) -> bool {
        let Some(key) = Self::signin_budget_key(email) else {
            return false;
        };
        if let Ok(allowed) = self.signin_email_limiter.try_check(&key).await {
            !allowed
        } else {
            budget_unavailable();
            !self.signin_fallback.check(&key)
        }
    }

    fn known_key(email: &str, net: Option<&str>) -> String {
        security::pseudonym(
            "known-network",
            &format!(
                "{}|{}",
                amicu_auth::service::normalize_email(email),
                net.unwrap_or("")
            ),
        )
    }

    /// True when `email` has exhausted `limiter`'s budget for `bucket`. Empty
    /// emails are not tracked (no shared bucket). The limiter fails open on
    /// database errors, like the per-IP one.
    async fn email_throttled(
        &self,
        limiter: &Arc<dyn RateLimiter>,
        bucket: &str,
        email: &str,
    ) -> bool {
        let normalized = amicu_auth::service::normalize_email(email);
        if normalized.is_empty() {
            return false;
        }
        let key = format!(
            "email|{bucket}|{}",
            security::pseudonym("email-rl", &normalized)
        );
        !limiter.check(&key).await
    }

    /// Configure the trusted-proxy allowlist used to decide whether a request's
    /// `X-Forwarded-For` header may be believed for rate-limit keying.
    #[must_use]
    pub fn with_trusted_proxies(mut self, trusted_proxies: TrustedProxies) -> Self {
        self.trusted_proxies = trusted_proxies;
        self
    }

    /// Configure the Cloudflare edge-trust policy (andypalmi/amicu-planning#142):
    /// when a request presents a valid `X-Amicu-Edge-Token`, its
    /// `CF-Connecting-IP` header is trusted as the rate-limit client identity
    /// in place of the peer/`X-Forwarded-For` path. See [`crate::edge`].
    #[must_use]
    pub fn with_edge_trust(mut self, edge_trust: EdgeTrust) -> Self {
        self.edge_trust = edge_trust;
        self
    }
}

/// Build the auth router. `.well-known` routes are unauthenticated and
/// unthrottled; credential routes are rate-limited.
pub fn auth_router(state: AuthApiState) -> Router {
    let well_known = Router::new()
        .route("/.well-known/jwks.json", get(jwks))
        .route(
            "/.well-known/openid-configuration",
            get(openid_configuration),
        );

    let credentials = Router::new()
        .route("/signup", post(sign_up))
        .route("/signin", post(token))
        .route("/token", post(token))
        .route("/token/refresh", post(refresh))
        .route("/logout", post(logout))
        .route("/recover", post(recover))
        .route("/reset", post(reset))
        .route("/verify", post(verify))
        .route("/verify/resend", post(resend_verification))
        // `route_layer`, not `layer`: applies the middleware only to routes
        // registered on THIS router, so a request for an unmatched path under
        // the credential prefix (e.g. a bogus `POST /whatever`) 404s before
        // ever reaching `rate_limit` - it never hits the limiter (and, on the
        // Postgres backend, never writes a `rate_limit_buckets` row keyed on
        // the unauthenticated caller's arbitrary path). `layer` would instead
        // wrap the whole router including its fallback 404 handler, letting
        // an attacker grow the bucket table without bound simply by varying
        // the path.
        .route_layer(from_fn_with_state(state.clone(), rate_limit))
        // Explicit, small cap: nothing here is bigger than a credential pair or
        // a token. Over-limit bodies are refused with 413 before buffering.
        .layer(DefaultBodyLimit::max(MAX_AUTH_BODY_BYTES));

    well_known.merge(credentials).with_state(state)
}

/// Email + password credentials for sign-up and sign-in.
#[derive(Deserialize, ToSchema)]
pub(crate) struct Credentials {
    /// Account email.
    email: String,
    /// Account password.
    password: String,
}

/// A refresh-token-bearing request (refresh rotation / logout).
#[derive(Deserialize, ToSchema)]
pub(crate) struct RefreshRequest {
    /// The opaque refresh token previously issued to this client.
    refresh_token: String,
}

/// An issued access + refresh token pair.
#[derive(Serialize, ToSchema)]
pub(crate) struct TokenResponse {
    /// Short-lived access JWT (verify offline via JWKS).
    access_token: String,
    /// Opaque, rotating refresh token.
    refresh_token: String,
    /// Always `Bearer`.
    token_type: String,
    /// Access-token lifetime, seconds.
    expires_in: i64,
}

/// The generic, anti-enumeration sign-up acknowledgement.
#[derive(Serialize, ToSchema)]
pub(crate) struct SignUpResponse {
    /// A message that reveals nothing about whether the email already exists.
    message: &'static str,
}

/// A password-reset request (`POST /recover`).
#[derive(Deserialize, ToSchema)]
pub(crate) struct RecoverRequest {
    /// The account email to send a reset link to.
    email: String,
}

/// The generic, anti-enumeration recover acknowledgement.
#[derive(Serialize, ToSchema)]
pub(crate) struct RecoverResponse {
    /// A message that reveals nothing about whether the email is registered.
    message: &'static str,
}

/// A password-reset completion (`POST /reset`): the opaque reset token plus the
/// new password to install.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ResetRequest {
    /// The opaque reset token delivered out-of-band (e.g. via the reset link).
    reset_token: String,
    /// The new account password (must satisfy the password policy).
    new_password: String,
}

/// An email-verification completion (`POST /verify`): the opaque verification
/// token delivered out-of-band at sign-up.
#[derive(Deserialize, ToSchema)]
pub(crate) struct VerifyRequest {
    /// The opaque verification token delivered out-of-band (e.g. via the
    /// "confirm your email" link).
    token: String,
}

/// A request to resend the email-verification link (`POST /verify/resend`).
#[derive(Deserialize, ToSchema)]
pub(crate) struct ResendVerificationRequest {
    /// The account email to send a fresh verification link to.
    email: String,
}

/// The generic, anti-enumeration resend-verification acknowledgement.
#[derive(Serialize, ToSchema)]
pub(crate) struct ResendVerificationResponse {
    /// A message that reveals nothing about whether an unverified account exists.
    message: &'static str,
}

#[utoipa::path(
    get,
    path = "/.well-known/jwks.json",
    tag = "auth",
    responses((status = 200, description = "JSON Web Key Set: the issuer's public verification keys.")),
)]
pub(crate) async fn jwks(State(state): State<AuthApiState>) -> Json<Jwks> {
    Json(state.service.jwks())
}

#[utoipa::path(
    get,
    path = "/.well-known/openid-configuration",
    tag = "auth",
    responses((status = 200, description = "OpenID Connect discovery document.")),
)]
pub(crate) async fn openid_configuration(
    State(state): State<AuthApiState>,
) -> Json<OpenIdConfiguration> {
    Json(state.service.openid_configuration())
}

/// Anti-enumeration: both a brand-new registration and a hit on an existing
/// account return the *same* generic `202 Accepted` body, with the same Argon2
/// timing (the service hashes on both paths). Only request-level validation
/// (empty email / weak password) yields a distinguishable error, and that
/// reveals nothing about which accounts exist. Mirrors the identical-response
/// defense on `/signin`.
#[utoipa::path(
    post,
    path = "/signup",
    tag = "auth",
    request_body = Credentials,
    responses(
        (status = 202, description = "Accepted. If the email can be registered, a follow-up is sent out-of-band. Response is identical whether or not the account already exists (anti-enumeration).", body = SignUpResponse),
        (status = 400, description = "Request-level validation failed (empty email or weak password)."),
        (status = 429, description = "Rate limited."),
    ),
)]
pub(crate) async fn sign_up(
    State(state): State<AuthApiState>,
    Json(body): Json<Credentials>,
) -> Result<Response, ApiError> {
    // Deliberately discard the Created/AlreadyRegistered distinction: the
    // response must be identical on both paths (anti-enumeration). We still
    // await the call so its Argon2 work - the timing equalizer - runs.
    state
        .service
        .sign_up(&body.email, &SecretValue::new(body.password.into_bytes()))
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(SignUpResponse {
            message: "If that email can be registered, check your inbox to continue.",
        }),
    )
        .into_response())
}

#[utoipa::path(
    post,
    path = "/token",
    tag = "auth",
    request_body = Credentials,
    responses(
        (status = 200, description = "Authenticated: an access + refresh token pair.", body = TokenResponse),
        (status = 401, description = "Invalid credentials."),
        (status = 429, description = "Rate limited."),
    ),
)]
pub(crate) async fn token(
    State(state): State<AuthApiState>,
    Extension(client): Extension<ClientFingerprint>,
    Json(body): Json<Credentials>,
) -> Result<Json<TokenResponse>, ApiError> {
    let event = |name: &'static str, sev: Severity| {
        SecurityEvent::new(name, sev)
            .email(&body.email)
            .ip_hash(client.ip.as_deref())
    };
    if state
        .signin_budget_refuses(&body.email, client.net.as_deref())
        .await
    {
        event("signin_failed", Severity::Warn)
            .outcome("rate_limited")
            .reason("email_budget_exhausted")
            .emit();
        return Err(ApiError(AuthError::RateLimited));
    }
    let result = state
        .service
        .sign_in_from(
            &body.email,
            &SecretValue::new(body.password.into_bytes()),
            client.net.as_deref(),
        )
        .await;
    match result {
        Ok(pair) => {
            // Remember this network for the account (best-effort, bounded).
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(1_000),
                state
                    .known_networks
                    .record(&AuthApiState::known_key(&body.email, client.net.as_deref())),
            )
            .await;
            event("signin_succeeded", Severity::Info)
                .outcome("ok")
                .emit();
            Ok(Json(to_response(&pair)?))
        }
        Err(err) => {
            // The per-email budget counts only FAILED guesses (consulted before
            // the check via `signin_budget_refuses`, consumed here). Unknown
            // emails are counted identically (anti-enumeration).
            if matches!(err, AuthError::InvalidCredentials)
                && state.signin_budget_consume(&body.email).await
            {
                event("signin_failed", Severity::Warn)
                    .outcome("rate_limited")
                    .reason("email_rate_limited")
                    .emit();
                return Err(ApiError(AuthError::RateLimited));
            }
            let reason = match &err {
                AuthError::InvalidCredentials => "invalid_credentials",
                AuthError::Throttled { .. } => "account_backoff",
                AuthError::EmailUnverified => "email_unverified",
                _ => "error",
            };
            event("signin_failed", Severity::Warn)
                .outcome("failed")
                .reason(reason)
                .emit();
            Err(err.into())
        }
    }
}

#[utoipa::path(
    post,
    path = "/token/refresh",
    tag = "auth",
    request_body = RefreshRequest,
    responses(
        (status = 200, description = "Rotated: a fresh access + refresh token pair.", body = TokenResponse),
        (status = 401, description = "Refresh token unknown, expired, or revoked."),
        (status = 429, description = "Rate limited."),
    ),
)]
pub(crate) async fn refresh(
    State(state): State<AuthApiState>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    let pair = state
        .service
        .refresh(&SecretValue::new(body.refresh_token.into_bytes()))
        .await?;
    Ok(Json(to_response(&pair)?))
}

#[utoipa::path(
    post,
    path = "/logout",
    tag = "auth",
    request_body = RefreshRequest,
    responses(
        (status = 204, description = "Refresh token revoked (idempotent)."),
        (status = 429, description = "Rate limited."),
    ),
)]
pub(crate) async fn logout(
    State(state): State<AuthApiState>,
    Json(body): Json<RefreshRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .service
        .logout(&SecretValue::new(body.refresh_token.into_bytes()))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Request a password reset.
///
/// Anti-enumeration: this ALWAYS returns the same generic `202 Accepted`,
/// whether or not the email is registered, with no account-existence signal in
/// the body or in the timing - the service pads every branch to a fixed
/// response-time floor (`amicu_auth::service`'s `ANTI_ENUMERATION_FLOOR`). A
/// reset token is minted, persisted, and durably queued for out-of-band delivery
/// only when the account exists; the actual send happens later, in the outbox
/// drain. Mirrors the identical-response defense on `/signup`.
#[utoipa::path(
    post,
    path = "/recover",
    tag = "auth",
    request_body = RecoverRequest,
    responses(
        (status = 202, description = "Accepted. If the email is registered, a reset link is sent out-of-band. Response is identical whether or not the account exists (anti-enumeration).", body = RecoverResponse),
        (status = 429, description = "Rate limited."),
    ),
)]
pub(crate) async fn recover(
    State(state): State<AuthApiState>,
    Extension(client): Extension<ClientFingerprint>,
    Json(body): Json<RecoverRequest>,
) -> Result<Response, ApiError> {
    let event = SecurityEvent::new("reset_requested", Severity::Info)
        .email(&body.email)
        .ip_hash(client.ip.as_deref());
    if state
        .email_throttled(&state.email_limiter, "recover", &body.email)
        .await
    {
        event.outcome("rate_limited").emit();
        return Err(ApiError(AuthError::RateLimited));
    }
    // The event never says whether the account exists (anti-enumeration holds
    // for the log stream too).
    event.outcome("accepted").emit();
    state.service.request_password_reset(&body.email).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(RecoverResponse {
            message: "If that email is registered, a password-reset link is on its way.",
        }),
    )
        .into_response())
}

/// Complete a password reset: consume a valid reset token and install a new
/// password.
///
/// On success the password hash is replaced and every refresh-token family the
/// user holds is revoked, forcing a fresh sign-in on all devices.
///
/// Both failure modes return `400`, but they are deliberately *not* the same
/// message: a weak new password gets a distinct, helpful policy message, while
/// an unknown/expired/already-used token gets a generic one. This leaks nothing,
/// because the password policy is checked *before* the token is looked up - so
/// the weak-password message can never depend on (and never signals) whether the
/// token is valid or which account it belongs to.
#[utoipa::path(
    post,
    path = "/reset",
    tag = "auth",
    request_body = ResetRequest,
    responses(
        (status = 200, description = "Password reset: the new password is installed and all prior sessions (refresh families) are revoked."),
        (status = 400, description = "Either the new password fails policy (distinct, helpful message) or the reset token is unknown, expired, or already used (generic message). Policy is checked before token lookup, so neither message reveals token validity or account existence."),
        (status = 429, description = "Rate limited."),
    ),
)]
pub(crate) async fn reset(
    State(state): State<AuthApiState>,
    Extension(client): Extension<ClientFingerprint>,
    Json(body): Json<ResetRequest>,
) -> Result<StatusCode, ApiError> {
    let result = state
        .service
        .reset_password(
            &SecretValue::new(body.reset_token.into_bytes()),
            &SecretValue::new(body.new_password.into_bytes()),
        )
        .await;
    match result {
        Ok(()) => {
            SecurityEvent::new("reset_completed", Severity::Info)
                .outcome("ok")
                .ip_hash(client.ip.as_deref())
                .emit();
            Ok(StatusCode::OK)
        }
        Err(err) => {
            let reason = match &err {
                AuthError::ResetInvalid => "invalid_token",
                AuthError::WeakPassword | AuthError::PasswordTooLong => "password_policy",
                _ => "error",
            };
            SecurityEvent::new("reset_completed", Severity::Warn)
                .outcome("failed")
                .reason(reason)
                .ip_hash(client.ip.as_deref())
                .emit();
            Err(err.into())
        }
    }
}

/// Confirm an email address by consuming a verification token.
///
/// The token is consumed atomically and single-use. Every rejected token
/// (unknown, expired, or already used) returns the same generic `400`, never a
/// `401`/`404`, so a rejection cannot be used to probe which tokens or accounts
/// exist. On success the account's email is marked verified.
#[utoipa::path(
    post,
    path = "/verify",
    tag = "auth",
    request_body = VerifyRequest,
    responses(
        (status = 200, description = "Email verified: the account's email is now confirmed."),
        (status = 400, description = "The verification token is unknown, expired, or already used (generic message, no enumeration)."),
        (status = 429, description = "Rate limited."),
    ),
)]
pub(crate) async fn verify(
    State(state): State<AuthApiState>,
    Extension(client): Extension<ClientFingerprint>,
    Json(body): Json<VerifyRequest>,
) -> Result<StatusCode, ApiError> {
    let result = state
        .service
        .verify_email(&SecretValue::new(body.token.into_bytes()))
        .await;
    match result {
        Ok(()) => Ok(StatusCode::OK),
        Err(err) => {
            SecurityEvent::new("verify_failed", Severity::Warn)
                .outcome("failed")
                .reason(match &err {
                    AuthError::VerifyInvalid => "invalid_token",
                    _ => "error",
                })
                .ip_hash(client.ip.as_deref())
                .emit();
            Err(err.into())
        }
    }
}

/// Resend the email-verification link.
///
/// Anti-enumeration: this ALWAYS returns the same generic `202 Accepted`,
/// whether or not the email is registered and whether or not it is already
/// verified, with no account signal in the body or in the timing (every branch
/// is padded to the same fixed response-time floor - see `/recover`). A fresh
/// verification token is minted, persisted, and durably queued only when the
/// account exists AND is not yet verified. Mirrors the identical-response
/// defense on `/recover`.
#[utoipa::path(
    post,
    path = "/verify/resend",
    tag = "auth",
    request_body = ResendVerificationRequest,
    responses(
        (status = 202, description = "Accepted. If an unverified account exists for that email, a fresh verification link is sent out-of-band. Response is identical whether or not the account exists or is already verified (anti-enumeration).", body = ResendVerificationResponse),
        (status = 429, description = "Rate limited."),
    ),
)]
pub(crate) async fn resend_verification(
    State(state): State<AuthApiState>,
    Extension(client): Extension<ClientFingerprint>,
    Json(body): Json<ResendVerificationRequest>,
) -> Result<Response, ApiError> {
    let event = SecurityEvent::new("verify_resend_requested", Severity::Info)
        .email(&body.email)
        .ip_hash(client.ip.as_deref());
    if state
        .email_throttled(&state.email_limiter, "verify_resend", &body.email)
        .await
    {
        event.outcome("rate_limited").emit();
        return Err(ApiError(AuthError::RateLimited));
    }
    event.outcome("accepted").emit();
    state.service.resend_verification(&body.email).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ResendVerificationResponse {
            message: "If an unverified account exists for that address, a new link is on its way.",
        }),
    )
        .into_response())
}

/// Render a token pair for the wire. This is the one legitimate place the
/// opaque refresh secret is serialized: to its owner, over TLS. Shared with the
/// session/workspace-switch surface, which mints the same pair shape.
pub(crate) fn to_response(pair: &TokenPair) -> Result<TokenResponse, ApiError> {
    let refresh_token = pair
        .refresh_token
        .expose_secret_str()
        .map_err(|_| ApiError(AuthError::RefreshInvalid))?
        .to_string();
    Ok(TokenResponse {
        access_token: pair.access_token.clone(),
        refresh_token,
        token_type: pair.token_type.to_string(),
        expires_in: pair.expires_in,
    })
}

/// Rate-limit middleware for credential routes.
///
/// The bucket key is per route and per client. Client identity, in order of
/// preference:
/// 1. `CF-Connecting-IP`, but ONLY when the request presents a valid
///    `X-Amicu-Edge-Token` (andypalmi/amicu-planning#142 - see
///    [`crate::edge::EdgeTrust`]): on Cloud Run behind Cloudflare, neither the
///    peer address (Google's LB) nor `X-Forwarded-For` (ends in Cloudflare's
///    own egress IP) is the real client, so this is the only trustworthy
///    source there.
/// 2. Otherwise, the peer socket address by default; `X-Forwarded-For` is
///    honored only when that peer is in the configured trusted-proxy
///    allowlist, and then only the real client parsed right-to-left from the
///    chain, so an attacker rotating the header cannot mint a fresh bucket per
///    request.
///
/// There is no single shared fallback bucket, so no one client can lock every
/// other client out.
///
/// For the peer address to be available the server must serve this router with
/// `Router::into_make_service_with_connect_info::<SocketAddr>()`. If it is
/// absent (misconfiguration) the request is let through rather than collapsed
/// into a shared bucket, which would itself be a lockout vector.
async fn rate_limit(State(state): State<AuthApiState>, request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);
    let xff = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok());
    // andypalmi/amicu-planning#142: on Cloud Run behind Cloudflare, prefer the
    // real client IP (`CF-Connecting-IP`) once the edge token proves the
    // request actually passed through the api-proxy Worker; otherwise fall
    // back to the existing peer/XFF logic unchanged. `trusted_client_ip`
    // itself already refuses to trust the header without a valid token, so a
    // request straight to the raw *.run.app URL can never spoof this.
    let edge_ip = state.edge_trust.trusted_client_ip(request.headers());
    let ident = state
        .trusted_proxies
        .client_ident_with_edge(edge_ip, peer, xff);
    // Pseudonymize once, here: handlers only ever see the hash.
    let fingerprint = ClientFingerprint {
        ip: ident.as_deref().map(security::ip_hash),
        net: ident
            .as_deref()
            .and_then(|i| i.parse::<std::net::IpAddr>().ok())
            .map(|ip| security::pseudonym("ipnet", &network_prefix(ip))),
    };
    let throttled = match &ident {
        Some(ident) => {
            let key = format!("{}|{ident}", request.uri().path());
            !state.limiter.check(&key).await
        }
        None => false, // no trustworthy identity: fail open, never share a bucket
    };

    if throttled {
        SecurityEvent::new("ip_rate_limited", Severity::Warn)
            .outcome("rate_limited")
            .ip_hash(fingerprint.ip.as_deref())
            .emit();
        ApiError(AuthError::RateLimited).into_response()
    } else {
        let mut request = request;
        request.extensions_mut().insert(fingerprint);
        next.run(request).await
    }
}

/// HTTP error wrapper mapping [`AuthError`] to status codes without leaking
/// internal detail.
pub(crate) struct ApiError(AuthError);

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            AuthError::InvalidCredentials | AuthError::RefreshInvalid => StatusCode::UNAUTHORIZED,
            // Both are request-level 400s, not account-existence signals.
            // `WeakPassword` keeps its own distinct, helpful message; `ResetInvalid`
            // is generic (never 401/404, so a bad/expired/used token is not
            // distinguishable from a missing account). The two carry different
            // bodies, but that is safe: the password policy is enforced before the
            // token is ever looked up, so the weak-password message cannot signal
            // token validity.
            // `VerifyInvalid` joins the generic 400 bucket: an unknown, expired,
            // or already-used verification token is indistinguishable from any
            // other rejected token (never a 401/404), so it leaks nothing about
            // which tokens or accounts exist.
            AuthError::WeakPassword
            | AuthError::PasswordTooLong
            | AuthError::ResetInvalid
            | AuthError::VerifyInvalid => StatusCode::BAD_REQUEST,
            // Credentials were correct but the email is unverified (only reachable
            // once the verification gate is enabled). Distinct from the generic
            // 401 so a verified-but-wrong-password attempt cannot be told from an
            // unverified one: the gate is checked only after the password
            // verifies.
            // `Forbidden`: not a member of the target workspace - a switch (or any
            // mint path's server-side membership re-check) found no
            // `workspace_members` row; no token is minted. Shares the 403 with the
            // unverified-email gate.
            AuthError::EmailUnverified | AuthError::Forbidden => StatusCode::FORBIDDEN,
            // Internal-only; sign-up never surfaces it (anti-enumeration).
            AuthError::UserExists => StatusCode::CONFLICT,
            AuthError::RateLimited | AuthError::Throttled { .. } => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let mut response = (status, Json(json!({ "error": self.0.to_string() }))).into_response();
        if let AuthError::Throttled { retry_after_secs } = self.0
            && let Ok(v) = HeaderValue::from_str(&retry_after_secs.max(1).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, v);
        }
        response
    }
}
