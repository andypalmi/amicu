//! Live-database integration tests for the Postgres auth stores (global
//! identity + the session token tables).
//!
//! These are `#[ignore]`d so `cargo test --workspace` skips them when no
//! Postgres is available (as in CI). Run them against a live database with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://amicu_app:amicu_app@localhost:5432/amicu \
//!     cargo test -p amicu-store -- --ignored
//! ```
//!
//! Connect as the non-superuser, non-BYPASSRLS `amicu_app` role (see
//! andypalmi/amicu-planning#114) to also exercise the row-level-security
//! backstop on the workspace-scoped tables; the always-on query predicates make
//! these tests pass under any role, but only `amicu_app` engages RLS too.
//!
//! In the new model, identity is GLOBAL and every auth token table has a real
//! `user_id` foreign key into `users`, so each test creates a user first (via
//! the identity store) and mints tokens for that user's UUID id. Tests use
//! freshly-random emails so they can share one database without colliding.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_auth::refresh::{RefreshConsume, RefreshStore, mint, mint_in_family, new_family_id};
use amicu_auth::reset::{ResetConsume, ResetStore, mint as mint_reset};
use amicu_auth::user::{User, UserStore};
use amicu_auth::verify::{VerifyConsume, VerifyStore, mint as mint_verify};
use amicu_auth::{Tier, WorkspaceId};
use amicu_store::{
    PostgresRefreshStore, PostgresResetStore, PostgresUserStore, PostgresVerifyStore, connect,
    run_migrations,
};
use sqlx::PgPool;
use uuid::Uuid;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A collision-resistant unique suffix for per-test identifiers.
fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos}_{n}")
}

/// A far-future expiry (seconds since the epoch).
const FAR_FUTURE: i64 = 9_999_999_999;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the ignored store tests");
    let pool = connect(&url).await.expect("connect to Postgres");
    run_migrations(&pool).await.expect("apply migrations");
    pool
}

/// Create a fresh global user (with their personal Owner workspace) and return
/// their UUID id as text - the `sub` the token tables foreign-key to.
async fn seed_user(store: &PostgresUserStore, tier: Tier) -> (String, String, WorkspaceId) {
    let id = Uuid::new_v4().to_string();
    let email = format!("{}@example.test", unique("mail"));
    let user = User::new(
        id.clone(),
        email.clone(),
        "argon2-phc-string".to_string(),
        tier,
    );
    // The server-generated personal workspace the create provisions; refresh
    // tokens FK to it, so a mint needs this id.
    let workspace_id = user.personal_workspace_id;
    store.create(user).await.expect("create user");
    (id, email, workspace_id)
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn user_round_trips_identity_and_workspace_billing() {
    let store = PostgresUserStore::new(pool().await);
    let (id, email, _) = seed_user(&store, Tier::Community).await;

    // A duplicate email is rejected globally (not per workspace).
    let dup = User::new(
        Uuid::new_v4().to_string(),
        email.clone(),
        "other".to_string(),
        Tier::Community,
    );
    assert!(matches!(
        store.create(dup).await,
        Err(amicu_auth::AuthError::UserExists)
    ));

    let by_email = store
        .get_by_email(&email)
        .await
        .expect("lookup by email")
        .expect("user present");
    assert_eq!(by_email.id, id);
    assert_eq!(by_email.email, email);
    // Tier is resolved from the user's personal workspace.
    assert_eq!(by_email.tier, Tier::Community);
    assert!(by_email.stripe_customer_id.is_none());
    // A fresh account starts unverified.
    assert!(!by_email.email_verified);

    // Marking the email verified persists and is idempotent.
    store.set_email_verified(&id).await.expect("mark verified");
    store.set_email_verified(&id).await.expect("idempotent");
    let verified = store
        .get_by_id(&id)
        .await
        .expect("lookup by id")
        .expect("user present");
    assert!(verified.email_verified);

    // Unknown / non-UUID lookups are Ok(None), not errors.
    assert!(store.get_by_id("nope").await.expect("lookup").is_none());
    assert!(
        store
            .get_by_id(&Uuid::new_v4().to_string())
            .await
            .expect("lookup")
            .is_none()
    );

    store
        .update_password(&id, "new-phc".to_string())
        .await
        .expect("update password");

    // Billing lands on the personal workspace and round-trips through the user.
    let cust = format!("cus_{}", unique("c"));
    store
        .set_stripe_customer(&id, cust.clone())
        .await
        .expect("set stripe customer");
    store
        .set_billing(&id, Tier::Pro, Some("sub_123".to_string()))
        .await
        .expect("set billing");

    let by_cust = store
        .get_by_stripe_customer(&cust)
        .await
        .expect("lookup by customer")
        .expect("user present");
    assert_eq!(by_cust.id, id);
    assert_eq!(by_cust.password_hash, "new-phc");
    assert_eq!(by_cust.tier, Tier::Pro);
    assert_eq!(by_cust.stripe_subscription_id.as_deref(), Some("sub_123"));

    // Updates to unknown ids are no-ops, not errors.
    store
        .update_password(&Uuid::new_v4().to_string(), "x".to_string())
        .await
        .expect("no-op update");

    // Deletion removes the user; a later lookup misses.
    store.delete(&id).await.expect("delete user");
    assert!(store.get_by_id(&id).await.expect("lookup").is_none());
    assert!(store.get_by_email(&email).await.expect("lookup").is_none());
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn refresh_rotates_once_and_detects_reuse() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let store = PostgresRefreshStore::new(shared);
    let (user, _, ws) = seed_user(&users, Tier::Community).await;

    let token = mint(&user, ws, FAR_FUTURE);
    let hash = token.record.token_hash;
    let family = token.record.family_id.clone();
    store.insert(token.record).await.expect("insert refresh");

    // The stored record round-trips.
    let got = store.get(&hash).await.expect("get").expect("present");
    assert_eq!(got.user_id, user);
    assert!(!got.revoked);

    // First consume wins and reports the family to rotate into.
    match store.consume(&hash, 0).await.expect("consume") {
        RefreshConsume::Consumed {
            user_id, family_id, ..
        } => {
            assert_eq!(user_id, user);
            assert_eq!(family_id, family);
        }
        other => panic!("expected Consumed, got {other:?}"),
    }

    // Re-presenting the now-revoked token is a reuse signal.
    assert_eq!(
        store.consume(&hash, 0).await.expect("re-consume"),
        RefreshConsume::Reuse {
            family_id: family.clone()
        }
    );

    // Compromise the family durably; every member dies and the flag persists.
    assert!(!store.is_family_compromised(&family).await.expect("flag"));
    store.compromise_family(&family).await.expect("compromise");
    assert!(store.is_family_compromised(&family).await.expect("flag"));

    // A fresh live token minted into the compromised family cannot be consumed.
    let poisoned = mint_in_family(&user, ws, &family, FAR_FUTURE);
    let poisoned_hash = poisoned.record.token_hash;
    store.insert(poisoned.record).await.expect("insert");
    store
        .compromise_family(&family)
        .await
        .expect("re-compromise");
    assert_eq!(
        store.consume(&poisoned_hash, 0).await.expect("consume"),
        RefreshConsume::Reuse {
            family_id: family.clone()
        }
    );

    // Unknown token is NotFound.
    let unknown = mint(&user, ws, FAR_FUTURE);
    assert_eq!(
        store
            .consume(&unknown.record.token_hash, 0)
            .await
            .expect("consume unknown"),
        RefreshConsume::NotFound
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn refresh_expired_is_rejected_and_revoke_all_works() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let store = PostgresRefreshStore::new(shared);
    let (user, _, ws) = seed_user(&users, Tier::Community).await;

    // Expired token: live but past expiry at consume time.
    let expired = mint(&user, ws, 100);
    let expired_hash = expired.record.token_hash;
    store.insert(expired.record).await.expect("insert");
    assert_eq!(
        store.consume(&expired_hash, 100).await.expect("consume"),
        RefreshConsume::Expired
    );

    // revoke_all_for_user kills every live token across families.
    let a = mint(&user, ws, FAR_FUTURE);
    let b = mint_in_family(&user, ws, &new_family_id(), FAR_FUTURE);
    let (ah, bh) = (a.record.token_hash, b.record.token_hash);
    store.insert(a.record).await.expect("insert a");
    store.insert(b.record).await.expect("insert b");
    store
        .revoke_all_for_user(&user)
        .await
        .expect("revoke all for user");
    assert!(matches!(
        store.consume(&ah, 0).await.expect("consume a"),
        RefreshConsume::Reuse { .. }
    ));
    assert!(matches!(
        store.consume(&bh, 0).await.expect("consume b"),
        RefreshConsume::Reuse { .. }
    ));
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn deleting_a_user_cascades_their_tokens() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let refresh = PostgresRefreshStore::new(shared);
    let (user, _, ws) = seed_user(&users, Tier::Community).await;

    let token = mint(&user, ws, FAR_FUTURE);
    let hash = token.record.token_hash;
    refresh.insert(token.record).await.expect("insert refresh");
    assert!(refresh.get(&hash).await.expect("get").is_some());

    // Deleting the user must cascade the refresh token (real FK, ON DELETE
    // CASCADE) - the soft-FK note in andypalmi/amicu-planning#114 is resolved.
    users.delete(&user).await.expect("delete user");
    assert!(refresh.get(&hash).await.expect("get").is_none());
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn reset_is_single_use_and_rejects_expired_and_unknown() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let store = PostgresResetStore::new(shared);
    let (user, _, _) = seed_user(&users, Tier::Community).await;

    let minted = mint_reset(&user, FAR_FUTURE);
    let hash = minted.record.token_hash;
    store.insert(minted.record).await.expect("insert reset");

    // First consume wins.
    assert_eq!(
        store.consume(&hash, 0).await.expect("consume"),
        ResetConsume::Consumed {
            user_id: user.clone()
        }
    );
    // Replay is rejected as reuse.
    assert_eq!(
        store.consume(&hash, 0).await.expect("re-consume"),
        ResetConsume::Reuse
    );

    // Expired token is rejected as Expired, not Consumed.
    let expired = mint_reset(&user, 100);
    let expired_hash = expired.record.token_hash;
    store.insert(expired.record).await.expect("insert expired");
    assert_eq!(
        store.consume(&expired_hash, 100).await.expect("consume"),
        ResetConsume::Expired
    );

    // Never-minted token is NotFound.
    let unknown = mint_reset(&user, FAR_FUTURE);
    assert_eq!(
        store
            .consume(&unknown.record.token_hash, 0)
            .await
            .expect("consume unknown"),
        ResetConsume::NotFound
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn verify_is_single_use_and_rejects_expired_and_unknown() {
    let shared = pool().await;
    let users = PostgresUserStore::new(shared.clone());
    let store = PostgresVerifyStore::new(shared);
    let (user, _, _) = seed_user(&users, Tier::Community).await;

    let minted = mint_verify(&user, FAR_FUTURE);
    let hash = minted.record.token_hash;
    store.insert(minted.record).await.expect("insert verify");

    // First consume wins and reports the user to mark verified.
    assert_eq!(
        store.consume(&hash, 0).await.expect("consume"),
        VerifyConsume::Consumed {
            user_id: user.clone()
        }
    );
    // Replay is rejected as reuse (the atomic compare-and-mark won only once).
    assert_eq!(
        store.consume(&hash, 0).await.expect("re-consume"),
        VerifyConsume::Reuse
    );

    // Expired token is rejected as Expired, not Consumed.
    let expired = mint_verify(&user, 100);
    let expired_hash = expired.record.token_hash;
    store.insert(expired.record).await.expect("insert expired");
    assert_eq!(
        store.consume(&expired_hash, 100).await.expect("consume"),
        VerifyConsume::Expired
    );

    // Never-minted token is NotFound.
    let unknown = mint_verify(&user, FAR_FUTURE);
    assert_eq!(
        store
            .consume(&unknown.record.token_hash, 0)
            .await
            .expect("consume unknown"),
        VerifyConsume::NotFound
    );
}
