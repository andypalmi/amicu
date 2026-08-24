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

/// Default cap on the TOTAL number of workspaces a user may OWN (`role =
/// 'owner'` in `workspace_members`, across every workspace - including the
/// personal workspace sign-up provisions), enforced by
/// [`PostgresWorkspaceStore::create_with_owner`] (andypalmi/amicu-planning#132).
/// There is no `DELETE /workspaces`, so this is a hard ceiling per owner;
/// `amicu-server` overrides it from `AMICU_MAX_WORKSPACES_PER_USER` via
/// [`PostgresWorkspaceStore::with_max_workspaces_per_user`].
pub const DEFAULT_MAX_WORKSPACES_PER_USER: u32 = 50;

/// The failure modes of [`PostgresWorkspaceStore::create_with_owner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceError {
    /// The owner already owns `max` workspaces (the configured
    /// `AMICU_MAX_WORKSPACES_PER_USER` cap), checked and enforced INSIDE the
    /// creating transaction under a per-owner advisory lock so it is exact
    /// under concurrency - see andypalmi/amicu-planning#132.
    LimitReached {
        /// The cap that was reached.
        max: u32,
    },
    /// A backend/data-integrity fault.
    Backend,
}

impl From<AuthError> for WorkspaceError {
    fn from(_: AuthError) -> Self {
        WorkspaceError::Backend
    }
}

/// Postgres implementation of the workspace store.
pub struct PostgresWorkspaceStore {
    pool: PgPool,
    max_workspaces_per_user: u32,
}

impl PostgresWorkspaceStore {
    /// A workspace store over `pool`, capped at
    /// [`DEFAULT_MAX_WORKSPACES_PER_USER`] total workspaces owned per user.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            max_workspaces_per_user: DEFAULT_MAX_WORKSPACES_PER_USER,
        }
    }

    /// Override the per-user workspace-creation cap enforced by
    /// [`create_with_owner`](Self::create_with_owner) (default
    /// [`DEFAULT_MAX_WORKSPACES_PER_USER`]). `amicu-server` wires this from
    /// `AMICU_MAX_WORKSPACES_PER_USER`; tests use a small cap to exercise the
    /// limit cheaply.
    #[must_use]
    pub fn with_max_workspaces_per_user(mut self, max: u32) -> Self {
        self.max_workspaces_per_user = max;
        self
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
    ///
    /// # Per-user cap (andypalmi/amicu-planning#132)
    /// There is no `DELETE /workspaces`, so a per-user cap on how many
    /// workspaces one user may OWN is a hard ceiling. Enforced INSIDE this
    /// transaction, before either insert: a `pg_advisory_xact_lock` keyed on
    /// `owner_id` is taken first (mirroring
    /// `PostgresMembershipStore::lock_workspace_mutations`), so two concurrent
    /// creates by the SAME owner serialize and cannot both observe a
    /// pre-increment count and both pass. Reaching the cap returns
    /// [`WorkspaceError::LimitReached`] and rolls back - nothing is inserted.
    // `owner_id`/`owner_uid` are the `&str` and parsed-`Uuid` forms of the SAME
    // id; `clippy::similar_names` reads them as confusable, but keeping the shared
    // stem is the clearer naming (as in `membership.rs`).
    #[allow(clippy::similar_names)]
    pub async fn create_with_owner(
        &self,
        name: &str,
        owner_id: &str,
    ) -> std::result::Result<WorkspaceId, WorkspaceError> {
        // The owner id (a token `sub`) is a UUID in the identity model; anything
        // else is a programming error at the API boundary, surfaced as Backend.
        let owner_uid = Uuid::parse_str(owner_id).map_err(|_| WorkspaceError::Backend)?;
        let id = Uuid::new_v4();
        // A globally-unique slug derived from the server-generated id (never
        // caller-influenced), so it can never collide on the slug UNIQUE
        // constraint - mirroring personal-workspace provisioning.
        let slug = format!("ws-{}", id.simple());
        let now = now_unix_seconds();

        let mut tx = self.pool.begin().await.map_err(map_db)?;

        // Serialize concurrent provisioning by the SAME owner FIRST (before any
        // cap read), keyed on the owner's user id - see the method docs.
        sqlx::query!(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))::text AS lock",
            owner_uid.to_string(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db)?;

        // How many workspaces does this owner ALREADY own, across every
        // workspace? Bind the USER context (never simultaneously with the
        // workspace context bound below - see the single-context invariant in
        // `lib.rs`) so the workspace_members RLS admits this user's own rows in
        // every workspace; the explicit `user_id = $1` predicate below is the
        // primary filter regardless.
        sqlx::query!(
            "SELECT set_config('app.current_user', $1, true)",
            owner_uid.to_string(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        let owned = sqlx::query!(
            "SELECT count(*) AS \"count!\" FROM workspace_members \
             WHERE user_id = $1 AND role = 'owner'",
            owner_uid,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(map_db)?
        .count;
        if owned >= i64::from(self.max_workspaces_per_user) {
            // Dropping `tx` here rolls it back: nothing is inserted.
            return Err(WorkspaceError::LimitReached {
                max: self.max_workspaces_per_user,
            });
        }

        // Clear the user context before binding the workspace context for the
        // inserts below - the two are never set at once (single-context
        // invariant). Then bind the new workspace so the workspace_members
        // INSERT satisfies the RLS WITH CHECK (workspace_id =
        // app.current_workspace); `workspaces` is a no-RLS control-plane table,
        // so the GUC is inert for it.
        sqlx::query!("SELECT set_config('app.current_user', '', true)")
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db)?;
        sqlx::query!(
            "SELECT set_config('app.current_workspace', $1, true)",
            WorkspaceId::from_uuid(id).to_string(),
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;

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
