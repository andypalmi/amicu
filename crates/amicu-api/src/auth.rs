//! HTTP surface for the auth service: `.well-known` discovery plus the
//! sign-up / token / refresh / logout endpoints, with rate limiting on the
//! credential-taking routes.

use std::net::SocketAddr;
use std::sync::Arc;

use amicu_auth::error::AuthError;
use amicu_auth::jwks::{Jwks, OpenIdConfiguration};
use amicu_auth::service::{AuthService, TokenPair};
use amicu_core::secret::SecretValue;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use crate::ratelimit::{RateLimiter, TrustedProxies};

/// Shared HTTP state.
#[derive(Clone)]
pub struct AuthApiState {
    service: Arc<AuthService>,
    limiter: Arc<RateLimiter>,
    trusted_proxies: TrustedProxies,
}

impl AuthApiState {
    /// Assemble state from a service and a rate limiter. No proxies are trusted
    /// by default; see [`AuthApiState::with_trusted_proxies`].
    #[must_use]
    pub fn new(service: Arc<AuthService>, limiter: Arc<RateLimiter>) -> Self {
        Self {
            service,
            limiter,
            trusted_proxies: TrustedProxies::default(),
        }
    }

    /// Configure the trusted-proxy allowlist used to decide whether a request's
    /// `X-Forwarded-For` header may be believed for rate-limit keying.
    #[must_use]
    pub fn with_trusted_proxies(mut self, trusted_proxies: TrustedProxies) -> Self {
        self.trusted_proxies = trusted_proxies;
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
        .layer(from_fn_with_state(state.clone(), rate_limit));

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
    Json(body): Json<Credentials>,
) -> Result<Json<TokenResponse>, ApiError> {
    let pair = state
        .service
        .sign_in(&body.email, &SecretValue::new(body.password.into_bytes()))
        .await?;
    Ok(Json(to_response(&pair)?))
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
/// the body or (Argon2 runs on neither path) the timing. A reset token is minted
/// and delivered out-of-band via the notification seam only when the account
/// exists. Mirrors the identical-response defense on `/signup`.
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
    Json(body): Json<RecoverRequest>,
) -> Result<Response, ApiError> {
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
    Json(body): Json<ResetRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .service
        .reset_password(
            &SecretValue::new(body.reset_token.into_bytes()),
            &SecretValue::new(body.new_password.into_bytes()),
        )
        .await?;
    Ok(StatusCode::OK)
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
    Json(body): Json<VerifyRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .service
        .verify_email(&SecretValue::new(body.token.into_bytes()))
        .await?;
    Ok(StatusCode::OK)
}

/// Resend the email-verification link.
///
/// Anti-enumeration: this ALWAYS returns the same generic `202 Accepted`,
/// whether or not the email is registered and whether or not it is already
/// verified, with no account signal in the body or (all account-dependent work
/// is detached) the timing. A fresh verification token is minted and delivered
/// out-of-band via the notification seam only when the account exists AND is not
/// yet verified. Mirrors the identical-response defense on `/recover`.
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
    Json(body): Json<ResendVerificationRequest>,
) -> Result<Response, ApiError> {
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
/// The bucket key is per route and per client. Client identity comes from the
/// peer socket address by default; `X-Forwarded-For` is honored only when that
/// peer is in the configured trusted-proxy allowlist, and then only the real
/// client parsed right-to-left from the chain, so an attacker rotating the
/// header cannot mint a fresh bucket per request. There is no single shared
/// fallback bucket, so no one client can lock every other client out.
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
    let throttled = match state
        .trusted_proxies
        .client_key(request.uri().path(), peer, xff)
    {
        Some(key) => !state.limiter.check(&key),
        None => false, // no trustworthy identity: fail open, never share a bucket
    };

    if throttled {
        ApiError(AuthError::RateLimited).into_response()
    } else {
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
            AuthError::WeakPassword | AuthError::ResetInvalid | AuthError::VerifyInvalid => {
                StatusCode::BAD_REQUEST
            }
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
            AuthError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.0.to_string() }))).into_response()
    }
}
