//! Postgres-backed workspace store: the workspace registry (tier + billing).
//!
//! Global control-plane, like [`user`](crate::user): `workspaces` carries no
//! RLS, so create/get/set operations need no workspace GUC. Workspace ids are
//! ALWAYS server-generated here (never caller-chosen) - provisioning is
//! server-authoritative (see andypalmi/amicu-planning#127).

use amicu_auth::{AuthError, Result, Tier};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{WorkspaceId, map_db, now_unix_seconds, tier_from_str, tier_to_str};

/// A workspace record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Server-generated id.
    pub id: WorkspaceId,
    /// URL-safe unique slug.
    pub slug: String,
    /// Human-facing name.
    pub name: String,
    /// The workspace's plan.
    pub tier: Tier,
    /// Stripe customer id once billing is set up (`None` otherwise).
    pub stripe_customer_id: Option<String>,
    /// Stripe subscription id of the active subscription (`None` otherwise).
    pub stripe_subscription_id: Option<String>,
    /// Creation time, Unix seconds.
    pub created_at: i64,
}

/// Postgres implementation of the workspace store.
pub struct PostgresWorkspaceStore {
    pool: PgPool,
}

impl PostgresWorkspaceStore {
    /// A workspace store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a workspace at `name`/`tier` and return its SERVER-GENERATED id.
    /// The slug is derived from the id (server-authoritative), so it can never
    /// collide or be caller-influenced.
    pub async fn create(&self, name: &str, tier: Tier) -> Result<WorkspaceId> {
        let id = Uuid::new_v4();
        let slug = format!("ws-{}", id.simple());
        sqlx::query!(
            "INSERT INTO workspaces (id, slug, name, tier, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
            id,
            slug,
            name,
            tier_to_str(tier),
            now_unix_seconds(),
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(WorkspaceId::from_uuid(id))
    }

    /// Fetch a workspace by id.
    pub async fn get(&self, id: WorkspaceId) -> Result<Option<Workspace>> {
        let row = sqlx::query!(
            "SELECT id, slug, name, tier, stripe_customer_id, stripe_subscription_id, created_at \
             FROM workspaces WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db)?;
        let Some(r) = row else {
            return Ok(None);
        };
        Ok(Some(Workspace {
            id: WorkspaceId::from_uuid(r.id),
            slug: r.slug,
            name: r.name,
            tier: tier_from_str(&r.tier)?,
            stripe_customer_id: r.stripe_customer_id,
            stripe_subscription_id: r.stripe_subscription_id,
            created_at: r.created_at,
        }))
    }

    /// Set a workspace's tier. Errors [`AuthError::Backend`] if the id is
    /// unknown (unlike the user setters, tier changes target a known workspace).
    pub async fn set_tier(&self, id: WorkspaceId, tier: Tier) -> Result<()> {
        let affected = sqlx::query!(
            "UPDATE workspaces SET tier = $2 WHERE id = $1",
            id.as_uuid(),
            tier_to_str(tier),
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?
        .rows_affected();
        if affected == 0 {
            return Err(AuthError::Backend);
        }
        Ok(())
    }
}
