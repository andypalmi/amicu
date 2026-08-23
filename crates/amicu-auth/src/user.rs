//! User records and the user store seam.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::claims::Tier;
use crate::error::{AuthError, Result};

/// A registered user. `password_hash` is an Argon2id PHC string, never plaintext.
#[derive(Debug, Clone)]
pub struct User {
    /// Stable user id (the token `sub`).
    pub id: String,
    /// Login email (normalized lowercase).
    pub email: String,
    /// Argon2id PHC hash.
    pub password_hash: String,
    /// Access tier. This is the single source of truth for a user's plan; billing
    /// webhooks update it (see [`UserStore::set_billing`]) and the next token the
    /// user is issued carries the new value.
    pub tier: Tier,
    /// RBAC roles.
    pub roles: Vec<String>,
    /// Stripe customer id (`cus_…`) once a checkout/portal session has been
    /// created for this user. `None` until the user first enters billing. Used to
    /// map an incoming Stripe webhook (which identifies the account only by
    /// customer id) back to the local user.
    pub stripe_customer_id: Option<String>,
    /// Stripe subscription id (`sub_…`) of the user's current active subscription,
    /// set when a subscription is created/updated and cleared when it is deleted.
    pub stripe_subscription_id: Option<String>,
    /// Whether the user has confirmed ownership of [`email`](User::email) by
    /// consuming a verification token. `false` until the email-verification flow
    /// completes (see [`crate::verify`]).
    pub email_verified: bool,
}

impl User {
    /// A newly-registered user: no billing relationship yet and an unverified
    /// email.
    #[must_use]
    pub fn new(id: String, email: String, password_hash: String, tier: Tier) -> Self {
        Self {
            id,
            email,
            password_hash,
            tier,
            roles: Vec::new(),
            stripe_customer_id: None,
            stripe_subscription_id: None,
            email_verified: false,
        }
    }
}

/// User persistence seam.
#[async_trait]
pub trait UserStore: Send + Sync {
    /// Create a user; error if the email is taken.
    async fn create(&self, user: User) -> Result<()>;
    /// Look up a user by (normalized) email.
    async fn get_by_email(&self, email: &str) -> Result<Option<User>>;
    /// Look up a user by id.
    async fn get_by_id(&self, id: &str) -> Result<Option<User>>;
    /// Look up a user by their Stripe customer id (`cus_…`). Billing webhooks
    /// identify the account only by customer id, so this is how a subscription
    /// event is mapped back to the local user. A no-match returns `Ok(None)`.
    async fn get_by_stripe_customer(&self, customer_id: &str) -> Result<Option<User>>;
    /// Replace a user's password hash (used by password reset). A no-op if the
    /// user id is unknown.
    async fn update_password(&self, user_id: &str, password_hash: String) -> Result<()>;
    /// Mark a user's email as verified (used by email verification). Idempotent
    /// and a no-op if the user id is unknown.
    async fn set_email_verified(&self, user_id: &str) -> Result<()>;
    /// Record (or update) a user's Stripe customer id. Called the first time a
    /// checkout or portal session is created so later webhooks can be mapped back
    /// to the user. A no-op if the user id is unknown.
    async fn set_stripe_customer(&self, user_id: &str, customer_id: String) -> Result<()>;
    /// Persist the outcome of a billing event: the user's new tier plus the
    /// subscription id backing it (`None` clears it, e.g. on cancellation). This
    /// is the durable store of a user's plan; the change takes effect on the
    /// user's next token issuance (sign-in or refresh) - live JWTs are not
    /// revoked. A no-op if the user id is unknown.
    async fn set_billing(
        &self,
        user_id: &str,
        tier: Tier,
        stripe_subscription_id: Option<String>,
    ) -> Result<()>;
    /// Delete a user and every record keyed to their global identity (workspace
    /// memberships and refresh/reset/verify tokens cascade). A no-op if the user
    /// id is unknown.
    async fn delete(&self, user_id: &str) -> Result<()>;
}

/// In-memory user store (tests, single-node dev).
#[derive(Default)]
pub struct InMemoryUserStore {
    inner: Mutex<HashMap<String, User>>,
}

#[async_trait]
impl UserStore for InMemoryUserStore {
    async fn create(&self, user: User) -> Result<()> {
        let mut map = self.inner.lock().expect("user store poisoned");
        if map.contains_key(&user.email) {
            return Err(AuthError::UserExists);
        }
        map.insert(user.email.clone(), user);
        Ok(())
    }

    async fn get_by_email(&self, email: &str) -> Result<Option<User>> {
        Ok(self
            .inner
            .lock()
            .expect("user store poisoned")
            .get(email)
            .cloned())
    }

    async fn get_by_id(&self, id: &str) -> Result<Option<User>> {
        Ok(self
            .inner
            .lock()
            .expect("user store poisoned")
            .values()
            .find(|u| u.id == id)
            .cloned())
    }

    async fn get_by_stripe_customer(&self, customer_id: &str) -> Result<Option<User>> {
        Ok(self
            .inner
            .lock()
            .expect("user store poisoned")
            .values()
            .find(|u| u.stripe_customer_id.as_deref() == Some(customer_id))
            .cloned())
    }

    async fn update_password(&self, user_id: &str, password_hash: String) -> Result<()> {
        if let Some(user) = self
            .inner
            .lock()
            .expect("user store poisoned")
            .values_mut()
            .find(|u| u.id == user_id)
        {
            user.password_hash = password_hash;
        }
        Ok(())
    }

    async fn set_email_verified(&self, user_id: &str) -> Result<()> {
        if let Some(user) = self
            .inner
            .lock()
            .expect("user store poisoned")
            .values_mut()
            .find(|u| u.id == user_id)
        {
            user.email_verified = true;
        }
        Ok(())
    }

    async fn set_stripe_customer(&self, user_id: &str, customer_id: String) -> Result<()> {
        if let Some(user) = self
            .inner
            .lock()
            .expect("user store poisoned")
            .values_mut()
            .find(|u| u.id == user_id)
        {
            user.stripe_customer_id = Some(customer_id);
        }
        Ok(())
    }

    async fn set_billing(
        &self,
        user_id: &str,
        tier: Tier,
        stripe_subscription_id: Option<String>,
    ) -> Result<()> {
        if let Some(user) = self
            .inner
            .lock()
            .expect("user store poisoned")
            .values_mut()
            .find(|u| u.id == user_id)
        {
            user.tier = tier;
            user.stripe_subscription_id = stripe_subscription_id;
        }
        Ok(())
    }

    async fn delete(&self, user_id: &str) -> Result<()> {
        self.inner
            .lock()
            .expect("user store poisoned")
            .retain(|_, u| u.id != user_id);
        Ok(())
    }
}
