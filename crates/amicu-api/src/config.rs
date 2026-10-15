//! HTTP surface for the Management API's config storage + version history:
//! `/configs/*`.
//!
//! - `POST   /configs`                     create a config + its first version
//! - `GET    /configs`                     list configs in the workspace
//! - `GET    /configs/{id}`                current version, content included
//! - `GET    /configs/{id}/versions`       version list (metadata only)
//! - `GET    /configs/{id}/versions/{no}`  one version's content + metadata
//! - `POST   /configs/{id}/versions`       append a new version
//! - `POST   /configs/{id}/rollback/{no}`  mint a new version copying `{no}`
//! - `GET    /configs/{id}/diff`           unified diff between two versions
//!
//! HCL content is canonicalized (parse -> [`amicu_core::graph::Graph`] ->
//! [`amicu_hcl::format_graph`]) before it is ever stored, on both `create` and
//! `POST .../versions`, so every stored version - and every diff computed
//! between two of them - is stable and deterministic. Content that fails to
//! parse is a `400` carrying the parser's own diagnostic. Rejecting inline
//! literal secrets is a sibling concern (`amicu-hcl`, `andypalmi/amicu#119`),
//! not enforced here.
//!
//! Session-authed at the same Community minimum as the other engine routes:
//! every route is layered with [`engine::auth_gate`] bound to the *same*
//! [`EngineApiState`] `/validate` and `/plan` use, so the gate's configured
//! minimum tier always matches across the Management API. Every write
//! (create/append/rollback) is attributed to the caller; see
//! [`created_by_for`] for how a token's `sub` claim becomes the store's
//! `created_by` column.

use std::io::Write as _;
use std::sync::Arc;

use amicu_auth::Claims;
use amicu_store::config::{
    ConfigError, ConfigStore, ConfigSummary, ConfigWithContent, VersionContent, VersionMeta,
};
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use similar::TextDiff;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::engine::{self, EngineApiState, MAX_ENGINE_BODY_BYTES, WorkspaceContext};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Shared state for the config endpoints: the config store. `amicu-server`'s
/// `build_app` selects the concrete store the same way it does the auth
/// stores - Postgres when `DATABASE_URL` is set, in-memory otherwise.
#[derive(Clone)]
pub struct ConfigApiState {
    store: Arc<dyn ConfigStore>,
}

impl ConfigApiState {
    /// Assemble config state from a store.
    #[must_use]
    pub fn new(store: Arc<dyn ConfigStore>) -> Self {
        Self { store }
    }
}

// ---------------------------------------------------------------------------
// created_by attribution
// ---------------------------------------------------------------------------

/// Derives a stable [`Uuid`] for `created_by` attribution from a token's
/// `sub` claim.
///
/// The config store's `created_by` column holds the accounts table's internal
/// UUID surrogate id - a value the domain layer deliberately never surfaces
/// (only the string `sub` claim is; see `amicu-store::user`'s `subject`
/// column, whose doc comment spells this split out). Rather than plumb a new
/// subject -> surrogate-id resolver through this vertical (out of scope for
/// this sub-issue), every write derives a UUID v5 hash of the subject under a
/// fixed namespace: the same caller always attributes to the same UUID,
/// which is exactly what "who wrote this version" needs without an extra
/// store round-trip or a new cross-vertical dependency. `created_by` is
/// already opaque on the wire (clients never need to reverse it), so a future
/// PR that threads a real subject -> account-id resolver through can replace
/// this derivation without changing the wire contract.
fn created_by_for(claims: &Claims) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("amicu:account:{}", claims.sub).as_bytes(),
    )
}

// ---------------------------------------------------------------------------
// Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /configs`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateConfigRequest {
    /// The config's name (unique per workspace).
    pub name: String,
    /// The HCL configuration text. Canonicalized (parsed and reformatted)
    /// before it is stored.
    pub content: String,
    /// An optional human note describing this version.
    #[serde(default)]
    pub message: Option<String>,
}

/// Request body for `POST /configs/{id}/versions`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AppendVersionRequest {
    /// The HCL configuration text. Canonicalized before it is stored.
    pub content: String,
    /// An optional human note describing this version.
    #[serde(default)]
    pub message: Option<String>,
}

/// Query parameters for `GET /configs/{id}/diff`.
#[derive(Debug, Deserialize)]
pub(crate) struct DiffQuery {
    /// The version number to diff from.
    from: i32,
    /// The version number to diff to.
    to: i32,
}

// ---------------------------------------------------------------------------
// Response DTOs
// ---------------------------------------------------------------------------

/// A config's identity and pointer metadata, without content. Returned by
/// `GET /configs`. Mirrors [`ConfigSummary`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConfigSummaryDto {
    /// The config's id (UUID text).
    pub id: String,
    /// The config's name.
    pub name: String,
    /// The current version number.
    pub current_version: i32,
    /// When the current version was last touched, Unix seconds.
    pub updated_at: i64,
}

impl From<ConfigSummary> for ConfigSummaryDto {
    fn from(c: ConfigSummary) -> Self {
        Self {
            id: c.id.to_string(),
            name: c.name,
            current_version: c.current_version,
            updated_at: c.updated_at,
        }
    }
}

/// A config's current version, content included. Returned by `POST /configs`
/// and `GET /configs/{id}`. Mirrors [`ConfigWithContent`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConfigDetailDto {
    /// The config's id (UUID text).
    pub id: String,
    /// The config's name.
    pub name: String,
    /// The current version number.
    pub current_version: i32,
    /// The current version's canonical HCL content.
    pub content: String,
    /// Hex-encoded SHA-256 digest of `content`.
    pub content_sha256: String,
    /// When the current version was last touched, Unix seconds.
    pub updated_at: i64,
}

impl From<ConfigWithContent> for ConfigDetailDto {
    fn from(c: ConfigWithContent) -> Self {
        Self {
            id: c.id.to_string(),
            name: c.name,
            current_version: c.current_version,
            content: c.content,
            content_sha256: hex::encode(c.content_sha256),
            updated_at: c.updated_at,
        }
    }
}

/// One version's metadata, without its content. Returned by
/// `GET /configs/{id}/versions` and as the result of an append or rollback.
/// Mirrors [`VersionMeta`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VersionMetaDto {
    /// The version number (1-based, monotonic per config).
    pub version_no: i32,
    /// When this version was created, Unix seconds.
    pub created_at: i64,
    /// The account id (UUID text) that created this version, if any. See
    /// [`created_by_for`] for how this is derived from the caller's token.
    pub created_by: Option<String>,
    /// An optional human note describing this version.
    pub message: Option<String>,
    /// Hex-encoded SHA-256 digest of this version's content.
    pub content_sha256: String,
}

impl From<VersionMeta> for VersionMetaDto {
    fn from(v: VersionMeta) -> Self {
        Self {
            version_no: v.version_no,
            created_at: v.created_at,
            created_by: v.created_by.map(|u| u.to_string()),
            message: v.message,
            content_sha256: hex::encode(v.content_sha256),
        }
    }
}

/// One version's full content plus its metadata. Returned by
/// `GET /configs/{id}/versions/{no}`. Mirrors [`VersionContent`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VersionContentDto {
    /// The version number (1-based, monotonic per config).
    pub version_no: i32,
    /// This version's canonical HCL content.
    pub content: String,
    /// Hex-encoded SHA-256 digest of `content`.
    pub content_sha256: String,
    /// When this version was created, Unix seconds.
    pub created_at: i64,
    /// The account id (UUID text) that created this version, if any.
    pub created_by: Option<String>,
    /// An optional human note describing this version.
    pub message: Option<String>,
}

impl From<VersionContent> for VersionContentDto {
    fn from(v: VersionContent) -> Self {
        Self {
            version_no: v.version_no,
            content: v.content,
            content_sha256: hex::encode(v.content_sha256),
            created_at: v.created_at,
            created_by: v.created_by.map(|u| u.to_string()),
            message: v.message,
        }
    }
}

/// Response for `GET /configs/{id}/diff`: a unified diff between two
/// versions' canonical HCL content.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConfigDiffDto {
    /// The version number diffed from.
    pub from: i32,
    /// The version number diffed to.
    pub to: i32,
    /// A standard unified diff (`---`/`+++` headers, `@@` hunks) from
    /// `from`'s content to `to`'s.
    pub diff: String,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error surfaced by the config handlers.
pub(crate) enum ConfigHttpError {
    /// The submitted HCL failed to parse, or the resulting graph could not be
    /// canonicalized (bad request); carries the parser's own diagnostic.
    Canonicalize(amicu_hcl::HclError),
    /// No config, or no such version, exists for the request (not found).
    NotFound(String),
    /// A config name is already taken in this workspace (conflict).
    DuplicateName(String),
    /// An unexpected store failure (internal).
    Backend,
}

impl From<ConfigError> for ConfigHttpError {
    fn from(e: ConfigError) -> Self {
        match e {
            ConfigError::DuplicateName(n) => Self::DuplicateName(n),
            ConfigError::NotFound => Self::NotFound("config not found".to_owned()),
            ConfigError::VersionNotFound(v) => Self::NotFound(format!("version {v} not found")),
            ConfigError::Database(_) | ConfigError::Backend => {
                let _ = writeln!(std::io::stderr(), "amicu-api: config store error: {e}");
                Self::Backend
            }
        }
    }
}

impl From<amicu_hcl::HclError> for ConfigHttpError {
    fn from(e: amicu_hcl::HclError) -> Self {
        Self::Canonicalize(e)
    }
}

impl IntoResponse for ConfigHttpError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ConfigHttpError::Canonicalize(e) => (StatusCode::BAD_REQUEST, e.to_string()),
            ConfigHttpError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ConfigHttpError::DuplicateName(n) => (
                StatusCode::CONFLICT,
                format!("a config named {n:?} already exists"),
            ),
            // A store-backend failure is internal detail. Log the real cause
            // and return a fixed, generic body so nothing about the store
            // internals leaks to the client.
            ConfigHttpError::Backend => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_owned(),
            ),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// Parses HCL and reformats it through the one canonical emitter
/// ([`amicu_hcl::format_graph`]), so stored content - and every diff computed
/// over it - is stable and deterministic. A parse or format failure is the
/// caller's own malformed HCL, surfaced with the parser's diagnostic.
fn canonicalize(content: &str) -> Result<String, ConfigHttpError> {
    let graph = amicu_engine::parse_config(content)?;
    Ok(amicu_hcl::format_graph(&graph)?)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Create a config from submitted HCL, canonicalizing it first.
#[utoipa::path(
    post,
    path = "/configs",
    tag = "config",
    request_body = CreateConfigRequest,
    responses(
        (status = 201, description = "The created config's first version, content included.", body = ConfigDetailDto),
        (status = 400, description = "The submitted HCL failed to parse."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 409, description = "A config with this name already exists."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn create_config_handler(
    State(state): State<ConfigApiState>,
    Extension(claims): Extension<Claims>,
    Extension(ctx): Extension<WorkspaceContext>,
    Json(body): Json<CreateConfigRequest>,
) -> Result<(StatusCode, Json<ConfigDetailDto>), ConfigHttpError> {
    let content = canonicalize(&body.content)?;
    let created_by = created_by_for(&claims);
    let created = state
        .store
        .create(
            ctx.workspace,
            &body.name,
            &content,
            Some(created_by),
            body.message.as_deref(),
        )
        .await?;
    let detail = state
        .store
        .get_current(ctx.workspace, created.id)
        .await?
        .ok_or(ConfigHttpError::Backend)?;
    Ok((StatusCode::CREATED, Json(ConfigDetailDto::from(detail))))
}

/// List every config in the workspace.
#[utoipa::path(
    get,
    path = "/configs",
    tag = "config",
    responses(
        (status = 200, description = "Every config in the workspace, by name.", body = Vec<ConfigSummaryDto>),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn list_configs_handler(
    State(state): State<ConfigApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
) -> Result<Json<Vec<ConfigSummaryDto>>, ConfigHttpError> {
    let configs = state.store.list(ctx.workspace).await?;
    Ok(Json(
        configs.into_iter().map(ConfigSummaryDto::from).collect(),
    ))
}

/// Fetch a config's current version, content included.
#[utoipa::path(
    get,
    path = "/configs/{id}",
    tag = "config",
    params(("id" = String, Path, description = "The config's id (UUID).")),
    responses(
        (status = 200, description = "The current version, content included.", body = ConfigDetailDto),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 404, description = "No config exists with this id."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn get_config_handler(
    State(state): State<ConfigApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<ConfigDetailDto>, ConfigHttpError> {
    let config = state
        .store
        .get_current(ctx.workspace, id)
        .await?
        .ok_or_else(|| ConfigHttpError::NotFound(format!("no config with id `{id}`")))?;
    Ok(Json(ConfigDetailDto::from(config)))
}

/// List every version of a config, oldest first (metadata only, no content).
#[utoipa::path(
    get,
    path = "/configs/{id}/versions",
    tag = "config",
    params(("id" = String, Path, description = "The config's id (UUID).")),
    responses(
        (status = 200, description = "Every version, oldest first. Empty (not 404) for an unknown config id.", body = Vec<VersionMetaDto>),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn list_versions_handler(
    State(state): State<ConfigApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<VersionMetaDto>>, ConfigHttpError> {
    let versions = state.store.list_versions(ctx.workspace, id).await?;
    Ok(Json(
        versions.into_iter().map(VersionMetaDto::from).collect(),
    ))
}

/// Fetch one specific version's content and metadata.
#[utoipa::path(
    get,
    path = "/configs/{id}/versions/{no}",
    tag = "config",
    params(
        ("id" = String, Path, description = "The config's id (UUID)."),
        ("no" = i32, Path, description = "The version number."),
    ),
    responses(
        (status = 200, description = "That version's full content and metadata.", body = VersionContentDto),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 404, description = "No config, or no such version, exists."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn get_version_handler(
    State(state): State<ConfigApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
    Path((id, no)): Path<(Uuid, i32)>,
) -> Result<Json<VersionContentDto>, ConfigHttpError> {
    let version = state
        .store
        .get_version(ctx.workspace, id, no)
        .await?
        .ok_or_else(|| ConfigHttpError::NotFound(format!("no version {no} for config `{id}`")))?;
    Ok(Json(VersionContentDto::from(version)))
}

/// Append a new version onto a config, canonicalizing its content first.
#[utoipa::path(
    post,
    path = "/configs/{id}/versions",
    tag = "config",
    params(("id" = String, Path, description = "The config's id (UUID).")),
    request_body = AppendVersionRequest,
    responses(
        (status = 201, description = "The newly-appended version's metadata.", body = VersionMetaDto),
        (status = 400, description = "The submitted HCL failed to parse."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 404, description = "No config exists with this id."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn append_version_handler(
    State(state): State<ConfigApiState>,
    Extension(claims): Extension<Claims>,
    Extension(ctx): Extension<WorkspaceContext>,
    Path(id): Path<Uuid>,
    Json(body): Json<AppendVersionRequest>,
) -> Result<(StatusCode, Json<VersionMetaDto>), ConfigHttpError> {
    let content = canonicalize(&body.content)?;
    let created_by = created_by_for(&claims);
    let version = state
        .store
        .append_version(
            ctx.workspace,
            id,
            &content,
            Some(created_by),
            body.message.as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(VersionMetaDto::from(version))))
}

/// Roll back a config to an earlier version's content.
///
/// History is append-only: this mints a brand-new version whose content
/// copies version `{no}`; it never rewrites or removes any prior version.
#[utoipa::path(
    post,
    path = "/configs/{id}/rollback/{no}",
    tag = "config",
    params(
        ("id" = String, Path, description = "The config's id (UUID)."),
        ("no" = i32, Path, description = "The version number to roll back to."),
    ),
    responses(
        (status = 201, description = "The freshly-minted version copying `no`'s content.", body = VersionMetaDto),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 404, description = "No config, or no such version, exists."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn rollback_handler(
    State(state): State<ConfigApiState>,
    Extension(claims): Extension<Claims>,
    Extension(ctx): Extension<WorkspaceContext>,
    Path((id, no)): Path<(Uuid, i32)>,
) -> Result<(StatusCode, Json<VersionMetaDto>), ConfigHttpError> {
    let created_by = created_by_for(&claims);
    let version = state
        .store
        .rollback(ctx.workspace, id, no, Some(created_by))
        .await?;
    Ok((StatusCode::CREATED, Json(VersionMetaDto::from(version))))
}

/// Compute a unified diff between two versions' canonical HCL content.
#[utoipa::path(
    get,
    path = "/configs/{id}/diff",
    tag = "config",
    params(
        ("id" = String, Path, description = "The config's id (UUID)."),
        ("from" = i32, Query, description = "The version number to diff from."),
        ("to" = i32, Query, description = "The version number to diff to."),
    ),
    responses(
        (status = 200, description = "A unified diff from `from`'s content to `to`'s.", body = ConfigDiffDto),
        (status = 400, description = "Missing or malformed `from`/`to` query parameters."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 404, description = "No config, or no such version (from or to), exists."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn diff_handler(
    State(state): State<ConfigApiState>,
    Extension(ctx): Extension<WorkspaceContext>,
    Path(id): Path<Uuid>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<ConfigDiffDto>, ConfigHttpError> {
    let from = state
        .store
        .get_version(ctx.workspace, id, query.from)
        .await?
        .ok_or_else(|| {
            ConfigHttpError::NotFound(format!("no version {} for config `{id}`", query.from))
        })?;
    let to = state
        .store
        .get_version(ctx.workspace, id, query.to)
        .await?
        .ok_or_else(|| {
            ConfigHttpError::NotFound(format!("no version {} for config `{id}`", query.to))
        })?;

    let from_label = format!("v{}", query.from);
    let to_label = format!("v{}", query.to);
    let diff = TextDiff::from_lines(&from.content, &to.content)
        .unified_diff()
        .header(&from_label, &to_label)
        .to_string();

    Ok(Json(ConfigDiffDto {
        from: query.from,
        to: query.to,
        diff,
    }))
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the `/configs/*` router. Every route is session-authed via
/// [`engine::auth_gate`], layered with `engine_state` - the *same* state
/// `/validate` and `/plan` are gated by, so the Management API's minimum tier
/// (Community by default) stays in lock-step across the whole surface.
pub fn config_router(state: ConfigApiState, engine_state: EngineApiState) -> Router {
    Router::new()
        .route(
            "/configs",
            post(create_config_handler).get(list_configs_handler),
        )
        .route("/configs/{id}", get(get_config_handler))
        .route(
            "/configs/{id}/versions",
            get(list_versions_handler).post(append_version_handler),
        )
        .route("/configs/{id}/versions/{no}", get(get_version_handler))
        .route("/configs/{id}/rollback/{no}", post(rollback_handler))
        .route("/configs/{id}/diff", get(diff_handler))
        .layer(DefaultBodyLimit::max(MAX_ENGINE_BODY_BYTES))
        .layer(from_fn_with_state(engine_state, engine::auth_gate))
        .with_state(state)
}
