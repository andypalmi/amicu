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
    /// Access tier.
    pub tier: Tier,
    /// RBAC roles.
    pub roles: Vec<String>,
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
}
