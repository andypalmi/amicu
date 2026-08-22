//! Postgres-backed [`UserStore`].
//!
//! Accounts are workspace-scoped: every statement filters on `workspace_id` and
//! runs inside a transaction that binds the `app.current_workspace` GUC so the
//! row-level-security policy applies. The domain user id (`User.id`, the token
//! `sub`) is stored in the `subject` column; the UUID `accounts.id` is an
//! internal surrogate the database fills and the domain never sees.

use amicu_auth::user::{User, UserStore};
use amicu_auth::{AuthError, Result, Tier};
use async_trait::async_trait;
use sqlx::PgPool;

use crate::{WorkspaceId, begin_scoped, map_db, now_unix_seconds, tier_from_str, tier_to_str};

/// Postgres implementation of [`UserStore`].
///
/// Holds the workspace it operates in so a real per-request workspace can be
/// threaded later; today self-host uses [`WorkspaceId::DEFAULT`] throughout.
pub struct PostgresUserStore {
    pool: PgPool,
    workspace: WorkspaceId,
}

impl PostgresUserStore {
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
}

/// The columns every user lookup selects, assembled into a [`User`].
struct AccountRow {
    subject: String,
    email: String,
    password_hash: String,
    tier: String,
    roles: Vec<String>,
    stripe_customer_id: Option<String>,
    stripe_subscription_id: Option<String>,
}

impl AccountRow {
    fn into_user(self) -> Result<User> {
        Ok(User {
            id: self.subject,
            email: self.email,
            password_hash: self.password_hash,
            tier: tier_from_str(&self.tier)?,
            roles: self.roles,
            stripe_customer_id: self.stripe_customer_id,
            stripe_subscription_id: self.stripe_subscription_id,
        })
    }
}

#[async_trait]
impl UserStore for PostgresUserStore {
    async fn create(&self, user: User) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let tier = tier_to_str(user.tier);
        let created_at = now_unix_seconds();
        let result = sqlx::query!(
            "INSERT INTO accounts \
             (workspace_id, subject, email, password_hash, tier, roles, \
              stripe_customer_id, stripe_subscription_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            wsid,
            user.id,
            user.email,
            user.password_hash,
            tier,
            &user.roles,
            user.stripe_customer_id,
            user.stripe_subscription_id,
            created_at,
        )
        .execute(&mut *tx)
        .await;

        match result {
            Ok(_) => {
                tx.commit().await.map_err(map_db)?;
                Ok(())
            }
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(AuthError::UserExists)
            }
            Err(other) => Err(map_db(other)),
        }
    }

    async fn get_by_email(&self, email: &str) -> Result<Option<User>> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let row = sqlx::query_as!(
            AccountRow,
            "SELECT subject, email, password_hash, tier, roles, \
             stripe_customer_id, stripe_subscription_id \
             FROM accounts WHERE workspace_id = $1 AND email = $2",
            wsid,
            email,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        row.map(AccountRow::into_user).transpose()
    }

    async fn get_by_id(&self, id: &str) -> Result<Option<User>> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let row = sqlx::query_as!(
            AccountRow,
            "SELECT subject, email, password_hash, tier, roles, \
             stripe_customer_id, stripe_subscription_id \
             FROM accounts WHERE workspace_id = $1 AND subject = $2",
            wsid,
            id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        row.map(AccountRow::into_user).transpose()
    }

    async fn get_by_stripe_customer(&self, customer_id: &str) -> Result<Option<User>> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let row = sqlx::query_as!(
            AccountRow,
            "SELECT subject, email, password_hash, tier, roles, \
             stripe_customer_id, stripe_subscription_id \
             FROM accounts WHERE workspace_id = $1 AND stripe_customer_id = $2",
            wsid,
            customer_id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        row.map(AccountRow::into_user).transpose()
    }

    async fn update_password(&self, user_id: &str, password_hash: String) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        sqlx::query!(
            "UPDATE accounts SET password_hash = $3 \
             WHERE workspace_id = $1 AND subject = $2",
            wsid,
            user_id,
            password_hash,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn set_stripe_customer(&self, user_id: &str, customer_id: String) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        sqlx::query!(
            "UPDATE accounts SET stripe_customer_id = $3 \
             WHERE workspace_id = $1 AND subject = $2",
            wsid,
            user_id,
            customer_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }

    async fn set_billing(
        &self,
        user_id: &str,
        tier: Tier,
        stripe_subscription_id: Option<String>,
    ) -> Result<()> {
        let mut tx = begin_scoped(&self.pool, self.workspace).await?;
        let wsid = self.workspace.as_uuid();
        let tier_text = tier_to_str(tier);
        sqlx::query!(
            "UPDATE accounts SET tier = $3, stripe_subscription_id = $4 \
             WHERE workspace_id = $1 AND subject = $2",
            wsid,
            user_id,
            tier_text,
            stripe_subscription_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
        tx.commit().await.map_err(map_db)?;
        Ok(())
    }
}
