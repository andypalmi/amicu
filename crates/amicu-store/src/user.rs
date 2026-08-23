//! Postgres-backed [`UserStore`]: GLOBAL identity.
//!
//! A user is one person, identified by a globally-unique email, independent of
//! any workspace (see andypalmi/amicu-planning#127). `users` carries no
//! workspace scoping and no RLS, so lookups by email/id need no workspace
//! context - which is exactly what sign-in requires, before any workspace is
//! selected.
//!
//! # Where tier and billing come from
//!
//! The [`User`] the auth service and billing surface consume still exposes a
//! `tier` and Stripe customer/subscription. In the new model those live on the
//! WORKSPACE, not the user: the workspace is the billing entity. Until the
//! access token carries a workspace claim (`wsid`, andypalmi/amicu-planning#129)
//! there is no per-request workspace to read them from, so this store resolves
//! them through `users.personal_workspace_id` - the one workspace provisioned
//! for the user at sign-up, where they are Owner. Both `users` and `workspaces`
//! are no-RLS control-plane tables, so the join needs no scoping. Once `wsid`
//! exists, tier/billing become claim-driven and this pointer's role narrows to
//! "the user's default workspace".
//!
//! # Sign-up provisioning is atomic
//!
//! [`create`](UserStore::create) inserts the user, their personal workspace, and
//! the founding Owner membership in ONE transaction (server-generated workspace
//! id): a freshly signed-up user is immediately usable, or nothing is written.

use amicu_auth::user::{User, UserStore};
use amicu_auth::{AuthError, Result, Tier};
use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{begin_scoped, map_db, now_unix_seconds, tier_from_str, tier_to_str};

/// Postgres implementation of [`UserStore`] (global identity).
pub struct PostgresUserStore {
    pool: PgPool,
}

impl PostgresUserStore {
    /// A global identity store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Parse a domain user id (the token `sub`) into the `users.id` UUID. Domain ids
/// are UUID text in the new model; an unparseable id is treated as "no such
/// user" by the read paths (returns `Ok(None)`), never an error.
fn parse_user_id(id: &str) -> Option<Uuid> {
    Uuid::parse_str(id).ok()
}

/// The columns a user lookup selects, joined across `users` and the user's
/// personal `workspaces` row, assembled into a [`User`].
struct UserRow {
    id: Uuid,
    email: String,
    password_hash: String,
    tier: String,
    stripe_customer_id: Option<String>,
    stripe_subscription_id: Option<String>,
    email_verified: bool,
}

impl UserRow {
    fn into_user(self) -> Result<User> {
        Ok(User {
            id: self.id.to_string(),
            email: self.email,
            password_hash: self.password_hash,
            tier: tier_from_str(&self.tier)?,
            // RBAC roles are not part of the identity model reworked here; a
            // fresh user has none. Workspace ROLE (owner/admin/member/viewer)
            // lives in workspace_members, distinct from the token's RBAC roles.
            roles: Vec::new(),
            stripe_customer_id: self.stripe_customer_id,
            stripe_subscription_id: self.stripe_subscription_id,
            email_verified: self.email_verified,
        })
    }
}

#[async_trait]
impl UserStore for PostgresUserStore {
    async fn create(&self, user: User) -> Result<()> {
        // The domain id (token `sub`) is the users.id UUID. The service mints a
        // UUID; anything else is a programming error, surfaced as Backend.
        let user_id = parse_user_id(&user.id).ok_or(AuthError::Backend)?;
        let workspace_id = Uuid::new_v4();
        let tier = tier_to_str(user.tier);
        let now = now_unix_seconds();
        // A globally-unique slug derived from the server-generated id (never
        // caller-influenced), so personal-workspace provisioning can never
        // collide on the slug UNIQUE constraint.
        let slug = format!("ws-{}", workspace_id.simple());
        let verified_at = user.email_verified.then_some(now);

        // Bind the new workspace so the workspace_members INSERT satisfies the
        // RLS WITH CHECK (workspace_id = app.current_workspace). users and
        // workspaces are no-RLS control-plane tables, so the GUC is inert for
        // them; it matters only for the membership row.
        let mut tx = begin_scoped(&self.pool, workspace_id.into()).await?;

        // workspaces first (users.personal_workspace_id and
        // workspace_members.workspace_id both reference it).
        if let Err(e) = sqlx::query!(
            "INSERT INTO workspaces (id, slug, name, tier, created_at) \
             VALUES ($1, $2, 'Personal workspace', $3, $4)",
            workspace_id,
            slug,
            tier,
            now,
        )
        .execute(&mut *tx)
        .await
        {
            return Err(map_db(e));
        }

        // The user. A duplicate email is the one expected, non-fatal collision.
        match sqlx::query!(
            "INSERT INTO users \
             (id, email, password_hash, email_verified_at, personal_workspace_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            user_id,
            user.email,
            user.password_hash,
            verified_at,
            workspace_id,
            now,
        )
        .execute(&mut *tx)
        .await
        {
            Ok(_) => {}
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                // Roll back the just-inserted workspace by dropping the tx.
                return Err(AuthError::UserExists);
            }
            Err(other) => return Err(map_db(other)),
        }

        // The founding Owner membership.
        sqlx::query!(
            "INSERT INTO workspace_members \
             (workspace_id, user_id, role, invited_by, joined_at) \
             VALUES ($1, $2, 'owner', NULL, $3)",
            workspace_id,
            user_id,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;

        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn get_by_email(&self, email: &str) -> Result<Option<User>> {
        let row = sqlx::query_as!(
            UserRow,
            "SELECT u.id, u.email, u.password_hash, \
                    (u.email_verified_at IS NOT NULL) AS \"email_verified!\", \
                    w.tier, w.stripe_customer_id, w.stripe_subscription_id \
             FROM users u \
             JOIN workspaces w ON w.id = u.personal_workspace_id \
             WHERE u.email = $1",
            email,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db)?;
        row.map(UserRow::into_user).transpose()
    }

    async fn get_by_id(&self, id: &str) -> Result<Option<User>> {
        let Some(uid) = parse_user_id(id) else {
            return Ok(None);
        };
        let row = sqlx::query_as!(
            UserRow,
            "SELECT u.id, u.email, u.password_hash, \
                    (u.email_verified_at IS NOT NULL) AS \"email_verified!\", \
                    w.tier, w.stripe_customer_id, w.stripe_subscription_id \
             FROM users u \
             JOIN workspaces w ON w.id = u.personal_workspace_id \
             WHERE u.id = $1",
            uid,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db)?;
        row.map(UserRow::into_user).transpose()
    }

    async fn get_by_stripe_customer(&self, customer_id: &str) -> Result<Option<User>> {
        let row = sqlx::query_as!(
            UserRow,
            "SELECT u.id, u.email, u.password_hash, \
                    (u.email_verified_at IS NOT NULL) AS \"email_verified!\", \
                    w.tier, w.stripe_customer_id, w.stripe_subscription_id \
             FROM users u \
             JOIN workspaces w ON w.id = u.personal_workspace_id \
             WHERE w.stripe_customer_id = $1",
            customer_id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(map_db)?;
        row.map(UserRow::into_user).transpose()
    }

    async fn update_password(&self, user_id: &str, password_hash: String) -> Result<()> {
        let Some(uid) = parse_user_id(user_id) else {
            return Ok(()); // unknown id: no-op, matching the trait contract
        };
        sqlx::query!(
            "UPDATE users SET password_hash = $2 WHERE id = $1",
            uid,
            password_hash,
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn set_email_verified(&self, user_id: &str) -> Result<()> {
        let Some(uid) = parse_user_id(user_id) else {
            return Ok(());
        };
        // Idempotent: keep the first verification instant if already set.
        sqlx::query!(
            "UPDATE users SET email_verified_at = COALESCE(email_verified_at, $2) \
             WHERE id = $1",
            uid,
            now_unix_seconds(),
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn set_stripe_customer(&self, user_id: &str, customer_id: String) -> Result<()> {
        let Some(uid) = parse_user_id(user_id) else {
            return Ok(());
        };
        // Billing lives on the user's personal workspace.
        sqlx::query!(
            "UPDATE workspaces SET stripe_customer_id = $2 \
             WHERE id = (SELECT personal_workspace_id FROM users WHERE id = $1)",
            uid,
            customer_id,
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn set_billing(
        &self,
        user_id: &str,
        tier: Tier,
        stripe_subscription_id: Option<String>,
    ) -> Result<()> {
        let Some(uid) = parse_user_id(user_id) else {
            return Ok(());
        };
        let tier_text = tier_to_str(tier);
        sqlx::query!(
            "UPDATE workspaces SET tier = $2, stripe_subscription_id = $3 \
             WHERE id = (SELECT personal_workspace_id FROM users WHERE id = $1)",
            uid,
            tier_text,
            stripe_subscription_id,
        )
        .execute(&self.pool)
        .await
        .map_err(map_db)?;
        Ok(())
    }

    async fn delete(&self, user_id: &str) -> Result<()> {
        let Some(uid) = parse_user_id(user_id) else {
            return Ok(());
        };
        let mut tx = self.pool.begin().await.map_err(map_db)?;
        // Capture the personal workspace before the user row (and its FK) go.
        let ws = sqlx::query!("SELECT personal_workspace_id FROM users WHERE id = $1", uid,)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_db)?;
        let Some(ws) = ws else {
            return Ok(()); // unknown user: no-op
        };
        // Deleting the user cascades their workspace_members rows and every
        // refresh/reset/verify token (the ON DELETE CASCADE the FKs declare).
        sqlx::query!("DELETE FROM users WHERE id = $1", uid)
            .execute(&mut *tx)
            .await
            .map_err(map_db)?;
        // Then the now-memberless personal workspace itself.
        sqlx::query!(
            "DELETE FROM workspaces WHERE id = $1",
            ws.personal_workspace_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }
}
