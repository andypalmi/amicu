//! Amicu persistence: the Postgres-backed store.
//!
//! The control-plane isolation boundary is a [`WorkspaceId`] (unified with the
//! Enterprise org); a solo self-host user is a workspace-of-one. Every stored
//! row is scoped to a workspace, and PR-2 adds row-level-security policies plus
//! the real query surface on top of this scaffold.
//!
//! This crate is Postgres-only and uses `sqlx`. Queries are always
//! parameterized through the compile-time-checked `sqlx::query!` / `query_as!`
//! macros; SQL is never string-built. Timestamps are stored as Unix seconds
//! (`i64`), matching the codebase's `expires_at` convention (no chrono).
//!
//! # Offline query cache
//!
//! The `query!`/`query_as!` macros verify every statement against the live
//! schema at build time. To keep CI buildable with no database, the checked
//! metadata is committed to `.sqlx/` (via `cargo sqlx prepare`); a build with
//! no `DATABASE_URL` falls back to that cache. Only pool construction and
//! [`sqlx::migrate!`] (which embeds the migration files at compile time) need
//! no connection at all.
//!
//! # Auth stores (PR-2)
//!
//! [`user`], [`refresh`], [`reset`], and [`verify`] provide the Postgres
//! implementations of the `amicu-auth` store traits. Each is workspace-scoped:
//! every query carries
//! a `workspace_id` predicate and runs inside a transaction that sets the
//! `app.current_workspace` GUC so the row-level-security policies apply. A real
//! multi-tenant deployment threads a per-request [`WorkspaceId`]; until
//! login-routing exists, the self-host default ([`WorkspaceId::DEFAULT`]) is
//! used everywhere.
//!
//! # Config store
//!
//! [`config`] provides [`PostgresConfigStore`], a workspace-scoped store for
//! HCL configs and their immutable version history. It follows the same
//! GUC-bound-transaction and workspace-predicate pattern as the auth stores,
//! but is not one of the `amicu-auth` traits: it has its own error type
//! ([`config::ConfigError`]).
//!
//! # State backend
//!
//! [`state`] provides [`PostgresStateBackend`], the first production
//! `amicu_state::StateBackendAdapter` implementation: canonical graph
//! snapshots, history, and the lock lease that mediates concurrent writes.
//! It follows the same workspace-scoping convention as the auth stores, plus
//! a second `stack_id` predicate (see the module docs).

use amicu_auth::AuthError;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

pub mod config;
pub mod refresh;
pub mod reset;
pub mod state;
pub mod user;
pub mod verify;

pub use config::PostgresConfigStore;
pub use refresh::PostgresRefreshStore;
pub use reset::PostgresResetStore;
pub use state::PostgresStateBackend;
pub use user::PostgresUserStore;
pub use verify::PostgresVerifyStore;

/// The control-plane isolation boundary: a workspace (unified with the
/// Enterprise org). Every persisted resource is scoped to a workspace, so a
/// solo self-host deployment is simply a workspace-of-one.
///
/// Serializes transparently as the underlying UUID string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(Uuid);

impl WorkspaceId {
    /// The well-known workspace for single-tenant self-host: the nil UUID.
    ///
    /// A self-host deployment that has not provisioned distinct workspaces
    /// operates entirely within this one, so it is the sensible default.
    pub const DEFAULT: Self = Self(Uuid::nil());

    /// Creates a fresh, random workspace id (UUID v4).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wraps an existing UUID as a workspace id.
    #[must_use]
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// The well-known single-tenant workspace (the nil UUID). Equivalent to
    /// [`WorkspaceId::DEFAULT`].
    #[must_use]
    pub const fn nil() -> Self {
        Self::DEFAULT
    }

    /// Returns the underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for WorkspaceId {
    /// The single-tenant self-host workspace (the nil UUID).
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for WorkspaceId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::from_str(s).map(Self)
    }
}

impl From<Uuid> for WorkspaceId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

impl From<WorkspaceId> for Uuid {
    fn from(id: WorkspaceId) -> Self {
        id.0
    }
}

/// Errors surfaced by the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A database driver / query error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// A schema migration failed to apply.
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

/// Opens a connection pool to the given Postgres `database_url`.
///
/// The pool is created lazily: this returns once options are validated, without
/// requiring an eager connection.
pub async fn connect(database_url: &str) -> Result<PgPool, StoreError> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// Applies all embedded schema migrations to the database behind `pool`.
///
/// Migrations are embedded from `./migrations` at compile time, so this
/// function needs no filesystem access at runtime. It is idempotent: already
/// applied migrations are skipped.
pub async fn run_migrations(pool: &PgPool) -> Result<(), StoreError> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}

/// Maps a Postgres [`Tier`](amicu_auth::Tier) to its stored text form. Mirrors
/// the `#[serde(rename_all = "snake_case")]` on `Tier` so a value written here
/// round-trips through the token claims unchanged.
pub(crate) fn tier_to_str(tier: amicu_auth::Tier) -> &'static str {
    use amicu_auth::Tier;
    match tier {
        Tier::Community => "community",
        Tier::Pro => "pro",
        Tier::Enterprise => "enterprise",
        Tier::Hosted => "hosted",
    }
}

/// Parses the stored text form back into a [`Tier`](amicu_auth::Tier). An
/// unrecognized value is a data-integrity fault (the store is the only writer),
/// surfaced as [`AuthError::Backend`].
pub(crate) fn tier_from_str(text: &str) -> Result<amicu_auth::Tier, AuthError> {
    use amicu_auth::Tier;
    match text {
        "community" => Ok(Tier::Community),
        "pro" => Ok(Tier::Pro),
        "enterprise" => Ok(Tier::Enterprise),
        "hosted" => Ok(Tier::Hosted),
        _ => Err(AuthError::Backend),
    }
}

/// Coarse mapper from a driver error to the auth taxonomy. Deliberately drops
/// the underlying detail: [`AuthError`] is intentionally opaque (no query text,
/// key, or token material) and this becomes a 500 at the API boundary.
pub(crate) fn map_db(_err: sqlx::Error) -> AuthError {
    AuthError::Backend
}

/// Current wall-clock time as Unix seconds. Saturates rather than panicking if
/// the clock is before the epoch (it never is on a sane host).
pub(crate) fn now_unix_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Begins a transaction and binds the workspace GUC (`app.current_workspace`)
/// for its lifetime, so the row-level-security policies on the scoped tables
/// admit exactly this workspace's rows. Every store operation runs its queries
/// inside the returned transaction and commits it.
pub(crate) async fn begin_scoped(
    pool: &PgPool,
    workspace: WorkspaceId,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, AuthError> {
    let mut tx = pool.begin().await.map_err(map_db)?;
    let wsid = workspace.to_string();
    // `set_config(_, _, true)` is the transaction-local (SET LOCAL) form; it
    // takes a bind parameter, unlike a bare `SET LOCAL`.
    // `set_config` returns the applied value as a row; we only need the side
    // effect, so fetch and discard it.
    sqlx::query!("SELECT set_config('app.current_workspace', $1, true)", wsid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_nil() {
        assert_eq!(WorkspaceId::default(), WorkspaceId::DEFAULT);
        assert_eq!(WorkspaceId::nil(), WorkspaceId::DEFAULT);
        assert_eq!(WorkspaceId::DEFAULT.as_uuid(), Uuid::nil());
    }

    #[test]
    fn display_from_str_round_trip() {
        let id = WorkspaceId::new();
        let text = id.to_string();
        let parsed: WorkspaceId = text.parse().expect("valid uuid text round-trips");
        assert_eq!(id, parsed);
    }

    #[test]
    fn serde_json_round_trip() {
        let id = WorkspaceId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        // Transparent: serializes as the bare UUID string.
        assert_eq!(json, format!("\"{id}\""));
        let back: WorkspaceId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn uuid_conversions_round_trip() {
        let raw = Uuid::new_v4();
        let id = WorkspaceId::from_uuid(raw);
        assert_eq!(id.as_uuid(), raw);
        assert_eq!(Uuid::from(id), raw);
        assert_eq!(WorkspaceId::from(raw), id);
    }

    #[test]
    fn from_str_rejects_garbage() {
        assert!("not-a-uuid".parse::<WorkspaceId>().is_err());
    }
}
