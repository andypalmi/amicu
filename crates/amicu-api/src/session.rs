//! HTTP surface for the workspace claim in tokens
//! (andypalmi/amicu-planning#129): list the authenticated user's workspaces and
//! switch the active one.
//!
//! Both routes are authenticated (any Community+ access token) and act ONLY on
//! the caller's OWN memberships, keyed by the verified `sub`:
//! - `GET /workspaces` lists the workspaces the caller belongs to (id, name,
//!   role), resolved server-side from `workspace_members`.
//! - `POST /session/workspace` switches the active workspace: it mints a fresh
//!   token pair scoped to the target ONLY IF the caller is a member of it
//!   (re-checked server-side); otherwise `403`, no token minted. The client
//!   never asserts a workspace it isn't a member of - it names a target, and the
//!   server decides.
//!
//! The switch opens a NEW refresh family scoped to the target workspace, so the
//! new session rotates independently and re-validates membership in that
//! workspace on every refresh.

use std::sync::Arc;

use amicu_auth::service::AuthService;
use amicu_auth::{Claims, WorkspaceId};
use axum::extract::{Extension, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use crate::auth::{ApiError, TokenResponse, to_response};
use crate::engine::AccessVerifier;

/// Shared state for the session/workspace surface: the issuer service (to mint
/// switched token pairs and read memberships) plus the access-token verifier
/// (to authenticate the caller).
#[derive(Clone)]
pub struct SessionApiState {
    service: Arc<AuthService>,
    verifier: Arc<dyn AccessVerifier>,
}

impl SessionApiState {
    /// Assemble state from the auth service and the access-token verifier.
    #[must_use]
    pub fn new(service: Arc<AuthService>, verifier: Arc<dyn AccessVerifier>) -> Self {
        Self { service, verifier }
    }
}

/// One workspace the caller belongs to.
#[derive(Serialize, ToSchema)]
pub(crate) struct WorkspaceView {
    /// The workspace id (UUID).
    workspace_id: String,
    /// The workspace's human-facing name.
    name: String,
    /// The caller's role in the workspace (`owner`/`admin`/`member`/`viewer`).
    role: String,
}

/// The list-my-workspaces response.
#[derive(Serialize, ToSchema)]
pub(crate) struct WorkspacesResponse {
    /// Every workspace the authenticated caller is a member of.
    workspaces: Vec<WorkspaceView>,
}

/// A request to switch the active workspace.
#[derive(Deserialize, ToSchema)]
pub(crate) struct SwitchWorkspaceRequest {
    /// The workspace to switch to (UUID). The caller must be a member; the
    /// server re-checks and refuses (`403`) otherwise.
    workspace_id: String,
}

/// Build the session router: both routes behind the authentication gate.
pub fn session_router(state: SessionApiState) -> Router {
    Router::new()
        .route("/workspaces", get(list_workspaces_handler))
        .route("/session/workspace", post(switch_workspace_handler))
        .layer(from_fn_with_state(state.clone(), session_auth_gate))
        .with_state(state)
}

/// Extract a `Bearer` token from the `Authorization` header.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Authentication gate for the session surface: any valid access token
/// (Community+) is admitted. On success the verified [`Claims`] are stashed in
/// the request extensions so the handlers can read the caller's `sub`.
///
/// Deliberately tier-agnostic: listing and switching your OWN workspaces is not
/// a paid feature, and the per-workspace tier is a property of the target, not a
/// gate on the switch itself.
async fn session_auth_gate(
    State(state): State<SessionApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_token(request.headers()) else {
        return unauthorized("missing or malformed bearer token");
    };
    match state.verifier.verify(token).await {
        Ok(claims) => {
            request.extensions_mut().insert(claims);
            next.run(request).await
        }
        Err(_) => unauthorized("invalid or expired access token"),
    }
}

fn unauthorized(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

/// List the workspaces the authenticated caller belongs to.
///
/// Scoped to the caller's OWN memberships (keyed by the verified `sub`): a
/// caller can only ever see their own.
#[utoipa::path(
    get,
    path = "/workspaces",
    tag = "session",
    responses(
        (status = 200, description = "The authenticated caller's workspace memberships.", body = WorkspacesResponse),
        (status = 401, description = "Missing or invalid access token."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn list_workspaces_handler(
    State(state): State<SessionApiState>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<WorkspacesResponse>, ApiError> {
    let memberships = state.service.list_workspaces(&claims.sub).await?;
    let workspaces = memberships
        .into_iter()
        .map(|m| WorkspaceView {
            workspace_id: m.workspace_id.to_string(),
            name: m.name,
            role: m.role.as_str().to_string(),
        })
        .collect();
    Ok(Json(WorkspacesResponse { workspaces }))
}

/// Switch the active workspace: mint a fresh token pair scoped to the target.
///
/// The caller names a target workspace; the server mints a new pair ONLY IF
/// `workspace_members(target, sub)` exists (re-checked here, never trusted from
/// the request), and returns `403` otherwise with no token minted. The new pair
/// opens a fresh refresh family scoped to the target.
#[utoipa::path(
    post,
    path = "/session/workspace",
    tag = "session",
    request_body = SwitchWorkspaceRequest,
    responses(
        (status = 200, description = "Switched: a fresh access + refresh token pair scoped to the target workspace.", body = TokenResponse),
        (status = 400, description = "The workspace_id is not a valid workspace id."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "The caller is not a member of the target workspace; no token is minted."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn switch_workspace_handler(
    State(state): State<SessionApiState>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<SwitchWorkspaceRequest>,
) -> Response {
    let Ok(target) = body.workspace_id.parse::<WorkspaceId>() else {
        return bad_request("workspace_id is not a valid workspace id");
    };
    // Membership is re-checked SERVER-SIDE inside `switch_workspace`; a
    // non-member yields AuthError::Forbidden -> 403 with no token minted.
    match state.service.switch_workspace(&claims.sub, target).await {
        Ok(pair) => match to_response(&pair) {
            Ok(body) => Json(body).into_response(),
            Err(err) => err.into_response(),
        },
        Err(err) => ApiError::from(err).into_response(),
    }
}
