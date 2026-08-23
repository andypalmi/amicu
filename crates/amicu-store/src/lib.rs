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
//! # Identity, membership, and auth stores
//!
//! Identity is GLOBAL; membership is the join (see
//! andypalmi/amicu-planning#127). [`user`] ([`PostgresUserStore`]) is the global
//! identity store, keyed by email/id with no workspace scoping; [`workspace`]
//! ([`PostgresWorkspaceStore`]) owns the workspace registry (tier + billing),
//! also global. [`membership`] ([`PostgresMembershipStore`]) and [`invite`]
//! ([`PostgresInviteStore`]) are the tenant-scoped stores: their queries run
//! inside a [`begin_scoped`]/[`begin_scoped_user`]/[`begin_scoped_invite`]
//! transaction that binds the matching GUC so the row-level-security policies
//! apply, and they take the workspace (or user/token) per call rather than being
//! pinned to one.
//!
//! [`refresh`], [`reset`], and [`verify`] provide the Postgres implementations
//! of the remaining `amicu-auth` store traits. These are session artifacts of
//! the global user identity (a real `user_id` foreign key into `users`, with
//! `ON DELETE CASCADE`), so - like `users` itself - they are control-plane and
//! carry no workspace scoping.
//!
//! There is no `WorkspaceId::DEFAULT` fallback in any of these reworked stores;
//! [`WorkspaceId::DEFAULT`] survives only for the not-yet-reworked self-host
//! config/state stores (andypalmi/amicu-planning#116).
//!
//! # Config store
//!
//! [`config`] provides [`config::ConfigStore`], the HCL config +
//! version-history persistence seam `amicu-api`'s Management API is wired
//! against, plus two implementations: [`PostgresConfigStore`] (workspace-scoped,
//! same GUC-bound-transaction pattern as the auth stores) and
//! [`InMemoryConfigStore`] (dev/tests/pre-database self-host, mirroring the
//! auth stores' in-memory fallback). It is not one of the `amicu-auth` traits:
//! it has its own error type ([`config::ConfigError`]).
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
pub mod invite;
pub mod membership;
pub mod refresh;
pub mod reset;
pub mod state;
pub mod user;
pub mod verify;
pub mod workspace;

pub use config::{ConfigStore, InMemoryConfigStore, PostgresConfigStore};
pub use invite::{Invite, PostgresInviteStore};
pub use membership::{Member, PostgresMembershipStore, Role};
pub use refresh::PostgresRefreshStore;
pub use reset::PostgresResetStore;
pub use state::PostgresStateBackend;
pub use user::PostgresUserStore;
pub use verify::PostgresVerifyStore;
pub use workspace::{PostgresWorkspaceStore, Workspace};

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
/// [`PgPoolOptions::connect`] connects **eagerly**: this awaits a real
/// connection (and validates the credentials/network path) before returning,
/// rather than deferring the first connection to the pool's first use. That is
/// the preferable behavior here - it turns a bad `DATABASE_URL` into an
/// immediate startup error instead of a failure on the first request.
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

// SINGLE-CONTEXT INVARIANT (upheld by all three helpers below).
//
// The `workspace_members`/`workspace_invites` RLS policies are OR-policies over
// two context GUCs (see 0003/0004). Their tenant isolation depends on AT MOST
// ONE context GUC being set per transaction: each helper below binds EXACTLY ONE
// (`app.current_workspace`, `app.current_user`, or `app.current_invite_token`)
// via SET LOCAL and touches no other, and SET LOCAL reverts it at transaction
// end so it cannot bleed into the next transaction on a pooled connection.
// Setting two at once would let a bare `SELECT` union two tenants' rows. Every
// query additionally carries its own explicit WHERE predicate as the primary
// filter; the RLS OR-policy is the backstop. The regression test
// `scoped_helpers_bind_exactly_one_context_guc` asserts the one-GUC property.

/// Begins a transaction and binds the workspace GUC (`app.current_workspace`)
/// for its lifetime, so the row-level-security policies on the scoped tables
/// admit exactly this workspace's rows. Every store operation runs its queries
/// inside the returned transaction and commits it. Binds ONLY
/// `app.current_workspace` (see the single-context invariant above).
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

/// Begins a transaction bound to a USER context (`app.current_user`), for the
/// one legitimately cross-workspace membership read: "which workspaces is this
/// user in?" (list-my-workspaces / workspace switching). The
/// `workspace_members` RLS policy admits a row whose `user_id` equals this GUC,
/// so a user always sees their own memberships across workspaces; the policy's
/// WITH CHECK is workspace-only, so this read-only context can never write.
/// `app.current_workspace` is left unset here, so no other tenant's rows leak.
pub(crate) async fn begin_scoped_user(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, AuthError> {
    let mut tx = pool.begin().await.map_err(map_db)?;
    let uid = user_id.to_string();
    sqlx::query!("SELECT set_config('app.current_user', $1, true)", uid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
    Ok(tx)
}

/// Begins a transaction bound to an INVITE-TOKEN context
/// (`app.current_invite_token`), for the one legitimately cross-workspace invite
/// read: redeeming a token whose workspace is not yet known. The unguessable
/// 32-byte token hash IS the capability; the `workspace_invites` RLS policy
/// admits exactly the invite row whose `token_hash` matches (compared as
/// lowercase hex). `app.current_workspace` is left unset, so no other invite
/// leaks, and the policy's WITH CHECK is workspace-only so this read-only
/// context cannot write.
pub(crate) async fn begin_scoped_invite<'a>(
    pool: &'a PgPool,
    token_hash: &[u8; 32],
) -> Result<sqlx::Transaction<'a, sqlx::Postgres>, AuthError> {
    let mut tx = pool.begin().await.map_err(map_db)?;
    let hex = hex_lower(token_hash);
    sqlx::query!(
        "SELECT set_config('app.current_invite_token', $1, true)",
        hex
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_db)?;
    Ok(tx)
}

/// Lowercase-hex encoding of a byte slice, for the invite-token RLS GUC (which
/// Postgres decodes with `decode(..., 'hex')`). Kept local to avoid pulling in a
/// hex crate for one 32-byte value.
pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
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

    /// Reads the three context GUCs inside `tx`, each coerced NULL -> "" so an
    /// unset (or reverted-to-empty) GUC reads as the empty string.
    #[cfg(test)]
    async fn read_context_gucs(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> (String, String, String) {
        let row = sqlx::query!(
            "SELECT \
               COALESCE(current_setting('app.current_workspace', true), '')    AS \"ws!\", \
               COALESCE(current_setting('app.current_user', true), '')         AS \"usr!\", \
               COALESCE(current_setting('app.current_invite_token', true), '') AS \"inv!\"",
        )
        .fetch_one(&mut **tx)
        .await
        .expect("read context gucs");
        (row.ws, row.usr, row.inv)
    }

    /// FIX N1 regression: each `begin_scoped_*` helper must bind EXACTLY ONE
    /// context GUC and leave the other two empty within its transaction. The
    /// `workspace_members`/`workspace_invites` OR-policies rely on this
    /// single-context invariant for tenant isolation (see the note above
    /// `begin_scoped` and the policy comments in 0003/0004). Running the three
    /// helpers in sequence on the same pool also proves SET LOCAL does not bleed
    /// one context into the next transaction.
    #[tokio::test]
    #[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
    async fn scoped_helpers_bind_exactly_one_context_guc() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for the ignored tests");
        let pool = connect(&url).await.expect("connect to Postgres");
        run_migrations(&pool).await.expect("apply migrations");

        // Workspace context: only app.current_workspace is set.
        let mut tx = begin_scoped(&pool, WorkspaceId::new())
            .await
            .expect("begin_scoped");
        let (ws, usr, inv) = read_context_gucs(&mut tx).await;
        assert!(!ws.is_empty(), "workspace GUC set");
        assert!(
            usr.is_empty(),
            "user GUC must stay empty under workspace scope"
        );
        assert!(
            inv.is_empty(),
            "invite GUC must stay empty under workspace scope"
        );
        tx.rollback().await.expect("rollback");

        // User context: only app.current_user is set (and the workspace GUC from
        // the prior transaction did not bleed through).
        let mut tx = begin_scoped_user(&pool, Uuid::new_v4())
            .await
            .expect("begin_scoped_user");
        let (ws, usr, inv) = read_context_gucs(&mut tx).await;
        assert!(
            ws.is_empty(),
            "workspace GUC must stay empty under user scope"
        );
        assert!(!usr.is_empty(), "user GUC set");
        assert!(
            inv.is_empty(),
            "invite GUC must stay empty under user scope"
        );
        tx.rollback().await.expect("rollback");

        // Invite-token context: only app.current_invite_token is set.
        let mut tx = begin_scoped_invite(&pool, &[7u8; 32])
            .await
            .expect("begin_scoped_invite");
        let (ws, usr, inv) = read_context_gucs(&mut tx).await;
        assert!(
            ws.is_empty(),
            "workspace GUC must stay empty under invite scope"
        );
        assert!(
            usr.is_empty(),
            "user GUC must stay empty under invite scope"
        );
        assert!(!inv.is_empty(), "invite GUC set");
        tx.rollback().await.expect("rollback");
    }
}
