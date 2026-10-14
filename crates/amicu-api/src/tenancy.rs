//! HTTP surface for additional-workspace provisioning and workspace invitations -
//! the final vertical of the authenticated
//! multi-tenant workspaces epic.
//!
//! Three server-authoritative, audited-or-atomic operations, all wired against
//! the durable Postgres tenancy stores:
//!
//! - [`POST /workspaces`](create_workspace_handler): an authenticated user
//!   provisions a NEW (additional) workspace. The id is SERVER-generated; a
//!   caller-supplied id in the body is ignored, so a caller can never land in an
//!   existing or other workspace. Creating the `workspaces` row and enrolling the
//!   caller as the founding Owner is ONE transaction.
//! - [`POST /org/invites`](create_invite_handler) /
//!   [`GET /org/invites`](list_invites_handler): an Owner/Admin of the caller's
//!   claim-bound workspace mints or lists pending invites. Only the SHA-256 hash
//!   of the opaque token is stored; the plaintext token is returned exactly once,
//!   to be delivered out-of-band (BYO-SMTP). The mutating create re-checks the
//!   actor's CURRENT role inside the store transaction (the #130 actor re-check).
//! - [`POST /org/invites/accept`](accept_invite_handler): any authenticated user
//!   redeems an invite token. The server hashes it, looks it up by hash, and in
//!   ONE transaction enrolls the user into the INVITE's workspace at the INVITE's
//!   role and marks the invite accepted. Membership binds to the invite's
//!   workspace/role only - the client cannot redirect it or escalate the role.
//!
//! # Claim-bound, header-free
//! `create_invite`/`list_invites` act in `ctx.workspace` (the verified
//! `claims.wsid`), never a request header. `create_workspace`/`accept_invite`
//! act as `claims.sub`. The client asserts none of it.
//!
//! # Postgres-backed
//! These are inherently multi-tenant operations (additional workspaces, shared
//! invites), so they are served only when the durable store is wired
//! ([`TenancyStore`] present). On the zero-config in-memory dev path - which is
//! single-personal-workspace by construction - they return a clean `503`,
//! mirroring how the billing surface reports itself unconfigured.

use async_trait::async_trait;
use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use utoipa::ToSchema;
use uuid::Uuid;

use amicu_auth::{Claims, Permission, Role, WorkspaceId};

use crate::engine::WorkspaceContext;
use crate::org::OrgApiState;
use crate::session::SessionApiState;

/// How long a freshly-minted invite is valid (7 days, in seconds).
const INVITE_TTL_SECS: i64 = 7 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// Store seam
// ---------------------------------------------------------------------------

/// A failure surfaced by a [`TenancyStore`] operation, mapped to a precise HTTP
/// status by [`IntoResponse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenancyError {
    /// Malformed request input (empty name/email, unknown role): `400`.
    BadRequest(String),
    /// The actor's current role does not permit the action (re-checked in the
    /// store's own transaction): `403`.
    Forbidden(String),
    /// The caller has reached `AMICU_MAX_WORKSPACES_PER_USER` - the TOTAL
    /// number of workspaces they may OWN, across every workspace including
    /// their personal one (a hard ceiling - there is no `DELETE /workspaces`),
    /// checked and enforced INSIDE the creating transaction: `403`
    LimitReached(u32),
    /// The presented invite token maps to no live invite - unknown, expired, or
    /// already accepted (kept deliberately indistinct): `400`.
    InvalidInvite,
    /// The redeeming user is already a member of the invite's workspace: `409`.
    AlreadyMember,
    /// (revoke only) no pending invite with that id exists in the caller's
    /// claim-bound workspace - unknown, already accepted, or another
    /// workspace's (kept indistinct so nothing about other tenants leaks): `404`.
    NotFound(String),
    /// The durable tenancy store is not wired (in-memory dev path): `503`.
    Unavailable,
    /// An unexpected store failure: `500`.
    Backend,
}

impl IntoResponse for TenancyError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            TenancyError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            TenancyError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            TenancyError::LimitReached(max) => (
                StatusCode::FORBIDDEN,
                format!("workspace creation limit reached ({max} per user)"),
            ),
            TenancyError::InvalidInvite => (
                StatusCode::BAD_REQUEST,
                "the invite token is invalid, expired, or already used".to_owned(),
            ),
            TenancyError::AlreadyMember => (
                StatusCode::CONFLICT,
                "you are already a member of that workspace".to_owned(),
            ),
            TenancyError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            TenancyError::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "workspace provisioning and invites require the database backend".to_owned(),
            ),
            TenancyError::Backend => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_owned(),
            ),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// A newly-provisioned workspace (server-generated id).
pub struct CreatedWorkspace {
    /// The server-generated workspace id.
    pub id: WorkspaceId,
    /// The workspace's display name.
    pub name: String,
}

/// One pending invite, for the Owner/Admin listing (never carries the token).
pub struct PendingInvite {
    /// The server-generated invite id.
    pub id: Uuid,
    /// The invited email.
    pub email: String,
    /// The role the invite grants on acceptance.
    pub role: Role,
    /// Expiry, Unix seconds.
    pub expires_at: i64,
}

/// The workspace + role a redeemed invite enrolled the caller into.
pub struct JoinedWorkspace {
    /// The INVITE's workspace (never client-asserted).
    pub workspace_id: WorkspaceId,
    /// The INVITE's role (never client-asserted).
    pub role: Role,
}

/// The durable, multi-tenant provisioning + invite authority the handlers act
/// through. One implementation today ([`PostgresTenancyStore`]); the trait keeps
/// the handlers testable and lets a future in-memory backend slot in.
#[async_trait]
pub trait TenancyStore: Send + Sync {
    /// Provision a new workspace and enroll `owner_id` as founding Owner
    /// atomically. The id is server-generated.
    async fn create_workspace(
        &self,
        name: &str,
        owner_id: &str,
    ) -> Result<CreatedWorkspace, TenancyError>;

    /// Create a pending invite to `workspace` at `role`, storing only
    /// `token_hash`, authorized against `actor_id`'s current role re-read in the
    /// store's own transaction. Returns the server-generated invite id.
    async fn create_invite(
        &self,
        workspace: WorkspaceId,
        actor_id: &str,
        email: &str,
        role: Role,
        token_hash: &[u8; 32],
        expires_at: i64,
    ) -> Result<Uuid, TenancyError>;

    /// Every pending invite for `workspace`, soonest-expiring first.
    async fn list_pending_invites(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<PendingInvite>, TenancyError>;

    /// Redeem `token_hash` and enroll `user_id` into the invite's workspace at
    /// the invite's role, atomically marking the invite accepted.
    async fn accept_invite(
        &self,
        user_id: &str,
        token_hash: &[u8; 32],
        now: i64,
    ) -> Result<JoinedWorkspace, TenancyError>;

    /// Revoke a PENDING invite `invite_id` in `workspace`, authorized against
    /// `actor_id`'s current role re-read in the store's own transaction. Only an
    /// invite belonging to `workspace` is revocable; an unknown/accepted/other-
    /// workspace id yields [`TenancyError::NotFound`].
    async fn revoke_invite(
        &self,
        workspace: WorkspaceId,
        actor_id: &str,
        invite_id: Uuid,
    ) -> Result<(), TenancyError>;
}

/// Postgres-backed [`TenancyStore`] over the durable `workspaces` /
/// `workspace_members` / `workspace_invites` tables.
pub struct PostgresTenancyStore {
    workspaces: amicu_store::PostgresWorkspaceStore,
    invites: amicu_store::PostgresInviteStore,
}

impl PostgresTenancyStore {
    /// Assemble the store from its two backing stores (built over one pool).
    #[must_use]
    pub fn new(
        workspaces: amicu_store::PostgresWorkspaceStore,
        invites: amicu_store::PostgresInviteStore,
    ) -> Self {
        Self {
            workspaces,
            invites,
        }
    }
}

impl From<amicu_store::WorkspaceError> for TenancyError {
    fn from(e: amicu_store::WorkspaceError) -> Self {
        match e {
            amicu_store::WorkspaceError::LimitReached { max } => TenancyError::LimitReached(max),
            amicu_store::WorkspaceError::Backend => TenancyError::Backend,
        }
    }
}

impl From<amicu_store::InviteError> for TenancyError {
    fn from(e: amicu_store::InviteError) -> Self {
        match e {
            amicu_store::InviteError::Forbidden(m) => TenancyError::Forbidden(m),
            amicu_store::InviteError::Invalid => TenancyError::InvalidInvite,
            amicu_store::InviteError::AlreadyMember => TenancyError::AlreadyMember,
            amicu_store::InviteError::NotFound => {
                TenancyError::NotFound("no such pending invite".to_owned())
            }
            amicu_store::InviteError::Backend => TenancyError::Backend,
        }
    }
}

#[async_trait]
impl TenancyStore for PostgresTenancyStore {
    async fn create_workspace(
        &self,
        name: &str,
        owner_id: &str,
    ) -> Result<CreatedWorkspace, TenancyError> {
        let id = self
            .workspaces
            .create_with_owner(name, owner_id)
            .await
            .map_err(TenancyError::from)?;
        Ok(CreatedWorkspace {
            id,
            name: name.to_owned(),
        })
    }

    async fn create_invite(
        &self,
        workspace: WorkspaceId,
        actor_id: &str,
        email: &str,
        role: Role,
        token_hash: &[u8; 32],
        expires_at: i64,
    ) -> Result<Uuid, TenancyError> {
        self.invites
            .create_authorized(workspace, actor_id, email, role, token_hash, expires_at)
            .await
            .map_err(TenancyError::from)
    }

    async fn list_pending_invites(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<PendingInvite>, TenancyError> {
        let invites = self
            .invites
            .list_pending(workspace)
            .await
            .map_err(|_| TenancyError::Backend)?;
        Ok(invites
            .into_iter()
            .map(|i| PendingInvite {
                id: i.id,
                email: i.email,
                role: i.role,
                expires_at: i.expires_at,
            })
            .collect())
    }

    async fn accept_invite(
        &self,
        user_id: &str,
        token_hash: &[u8; 32],
        now: i64,
    ) -> Result<JoinedWorkspace, TenancyError> {
        let accepted = self
            .invites
            .accept_and_enroll(user_id, token_hash, now)
            .await
            .map_err(TenancyError::from)?;
        Ok(JoinedWorkspace {
            workspace_id: accepted.workspace_id,
            role: accepted.role,
        })
    }

    async fn revoke_invite(
        &self,
        workspace: WorkspaceId,
        actor_id: &str,
        invite_id: Uuid,
    ) -> Result<(), TenancyError> {
        self.invites
            .revoke_authorized(workspace, actor_id, invite_id)
            .await
            .map_err(TenancyError::from)
    }
}

// ---------------------------------------------------------------------------
// Token minting / hashing
//
// The opaque invite token is generated and hashed at THIS (API) layer; the store
// only ever sees the 32-byte SHA-256 hash, matching the existing invite-store
// signatures. 244 bits of entropy (two v4 UUIDs) rendered as 64 lowercase hex
// chars - unguessable, and the token IS the capability that reveals its invite.
// ---------------------------------------------------------------------------

/// Mint a fresh opaque invite token (returned to the inviter exactly once).
fn mint_invite_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// SHA-256 of a presented invite token, the only form the server stores.
fn hash_invite_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Current wall-clock time in Unix seconds.
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------
// Wire DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /workspaces`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateWorkspaceRequest {
    /// The new workspace's display name.
    #[schema(example = "Acme Team")]
    pub name: String,
    /// IGNORED. A workspace id is always server-generated; any value supplied
    /// here is discarded so a caller can never choose or land in an existing or
    /// other workspace. Present only to document that it carries no authority.
    #[serde(default)]
    #[schema(read_only = true)]
    pub workspace_id: Option<String>,
}

/// Response for `POST /workspaces`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreateWorkspaceResponse {
    /// The SERVER-GENERATED workspace id.
    pub workspace_id: String,
    /// The workspace's display name.
    pub name: String,
    /// The caller's role in the new workspace (always `owner`).
    #[schema(example = "owner")]
    pub role: String,
}

/// Request body for `POST /org/invites`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateInviteRequest {
    /// The invitee's email (delivery is out-of-band / BYO-SMTP).
    #[schema(example = "teammate@example.com")]
    pub email: String,
    /// The role the invite grants on acceptance
    /// (`owner` | `admin` | `member` | `viewer`).
    #[schema(example = "member")]
    pub role: String,
}

/// Response for `POST /org/invites`. The plaintext `token` is returned exactly
/// once - the server stores only its hash and can never show it again.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreateInviteResponse {
    /// The server-generated invite id.
    pub invite_id: String,
    /// The invited email.
    pub email: String,
    /// The role the invite grants.
    #[schema(example = "member")]
    pub role: String,
    /// The one-time plaintext invite token. Deliver it out-of-band; it is never
    /// retrievable again.
    pub token: String,
    /// Expiry, Unix seconds.
    pub expires_at: i64,
}

/// One pending invite in the Owner/Admin listing (never carries the token).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InviteView {
    /// The invite id.
    pub invite_id: String,
    /// The invited email.
    pub email: String,
    /// The role the invite grants.
    #[schema(example = "member")]
    pub role: String,
    /// Expiry, Unix seconds.
    pub expires_at: i64,
}

/// Response for `GET /org/invites`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct InvitesResponse {
    /// The workspace's pending invites, soonest-expiring first.
    pub invites: Vec<InviteView>,
}

/// Request body for `POST /org/invites/accept`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AcceptInviteRequest {
    /// The one-time plaintext invite token.
    pub token: String,
}

/// Response for `POST /org/invites/accept`: the workspace the caller joined.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AcceptInviteResponse {
    /// The INVITE's workspace - what the membership was bound to, never a
    /// client-asserted value.
    pub workspace_id: String,
    /// The INVITE's role - what was granted, never a client-asserted value.
    #[schema(example = "member")]
    pub role: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Provision an additional workspace, enrolling the caller as its Owner.
///
/// The id is server-generated; any `workspace_id` in the body is ignored, so a
/// caller can never choose or land in an existing/other workspace. The caller
/// then switches into it via `POST /session/workspace` to obtain scoped tokens.
#[utoipa::path(
    post,
    path = "/workspaces",
    tag = "tenancy",
    request_body = CreateWorkspaceRequest,
    responses(
        (status = 201, description = "The new workspace (server-generated id); caller is Owner.", body = CreateWorkspaceResponse),
        (status = 400, description = "Empty name."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "The caller has reached the per-user workspace-creation cap."),
        (status = 503, description = "Provisioning requires the database backend."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn create_workspace_handler(
    State(state): State<SessionApiState>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<CreateWorkspaceRequest>,
) -> Response {
    let Some(tenancy) = state.tenancy() else {
        return TenancyError::Unavailable.into_response();
    };
    let name = body.name.trim();
    if name.is_empty() {
        return TenancyError::BadRequest("`name` must not be empty".to_owned()).into_response();
    }
    // `body.workspace_id` is deliberately never read: the id is server-generated.
    match tenancy.create_workspace(name, &claims.sub).await {
        Ok(ws) => (
            StatusCode::CREATED,
            Json(CreateWorkspaceResponse {
                workspace_id: ws.id.to_string(),
                name: ws.name,
                role: Role::Owner.as_str().to_owned(),
            }),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// Mint a pending invite to the caller's claim-bound workspace.
///
/// Owner/Admin only, re-checked against the actor's CURRENT role inside the
/// store transaction. The plaintext token is returned once; only its hash is
/// stored.
#[utoipa::path(
    post,
    path = "/org/invites",
    tag = "tenancy",
    request_body = CreateInviteRequest,
    responses(
        (status = 201, description = "The invite, including its one-time plaintext token.", body = CreateInviteResponse),
        (status = 400, description = "Empty email or unknown role."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, or your role may not invite this member."),
        (status = 503, description = "Invites require the database backend."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn create_invite_handler(
    State(state): State<OrgApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
    Json(body): Json<CreateInviteRequest>,
) -> Response {
    let Some(tenancy) = state.tenancy() else {
        return TenancyError::Unavailable.into_response();
    };
    let email = body.email.trim();
    if email.is_empty() {
        return TenancyError::BadRequest("`email` must not be empty".to_owned()).into_response();
    }
    let Some(role) = Role::parse_wire(&body.role) else {
        return TenancyError::BadRequest(format!("unknown role: {}", body.role)).into_response();
    };
    // Cheap claim-role pre-check (fail fast, no store round-trip); the store
    // re-checks the actor's CURRENT role in-transaction as the authority.
    if !ctx.role.grants(Permission::InviteMember) {
        return TenancyError::Forbidden("your role may not invite members".to_owned())
            .into_response();
    }

    let token = mint_invite_token();
    let token_hash = hash_invite_token(&token);
    let expires_at = now_unix() + INVITE_TTL_SECS;
    match tenancy
        .create_invite(
            ctx.workspace,
            &ctx.user_id,
            email,
            role,
            &token_hash,
            expires_at,
        )
        .await
    {
        Ok(id) => (
            StatusCode::CREATED,
            Json(CreateInviteResponse {
                invite_id: id.to_string(),
                email: email.to_owned(),
                role: role.as_str().to_owned(),
                token,
                expires_at,
            }),
        )
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// List the caller's claim-bound workspace's pending invites (Owner/Admin only).
/// Never leaks token plaintext or hashes, nor invites of any other workspace.
#[utoipa::path(
    get,
    path = "/org/invites",
    tag = "tenancy",
    responses(
        (status = 200, description = "The workspace's pending invites.", body = InvitesResponse),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, or your role may not view invites."),
        (status = 503, description = "Invites require the database backend."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn list_invites_handler(
    State(state): State<OrgApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
) -> Response {
    if !ctx.role.grants(Permission::InviteMember) {
        return TenancyError::Forbidden("your role may not view invites".to_owned())
            .into_response();
    }
    let Some(tenancy) = state.tenancy() else {
        return TenancyError::Unavailable.into_response();
    };
    match tenancy.list_pending_invites(ctx.workspace).await {
        Ok(invites) => Json(InvitesResponse {
            invites: invites
                .into_iter()
                .map(|i| InviteView {
                    invite_id: i.id.to_string(),
                    email: i.email,
                    role: i.role.as_str().to_owned(),
                    expires_at: i.expires_at,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => e.into_response(),
    }
}

/// Revoke a pending invite in the caller's claim-bound workspace.
///
/// Owner/Admin only, re-checked against the actor's CURRENT role inside the store
/// transaction (a just-demoted actor cannot revoke). Only a PENDING invite that
/// belongs to `ctx.workspace` is revocable; an invite in another workspace, an
/// unknown id, or an already-accepted invite is reported as `404` identically, so
/// nothing about other tenants leaks. Revocation is audited (`invite.revoked`).
#[utoipa::path(
    delete,
    path = "/org/invites/{id}",
    tag = "tenancy",
    params(("id" = String, Path, description = "The pending invite's id.")),
    responses(
        (status = 204, description = "The pending invite was revoked."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Below Enterprise, or your role may not revoke invites."),
        (status = 404, description = "No such pending invite in your workspace."),
        (status = 503, description = "Invites require the database backend."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn revoke_invite_handler(
    State(state): State<OrgApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
    Path(invite_id): Path<String>,
) -> Response {
    // Cheap claim-role pre-check (fail fast); the store re-checks the actor's
    // CURRENT role in-transaction as the authority.
    if !ctx.role.grants(Permission::InviteMember) {
        return TenancyError::Forbidden("your role may not revoke invites".to_owned())
            .into_response();
    }
    let Some(tenancy) = state.tenancy() else {
        return TenancyError::Unavailable.into_response();
    };
    // A malformed id cannot name a pending invite in this workspace; report it as
    // not-found (identical to a cross-workspace/unknown id) rather than leaking a
    // distinct 400 shape.
    let Ok(id) = Uuid::parse_str(invite_id.trim()) else {
        return TenancyError::NotFound("no such pending invite".to_owned()).into_response();
    };
    match tenancy.revoke_invite(ctx.workspace, &ctx.user_id, id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => e.into_response(),
    }
}

/// Redeem an invite token and join its workspace.
///
/// Any authenticated user may accept. The server hashes the presented token,
/// looks up the single invite it maps to, and - in one transaction - enrolls the
/// caller into the INVITE's workspace at the INVITE's role and marks it accepted.
/// The membership binds to the invite's workspace/role only; a tampered body
/// cannot redirect it or escalate the role.
#[utoipa::path(
    post,
    path = "/org/invites/accept",
    tag = "tenancy",
    request_body = AcceptInviteRequest,
    responses(
        (status = 200, description = "Joined: the invite's workspace and granted role.", body = AcceptInviteResponse),
        (status = 400, description = "Missing token, or the token is invalid/expired/used."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 409, description = "You are already a member of that workspace."),
        (status = 503, description = "Invites require the database backend."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn accept_invite_handler(
    State(state): State<SessionApiState>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<AcceptInviteRequest>,
) -> Response {
    let Some(tenancy) = state.tenancy() else {
        return TenancyError::Unavailable.into_response();
    };
    let token = body.token.trim();
    if token.is_empty() {
        return TenancyError::BadRequest("`token` must not be empty".to_owned()).into_response();
    }
    let token_hash = hash_invite_token(token);
    match tenancy
        .accept_invite(&claims.sub, &token_hash, now_unix())
        .await
    {
        Ok(joined) => Json(AcceptInviteResponse {
            workspace_id: joined.workspace_id.to_string(),
            role: joined.role.as_str().to_owned(),
        })
        .into_response(),
        Err(e) => e.into_response(),
    }
}
