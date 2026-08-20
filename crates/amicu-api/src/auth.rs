//! HTTP surface for the auth service: `.well-known` discovery plus the
//! sign-up / token / refresh / logout endpoints, with rate limiting on the
//! credential-taking routes.

use std::sync::Arc;

use amicu_auth::error::AuthError;
use amicu_auth::jwks::{Jwks, OpenIdConfiguration};
use amicu_auth::service::{AuthService, TokenPair};
use amicu_core::secret::SecretValue;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ratelimit::RateLimiter;

/// Shared HTTP state.
#[derive(Clone)]
pub struct AuthApiState {
    service: Arc<AuthService>,
    limiter: Arc<RateLimiter>,
}

impl AuthApiState {
    /// Assemble state from a service and a rate limiter.
    #[must_use]
    pub fn new(service: Arc<AuthService>, limiter: Arc<RateLimiter>) -> Self {
        Self { service, limiter }
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
        .layer(from_fn_with_state(state.clone(), rate_limit));

    well_known.merge(credentials).with_state(state)
}

#[derive(Deserialize)]
struct Credentials {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct RefreshRequest {
    refresh_token: String,
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: i64,
}

#[derive(Serialize)]
struct SignUpResponse {
    user_id: String,
}

async fn jwks(State(state): State<AuthApiState>) -> Json<Jwks> {
    Json(state.service.jwks())
}

async fn openid_configuration(State(state): State<AuthApiState>) -> Json<OpenIdConfiguration> {
    Json(state.service.openid_configuration())
}

async fn sign_up(
    State(state): State<AuthApiState>,
    Json(body): Json<Credentials>,
) -> Result<Response, ApiError> {
    let id = state
        .service
        .sign_up(&body.email, &SecretValue::new(body.password.into_bytes()))
        .await?;
    Ok((StatusCode::CREATED, Json(SignUpResponse { user_id: id })).into_response())
}

async fn token(
    State(state): State<AuthApiState>,
    Json(body): Json<Credentials>,
) -> Result<Json<TokenResponse>, ApiError> {
    let pair = state
        .service
        .sign_in(&body.email, &SecretValue::new(body.password.into_bytes()))
        .await?;
    Ok(Json(to_response(&pair)?))
}

async fn refresh(
    State(state): State<AuthApiState>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    let pair = state
        .service
        .refresh(&SecretValue::new(body.refresh_token.into_bytes()))
        .await?;
    Ok(Json(to_response(&pair)?))
}

async fn logout(
    State(state): State<AuthApiState>,
    Json(body): Json<RefreshRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .service
        .logout(&SecretValue::new(body.refresh_token.into_bytes()))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Render a token pair for the wire. This is the one legitimate place the
/// opaque refresh secret is serialized: to its owner, over TLS.
fn to_response(pair: &TokenPair) -> Result<TokenResponse, ApiError> {
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

/// Rate-limit middleware for credential routes. Keys on the proxy-supplied
/// client IP, falling back to a shared bucket when absent.
async fn rate_limit(State(state): State<AuthApiState>, request: Request, next: Next) -> Response {
    let key = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("global")
        .to_string();

    if state.limiter.check(&key) {
        next.run(request).await
    } else {
        ApiError(AuthError::RateLimited).into_response()
    }
}

/// HTTP error wrapper mapping [`AuthError`] to status codes without leaking
/// internal detail.
struct ApiError(AuthError);

impl From<AuthError> for ApiError {
    fn from(err: AuthError) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            AuthError::InvalidCredentials | AuthError::RefreshInvalid => StatusCode::UNAUTHORIZED,
            AuthError::UserExists => StatusCode::CONFLICT,
            AuthError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.0.to_string() }))).into_response()
    }
}
