//! Postgres-backed store for HCL configs and their immutable version history.
//!
//! A [`Config`] is a named, workspace-scoped pointer at its current version;
//! every revision is recorded as an append-only row in `config_versions` that
//! the store never `UPDATE`s or `DELETE`s. [`PostgresConfigStore::rollback`]
//! does not rewrite history: it mints a brand-new version whose content copies
//! an older one, so the full history stays intact and every past revision
//! remains fetchable via [`PostgresConfigStore::list_versions`] /
//! [`PostgresConfigStore::get_version`].
//!
//! Content is stored exactly as the caller supplies it; canonicalizing HCL is
//! the API layer's job (a later sub-issue), not the store's. `content_sha256`
//! is always computed server-side so it can never disagree with `content`.
//!
//! Every statement is workspace-scoped and runs inside a GUC-bound transaction
//! (see [`crate::begin_scoped`]).

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{WorkspaceId, begin_scoped, now_unix_seconds};

/// Errors surfaced by [`PostgresConfigStore`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A database driver / query error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// [`PostgresConfigStore::create`] was asked to create a config whose
    /// name is already taken in the workspace.
    #[error("a config named {0:?} already exists")]
    DuplicateName(String),

    /// The target config id has no row in this workspace.
    #[error("config not found")]
    NotFound,

    /// [`PostgresConfigStore::rollback`] was asked to roll back to a version
    /// number that has no row in `config_versions`.
    #[error("version {0} does not exist")]
    VersionNotFound(i32),

    /// A stored value violated an invariant the store itself guarantees (e.g.
    /// a hash column of the wrong length). Never expected in practice.
    #[error("config store data integrity fault")]
    Backend,
}

/// Result alias for [`PostgresConfigStore`] operations.
pub type Result<T> = std::result::Result<T, ConfigError>;

/// A config's identity and pointer metadata, without content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub id: Uuid,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub current_version: i32,
}

/// A row in a config listing: identity, pointer, and freshness, no content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSummary {
    pub id: Uuid,
    pub name: String,
    pub current_version: i32,
    pub updated_at: i64,
}

/// A config's current version, content included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWithContent {
    pub id: Uuid,
    pub name: String,
    pub current_version: i32,
    pub content: String,
    pub content_sha256: [u8; 32],
    pub updated_at: i64,
}

/// One version's metadata, without its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMeta {
    pub version_no: i32,
    pub created_at: i64,
    pub created_by: Option<Uuid>,
    pub message: Option<String>,
    pub content_sha256: [u8; 32],
}

/// One version's full content plus its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionContent {
    pub version_no: i32,
    pub content: String,
    pub content_sha256: [u8; 32],
    pub created_at: i64,
    pub created_by: Option<Uuid>,
    pub message: Option<String>,
}

/// The columns a version-metadata query selects, before the raw hash bytes
/// are validated into a fixed-size array.
struct VersionMetaRow {
    version_no: i32,
    created_at: i64,
    created_by: Option<Uuid>,
    message: Option<String>,
    content_sha256: Vec<u8>,
}

impl VersionMetaRow {
    fn into_meta(self) -> Result<VersionMeta> {
        Ok(VersionMeta {
            version_no: self.version_no,
            created_at: self.created_at,
            created_by: self.created_by,
            message: self.message,
            content_sha256: hash_from_bytes(self.content_sha256)?,
        })
    }
}

/// The columns a version-content query selects, before the raw hash bytes are
/// validated into a fixed-size array.
struct VersionContentRow {
    version_no: i32,
    content: String,
    content_sha256: Vec<u8>,
    created_at: i64,
    created_by: Option<Uuid>,
    message: Option<String>,
}

impl VersionContentRow {
    fn into_content(self) -> Result<VersionContent> {
        Ok(VersionContent {
            version_no: self.version_no,
            content: self.content,
            content_sha256: hash_from_bytes(self.content_sha256)?,
            created_at: self.created_at,
            created_by: self.created_by,
            message: self.message,
        })
    }
}

/// The columns a current-version-with-content query selects, before the raw
/// hash bytes are validated into a fixed-size array.
struct ConfigWithContentRow {
    id: Uuid,
    name: String,
    current_version: i32,
    content: String,
    content_sha256: Vec<u8>,
    updated_at: i64,
}

impl ConfigWithContentRow {
    fn into_config(self) -> Result<ConfigWithContent> {
        Ok(ConfigWithContent {
            id: self.id,
            name: self.name,
            current_version: self.current_version,
            content: self.content,
            content_sha256: hash_from_bytes(self.content_sha256)?,
            updated_at: self.updated_at,
        })
    }
}

/// Converts a stored BYTEA hash back into the fixed-size array. A wrong
/// length is a data-integrity fault (the store only ever writes 32-byte
/// SHA-256 digests).
fn hash_from_bytes(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes.try_into().map_err(|_| ConfigError::Backend)
}

/// The SHA-256 digest of `content`, computed server-side so it always matches
/// what was actually stored.
fn content_sha256(content: &str) -> [u8; 32] {
    Sha256::digest(content.as_bytes()).into()
}

/// Postgres implementation of the HCL config + version-history store.
pub struct PostgresConfigStore {
    pool: PgPool,
    workspace: WorkspaceId,
}

impl PostgresConfigStore {
    /// A store bound to the single-tenant default workspace.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            workspace: WorkspaceId::DEFAULT,
        }
    }

    /// A store bound to a specific workspace (multi-tenant, future use).
    #[must_use]
    pub fn with_workspace(pool: PgPool, workspace: WorkspaceId) -> Self {
        Self { pool, workspace }
    }

    /// Begins a GUC-bound transaction scoped to this store's workspace.
    async fn begin(&self) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
        begin_scoped(&self.pool, self.workspace)
            .await
            .map_err(|_| ConfigError::Backend)
    }

    /// Creates a new config together with its first version (`version_no =
    /// 1`), atomically in one transaction. `content` is stored exactly as
    /// given.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::DuplicateName`] if `name` is already taken in
    /// this workspace.
    pub async fn create(
        &self,
        name: &str,
        content: &str,
        created_by: Option<Uuid>,
        message: Option<&str>,
    ) -> Result<Config> {
        let mut tx = self.begin().await?;
        let wsid = self.workspace.as_uuid();
        let id = Uuid::new_v4();
        let now = now_unix_seconds();
        let sha = content_sha256(content);

        let insert_config = sqlx::query!(
            "INSERT INTO configs \
             (workspace_id, id, name, created_at, updated_at, current_version) \
             VALUES ($1, $2, $3, $4, $4, 1)",
            wsid,
            id,
            name,
            now,
        )
        .execute(&mut *tx)
        .await;

        match insert_config {
            Ok(_) => {}
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                return Err(ConfigError::DuplicateName(name.to_string()));
            }
            Err(other) => return Err(other.into()),
        }

        sqlx::query!(
            "INSERT INTO config_versions \
             (workspace_id, config_id, version_no, content, content_sha256, \
              created_at, created_by, message) \
             VALUES ($1, $2, 1, $3, $4, $5, $6, $7)",
            wsid,
            id,
            content,
            sha.as_slice(),
            now,
            created_by,
            message,
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(Config {
            id,
            name: name.to_string(),
            created_at: now,
            updated_at: now,
            current_version: 1,
        })
    }

    /// Lists every config in this workspace, ordered by name.
    pub async fn list(&self) -> Result<Vec<ConfigSummary>> {
        let mut tx = self.begin().await?;
        let wsid = self.workspace.as_uuid();
        let rows = sqlx::query_as!(
            ConfigSummary,
            "SELECT id, name, current_version, updated_at \
             FROM configs WHERE workspace_id = $1 ORDER BY name",
            wsid,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Fetches a config's current version, content included.
    pub async fn get_current(&self, id: Uuid) -> Result<Option<ConfigWithContent>> {
        let mut tx = self.begin().await?;
        let wsid = self.workspace.as_uuid();
        let row = sqlx::query_as!(
            ConfigWithContentRow,
            "SELECT c.id, c.name, c.current_version, cv.content, cv.content_sha256, \
                    c.updated_at \
             FROM configs c \
             JOIN config_versions cv \
               ON cv.workspace_id = c.workspace_id AND cv.config_id = c.id \
                  AND cv.version_no = c.current_version \
             WHERE c.workspace_id = $1 AND c.id = $2",
            wsid,
            id,
        )
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.map(ConfigWithContentRow::into_config).transpose()
    }

    /// Lists every version of a config (metadata only, no content), oldest
    /// first. Returns an empty list for an unknown config id.
    pub async fn list_versions(&self, id: Uuid) -> Result<Vec<VersionMeta>> {
        let mut tx = self.begin().await?;
        let wsid = self.workspace.as_uuid();
        let rows = sqlx::query_as!(
            VersionMetaRow,
            "SELECT version_no, created_at, created_by, message, content_sha256 \
             FROM config_versions WHERE workspace_id = $1 AND config_id = $2 \
             ORDER BY version_no",
            wsid,
            id,
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.into_iter().map(VersionMetaRow::into_meta).collect()
    }

    /// Fetches one specific version's content and metadata.
    pub async fn get_version(&self, id: Uuid, version_no: i32) -> Result<Option<VersionContent>> {
        let mut tx = self.begin().await?;
        let wsid = self.workspace.as_uuid();
        let row = sqlx::query_as!(
            VersionContentRow,
            "SELECT version_no, content, content_sha256, created_at, created_by, message \
             FROM config_versions \
             WHERE workspace_id = $1 AND config_id = $2 AND version_no = $3",
            wsid,
            id,
            version_no,
        )
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.map(VersionContentRow::into_content).transpose()
    }

    /// Appends a new version onto a config: bumps `current_version` to
    /// `max(version_no) + 1`, inserts the new version row, and touches
    /// `updated_at`, atomically in one transaction. `content` is stored
    /// exactly as given.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NotFound`] if `id` has no row in this
    /// workspace.
    pub async fn append_version(
        &self,
        id: Uuid,
        content: &str,
        created_by: Option<Uuid>,
        message: Option<&str>,
    ) -> Result<VersionMeta> {
        let mut tx = self.begin().await?;
        let wsid = self.workspace.as_uuid();
        let now = now_unix_seconds();
        let sha = content_sha256(content);

        // Atomic bump: the row lock taken by this UPDATE serializes concurrent
        // appends onto the same config, so two racing writers cannot mint the
        // same version_no.
        let bumped = sqlx::query!(
            "UPDATE configs SET current_version = current_version + 1, updated_at = $3 \
             WHERE workspace_id = $1 AND id = $2 \
             RETURNING current_version",
            wsid,
            id,
            now,
        )
        .fetch_optional(&mut *tx)
        .await?;

        let Some(bumped) = bumped else {
            return Err(ConfigError::NotFound);
        };
        let version_no = bumped.current_version;

        sqlx::query!(
            "INSERT INTO config_versions \
             (workspace_id, config_id, version_no, content, content_sha256, \
              created_at, created_by, message) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            wsid,
            id,
            version_no,
            content,
            sha.as_slice(),
            now,
            created_by,
            message,
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(VersionMeta {
            version_no,
            created_at: now,
            created_by,
            message: message.map(str::to_string),
            content_sha256: sha,
        })
    }

    /// Rolls back a config to an earlier version's content by minting a
    /// brand-new version that copies it. History is never rewritten: `
    /// to_version` and every version in between stay in place, and the
    /// returned [`VersionMeta`] describes the freshly-appended version, not
    /// `to_version` itself. `created_by` attributes the rollback action itself
    /// (who triggered it), exactly like [`Self::append_version`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::VersionNotFound`] if `to_version` has no row for
    /// this config, or [`ConfigError::NotFound`] if `id` has no row in this
    /// workspace.
    pub async fn rollback(
        &self,
        id: Uuid,
        to_version: i32,
        created_by: Option<Uuid>,
    ) -> Result<VersionMeta> {
        let mut tx = self.begin().await?;
        let wsid = self.workspace.as_uuid();

        let source = sqlx::query!(
            "SELECT content FROM config_versions \
             WHERE workspace_id = $1 AND config_id = $2 AND version_no = $3",
            wsid,
            id,
            to_version,
        )
        .fetch_optional(&mut *tx)
        .await?;

        let Some(source) = source else {
            return Err(ConfigError::VersionNotFound(to_version));
        };

        let now = now_unix_seconds();
        let sha = content_sha256(&source.content);

        // Same atomic bump as append_version: the row lock serializes
        // concurrent writers onto this config.
        let bumped = sqlx::query!(
            "UPDATE configs SET current_version = current_version + 1, updated_at = $3 \
             WHERE workspace_id = $1 AND id = $2 \
             RETURNING current_version",
            wsid,
            id,
            now,
        )
        .fetch_optional(&mut *tx)
        .await?;

        let Some(bumped) = bumped else {
            return Err(ConfigError::NotFound);
        };
        let version_no = bumped.current_version;

        let message = format!("rollback to version {to_version}");
        sqlx::query!(
            "INSERT INTO config_versions \
             (workspace_id, config_id, version_no, content, content_sha256, \
              created_at, created_by, message) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            wsid,
            id,
            version_no,
            source.content,
            sha.as_slice(),
            now,
            created_by,
            message,
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(VersionMeta {
            version_no,
            created_at: now,
            created_by,
            message: Some(message),
            content_sha256: sha,
        })
    }
}
