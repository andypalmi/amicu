//! Live-database integration tests for the Postgres auth stores.
//!
//! These are `#[ignore]`d so `cargo test --workspace` skips them when no
//! Postgres is available (as in CI). Run them against a live database with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://amicu:amicu@localhost:5432/amicu \
//!     cargo test -p amicu-store -- --ignored
//! ```
//!
//! Every test uses freshly-random subjects/emails (and random token hashes via
//! the amicu-auth mint helpers) so they can share one database without
//! colliding.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_auth::Tier;
use amicu_auth::refresh::{RefreshConsume, RefreshStore, mint, mint_in_family, new_family_id};
use amicu_auth::reset::{ResetConsume, ResetStore, mint as mint_reset};
use amicu_auth::user::{User, UserStore};
use amicu_auth::verify::{VerifyConsume, VerifyStore, mint as mint_verify};
use amicu_store::{
    PostgresRefreshStore, PostgresResetStore, PostgresUserStore, PostgresVerifyStore, WorkspaceId,
    connect, run_migrations,
};
use sqlx::PgPool;

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

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn user_round_trips_through_subject_and_billing() {
    let store = PostgresUserStore::new(pool().await);
    let subject = unique("usr");
    let email = format!("{}@example.test", unique("mail"));

    let user = User::new(
        subject.clone(),
        email.clone(),
        "argon2-phc-string".to_string(),
        Tier::Community,
    );
    store.create(user).await.expect("create user");

    // A duplicate email in the same workspace is rejected.
    let dup = User::new(
        unique("usr"),
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
    assert_eq!(by_email.id, subject);
    assert_eq!(by_email.email, email);
    assert_eq!(by_email.tier, Tier::Community);
    assert!(by_email.stripe_customer_id.is_none());
    // A fresh account starts unverified.
    assert!(!by_email.email_verified);

    // Marking the email verified persists and is idempotent.
    store
        .set_email_verified(&subject)
        .await
        .expect("mark email verified");
    store
        .set_email_verified(&subject)
        .await
        .expect("idempotent re-verify");
    let verified = store
        .get_by_email(&email)
        .await
        .expect("lookup by email")
        .expect("user present");
    assert!(verified.email_verified);

    let by_id = store
        .get_by_id(&subject)
        .await
        .expect("lookup by id")
        .expect("user present");
    assert_eq!(by_id.email, email);

    // Unknown lookups are Ok(None), not errors.
    assert!(store.get_by_id("nope").await.expect("lookup").is_none());

    store
        .update_password(&subject, "new-phc".to_string())
        .await
        .expect("update password");
    let cust = format!("cus_{}", unique("c"));
    store
        .set_stripe_customer(&subject, cust.clone())
        .await
        .expect("set stripe customer");
    store
        .set_billing(&subject, Tier::Pro, Some("sub_123".to_string()))
        .await
        .expect("set billing");

    let by_cust = store
        .get_by_stripe_customer(&cust)
        .await
        .expect("lookup by customer")
        .expect("user present");
    assert_eq!(by_cust.id, subject);
    assert_eq!(by_cust.password_hash, "new-phc");
    assert_eq!(by_cust.tier, Tier::Pro);
    assert_eq!(by_cust.stripe_subscription_id.as_deref(), Some("sub_123"));

    // Updates to unknown ids are no-ops, not errors.
    store
        .update_password("ghost", "x".to_string())
        .await
        .expect("no-op update");
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn refresh_rotates_once_and_detects_reuse() {
    let store = PostgresRefreshStore::new(pool().await);
    let user = unique("usr");
    let token = mint(&user, FAR_FUTURE);
    let hash = token.record.token_hash;
    let family = token.record.family_id.clone();
    store.insert(token.record).await.expect("insert refresh");

    // The stored record round-trips.
    let got = store.get(&hash).await.expect("get").expect("present");
    assert_eq!(got.user_id, user);
    assert!(!got.revoked);

    // First consume wins and reports the family to rotate into.
    match store.consume(&hash, 0).await.expect("consume") {
        RefreshConsume::Consumed { user_id, family_id } => {
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
    let poisoned = mint_in_family(&user, &family, FAR_FUTURE);
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
    let unknown = mint(&user, FAR_FUTURE);
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
    let store = PostgresRefreshStore::new(pool().await);
    let user = unique("usr");

    // Expired token: live but past expiry at consume time.
    let expired = mint(&user, 100);
    let expired_hash = expired.record.token_hash;
    store.insert(expired.record).await.expect("insert");
    assert_eq!(
        store.consume(&expired_hash, 100).await.expect("consume"),
        RefreshConsume::Expired
    );

    // revoke_all_for_user kills every live token across families.
    let a = mint(&user, FAR_FUTURE);
    let b = mint_in_family(&user, &new_family_id(), FAR_FUTURE);
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
async fn reset_is_single_use_and_rejects_expired_and_unknown() {
    let store = PostgresResetStore::new(pool().await);
    let user = unique("usr");

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
    let store = PostgresVerifyStore::new(pool().await);
    let user = unique("usr");

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

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn verify_token_is_invisible_to_another_workspace() {
    let shared = pool().await;
    // Mint into the default (single-tenant) workspace.
    let default_store = PostgresVerifyStore::new(shared.clone());
    let user = unique("usr");
    let minted = mint_verify(&user, FAR_FUTURE);
    let hash = minted.record.token_hash;
    default_store.insert(minted.record).await.expect("insert");

    // A store bound to a different workspace shares the same token hash space but
    // must not see the other tenant's row: the workspace_id predicate (and the
    // RLS backstop) scope it out, so consuming reports NotFound, not Consumed.
    let other_store = PostgresVerifyStore::with_workspace(shared, WorkspaceId::new());
    assert_eq!(
        other_store.consume(&hash, 0).await.expect("cross-tenant"),
        VerifyConsume::NotFound
    );

    // The token is still live for its own workspace.
    assert_eq!(
        default_store.consume(&hash, 0).await.expect("consume"),
        VerifyConsume::Consumed { user_id: user }
    );
}
