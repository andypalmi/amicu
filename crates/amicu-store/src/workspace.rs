//! Postgres-backed workspace store: the workspace registry (tier + billing).
//!
//! Global control-plane, like [`user`](crate::user): `workspaces` carries no
//! RLS, so create/get/set operations need no workspace GUC. Workspace ids are
//! ALWAYS server-generated here (never caller-chosen) - provisioning is
//! server-authoritative (see andypalmi/amicu-planning#127).

use amicu_auth::{AuthError, Result, Tier};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{WorkspaceId, begin_scoped, map_db, now_unix_seconds, tier_from_str, tier_to_str};

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

    /// Provision an ADDITIONAL workspace and enroll `owner_id` as its founding
    /// Owner, atomically, returning the SERVER-GENERATED workspace id.
    ///
    /// This is the multi-tenant analogue of sign-up's personal-workspace
    /// provisioning ([`PostgresUserStore::create`](crate::user)): a logged-in user
    /// creating a second/Nth workspace. Both the `workspaces` row and the founding
    /// `workspace_members(owner, Owner)` row are written in ONE transaction (the
    /// caller is immediately the Owner of a usable workspace, or nothing is
    /// written). The workspace id is generated here and never caller-influenced,
    /// so a caller can never land in an existing or other workspace.
    ///
    /// The new workspace starts at [`Tier::Community`]; billing upgrades are a
    /// separate, subscription-driven concern (the org console / Stripe surface).
    // `owner_id`/`owner_uid` are the `&str` and parsed-`Uuid` forms of the SAME
    // id; `clippy::similar_names` reads them as confusable, but keeping the shared
    // stem is the clearer naming (as in `membership.rs`).
    #[allow(clippy::similar_names)]
    pub async fn create_with_owner(&self, name: &str, owner_id: &str) -> Result<WorkspaceId> {
        // The owner id (a token `sub`) is a UUID in the identity model; anything
        // else is a programming error at the API boundary, surfaced as Backend.
        let owner_uid = Uuid::parse_str(owner_id).map_err(|_| AuthError::Backend)?;
        let id = Uuid::new_v4();
        // A globally-unique slug derived from the server-generated id (never
        // caller-influenced), so it can never collide on the slug UNIQUE
        // constraint - mirroring personal-workspace provisioning.
        let slug = format!("ws-{}", id.simple());
        let now = now_unix_seconds();

        // Bind the new workspace so the workspace_members INSERT satisfies the RLS
        // WITH CHECK (workspace_id = app.current_workspace). `workspaces` is a
        // no-RLS control-plane table, so the GUC is inert for it; it matters only
        // for the founding-membership row. ONE context (workspace) is bound, so
        // the single-context invariant holds.
        let mut tx = begin_scoped(&self.pool, WorkspaceId::from_uuid(id)).await?;
        sqlx::query!(
            "INSERT INTO workspaces (id, slug, name, tier, created_at) \
             VALUES ($1, $2, $3, 'community', $4)",
            id,
            slug,
            name,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        sqlx::query!(
            "INSERT INTO workspace_members \
             (workspace_id, user_id, role, invited_by, joined_at) \
             VALUES ($1, $2, 'owner', NULL, $3)",
            id,
            owner_uid,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
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
