//! Postgres-backed [`amicu_auth::throttle::KnownNetworks`]
//! (andypalmi/amicu-planning#194), shared by every replica.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use amicu_auth::error::{AuthError, Result};
use amicu_auth::throttle::{KNOWN_NETWORK_TTL_SECS, KnownNetworks};
use async_trait::async_trait;

use crate::PgPool;

const CLEANUP_EVERY: u64 = 256;

/// Known-network store over the one Postgres pool.
#[derive(Debug, Clone)]
pub struct PostgresKnownNetworks {
    pool: PgPool,
    calls: Arc<AtomicU64>,
}

impl PostgresKnownNetworks {
    /// A store over `pool`.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            calls: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn cleanup(&self) {
        let ttl = i64::try_from(KNOWN_NETWORK_TTL_SECS).unwrap_or(i64::MAX);
        let _ = sqlx::query!(
            "DELETE FROM known_networks WHERE seen_at <= extract(epoch FROM now())::bigint - $1",
            ttl,
        )
        .execute(&self.pool)
        .await;
    }
}

#[async_trait]
impl KnownNetworks for PostgresKnownNetworks {
    async fn is_known(&self, key: &str) -> Result<bool> {
        let ttl = i64::try_from(KNOWN_NETWORK_TTL_SECS).unwrap_or(i64::MAX);
        let row = sqlx::query_scalar!(
            "SELECT seen_at FROM known_networks \
             WHERE key = $1 AND seen_at > extract(epoch FROM now())::bigint - $2",
            key,
            ttl,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AuthError::Backend)?;
        Ok(row.is_some())
    }

    async fn record(&self, key: &str) -> Result<()> {
        sqlx::query!(
            "INSERT INTO known_networks (key, seen_at) \
             VALUES ($1, extract(epoch FROM now())::bigint) \
             ON CONFLICT (key) DO UPDATE SET seen_at = EXCLUDED.seen_at",
            key,
        )
        .execute(&self.pool)
        .await
        .map_err(|_| AuthError::Backend)?;
        if self
            .calls
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(CLEANUP_EVERY)
        {
            let this = self.clone();
            tokio::spawn(async move { this.cleanup().await });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
    async fn records_and_recognizes_a_network() {
        let url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for the ignored tests");
        let pool = crate::connect(&url).await.expect("connect");
        crate::run_migrations(&pool).await.expect("migrate");
        let store = PostgresKnownNetworks::new(pool);
        let key = format!("test|known|{}", uuid::Uuid::new_v4());
        assert!(!store.is_known(&key).await.unwrap());
        store.record(&key).await.unwrap();
        store.record(&key).await.unwrap();
        assert!(store.is_known(&key).await.unwrap());
    }
}
