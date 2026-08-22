//! Amicu persistence: the Postgres-backed store.
//!
//! The control-plane isolation boundary is a [`WorkspaceId`] (unified with the
//! Enterprise org); a solo self-host user is a workspace-of-one. Every stored
//! row is scoped to a workspace, and PR-2 adds row-level-security policies plus
//! the real query surface on top of this scaffold.
//!
//! This crate is Postgres-only and uses `sqlx`. Queries are always
//! parameterized; SQL is never string-built. Timestamps are stored as Unix
//! seconds (`i64`), matching the codebase's `expires_at` convention (no
//! chrono).
//!
//! # Database-free scaffold
//!
//! This module intentionally avoids the compile-time-checked `sqlx::query!` /
//! `query_as!` macros: those require a live database or an offline cache. Only
//! pool construction and [`sqlx::migrate!`] (which embeds the migration files
//! at compile time and needs no connection) are used here. The real query
//! surface lands in PR-2.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

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
