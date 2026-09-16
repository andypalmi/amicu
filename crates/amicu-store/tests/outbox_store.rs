//! Live-database integration tests for the transactional email outbox
//! (andypalmi/amicu-planning#141).
//!
//! `#[ignore]`d like every other live-DB test in this workspace, so
//! `cargo test --workspace` skips them when no Postgres is available (as in
//! CI). Run them against the project's docker-compose database:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://amicu_app:amicu_app@localhost:5432/amicu \
//!     cargo test -p amicu-store --test outbox_store -- --ignored
//! ```
//!
//! …or, where Docker is unavailable, against a throwaway `initdb` cluster, in
//! the style of `crates/amicu-server/tests/rls_guard_live.rs`. Port **55450** is
//! used deliberately: it is outside the 55442-55444 range that file's own recipe
//! occupies, so both clusters can be up at once.
//!
//! ```sh
//! initdb -D /private/tmp/amicu-pg-outbox --auth-local=trust --auth-host=trust -U postgres
//! pg_ctl -D /private/tmp/amicu-pg-outbox \
//!     -o "-p 55450 -k /private/tmp/amicu-pg-outbox -h ''" \
//!     -l /private/tmp/amicu-pg-outbox/log start
//! psql -h /private/tmp/amicu-pg-outbox -p 55450 -U postgres -d postgres \
//!     -c "CREATE ROLE amicu_app LOGIN;"
//! createdb -h /private/tmp/amicu-pg-outbox -p 55450 -U postgres -O amicu_app amicu
//!
//! export DATABASE_URL="postgres://%2Fprivate%2Ftmp%2Famicu-pg-outbox:55450/amicu?user=amicu_app"
//! cargo test -p amicu-store --test outbox_store -- --ignored
//!
//! pg_ctl -D /private/tmp/amicu-pg-outbox stop
//! ```
//!
//! # One test, several scenarios - on purpose
//!
//! `email_outbox` is a single GLOBAL queue with no per-test scoping column, and
//! claiming is by definition a whole-queue operation. Scenarios therefore cannot
//! run concurrently against it: two `#[tokio::test]`s would claim each other's
//! rows. They are sequenced inside [`outbox_claiming_lifecycle`] instead, which
//! is simpler - and more honest - than a cross-test lock held across `await`s.
//! Each scenario still leases away whatever is already due (see [`quiesce`]) and
//! works only with rows it enqueued itself, identified by a unique recipient.

use std::time::{SystemTime, UNIX_EPOCH};

use amicu_auth::Tier;
use amicu_auth::notify::{LoggingNotifier, Notification, NotificationKind};
use amicu_auth::reset::{ResetStore, mint as mint_reset};
use amicu_auth::user::{User, UserStore};
use amicu_store::outbox::{
    CLAIM_LEASE_SECS, ClaimedEmail, FailureOutcome, MAX_ATTEMPTS, NewOutboxEmail, claim_due,
    enqueue, mark_failed, mark_sent, plan_failure,
};
use amicu_store::{PostgresResetStore, PostgresUserStore, connect, run_migrations};
use sqlx::PgPool;
use uuid::Uuid;

/// Big enough to sweep the whole queue in one statement.
const SWEEP: i64 = 100_000;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the ignored store tests");
    let pool = connect(&url).await.expect("connect to Postgres");
    run_migrations(&pool).await.expect("apply migrations");
    pool
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs(),
    )
    .expect("fits in i64")
}

/// A collision-resistant recipient, so concurrently-running tests never claim
/// each other's rows.
fn unique_recipient(tag: &str) -> String {
    format!("{tag}-{}@outbox.test", Uuid::new_v4())
}

/// Lease away everything currently due, so the tests below see only the rows
/// they enqueue themselves. Non-destructive: the swept rows keep their state and
/// simply become due again once their lease lapses.
async fn quiesce(pool: &PgPool) {
    claim_due(pool, now(), SWEEP).await.expect("quiesce queue");
}

fn queued(recipient: &str) -> NewOutboxEmail {
    NewOutboxEmail {
        recipient: recipient.to_string(),
        kind: "verify_email",
        token: Some("opaque-token".to_string()),
    }
}

/// Row state the tests assert on. Read with the runtime `query_as` API rather
/// than the `query!` macro, so this file needs no entry in the `.sqlx` cache.
#[derive(Debug, sqlx::FromRow)]
struct Row {
    attempts: i32,
    next_attempt_at: i64,
    locked_until: Option<i64>,
    last_error: Option<String>,
    sent_at: Option<i64>,
    dead_at: Option<i64>,
    token: Option<String>,
}

async fn read(pool: &PgPool, id: Uuid) -> Row {
    sqlx::query_as::<_, Row>(
        "SELECT attempts, next_attempt_at, locked_until, last_error, sent_at, dead_at, token \
         FROM email_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("read back the queued row")
}

async fn enqueue_then_claim_then_mark_sent(pool: &PgPool) {
    quiesce(pool).await;

    let recipient = unique_recipient("sent");
    let id = enqueue(pool, &queued(&recipient), now())
        .await
        .expect("enqueue");

    let claimed = claim_due(pool, now(), SWEEP).await.expect("claim");
    let mine: Vec<&ClaimedEmail> = claimed
        .iter()
        .filter(|c| c.recipient == recipient)
        .collect();
    assert_eq!(mine.len(), 1, "exactly our row must be claimed");
    assert_eq!(mine[0].id, id);
    assert_eq!(mine[0].attempts, 1, "claiming stamps the attempt");
    assert_eq!(mine[0].token.as_deref(), Some("opaque-token"));
    assert!(
        mine[0].to_notification().is_some(),
        "a claimed row must rebuild into a Notification the mailer accepts"
    );

    // Leased: a second claim, before the lease lapses, must not hand it out
    // again - this is what stops two Cloud Run replicas double-delivering.
    let again = claim_due(pool, now(), SWEEP).await.expect("re-claim");
    assert!(
        !again.iter().any(|c| c.id == id),
        "a leased row must not be claimable again"
    );

    mark_sent(pool, id, now()).await.expect("mark sent");
    let row = read(pool, id).await;
    assert!(row.sent_at.is_some());
    assert!(row.locked_until.is_none());
    assert!(
        row.token.is_none(),
        "the raw one-shot token must be cleared once the message is delivered"
    );

    // Sent rows are permanently out of the queue, lease or no lease.
    let after_send = claim_due(pool, now() + CLAIM_LEASE_SECS + 1, SWEEP)
        .await
        .expect("claim after the lease would have lapsed");
    assert!(!after_send.iter().any(|c| c.id == id));
}

/// The core concurrency guarantee: two claimers running against the SAME due
/// rows, in overlapping transactions, get DISJOINT sets. `FOR UPDATE SKIP
/// LOCKED` makes the second claimer step over rows the first has locked rather
/// than block on (or duplicate) them - so overlapping Cloud Scheduler
/// invocations and multiple replicas can never double-deliver.
async fn concurrent_claimers_never_receive_the_same_message(pool: &PgPool) {
    quiesce(pool).await;

    let recipient = unique_recipient("race");
    let mut expected = Vec::new();
    for _ in 0..6 {
        expected.push(
            enqueue(pool, &queued(&recipient), now())
                .await
                .expect("enqueue"),
        );
    }

    // Two genuinely overlapping transactions: the first still holds its row
    // locks (it has not committed) when the second claims.
    let mut first = pool.begin().await.expect("begin first claimer");
    let mut second = pool.begin().await.expect("begin second claimer");

    let a = claim_due(&mut *first, now(), 3).await.expect("first claim");
    let b = claim_due(&mut *second, now(), 3)
        .await
        .expect("second claim");

    first.commit().await.expect("commit first claimer");
    second.commit().await.expect("commit second claimer");

    let a_ids: Vec<Uuid> = a
        .iter()
        .filter(|c| c.recipient == recipient)
        .map(|c| c.id)
        .collect();
    let b_ids: Vec<Uuid> = b
        .iter()
        .filter(|c| c.recipient == recipient)
        .map(|c| c.id)
        .collect();

    assert_eq!(
        a_ids.len(),
        3,
        "first claimer must fill its batch: {a_ids:?}"
    );
    assert_eq!(
        b_ids.len(),
        3,
        "second claimer must SKIP the locked rows and take the other three: {b_ids:?}"
    );
    for id in &a_ids {
        assert!(
            !b_ids.contains(id),
            "message {id} was handed to BOTH claimers - double delivery"
        );
    }

    let mut all: Vec<Uuid> = a_ids.into_iter().chain(b_ids).collect();
    all.sort_unstable();
    let mut want = expected.clone();
    want.sort_unstable();
    assert_eq!(
        all, want,
        "between them the claimers must cover every message"
    );

    // Every row was stamped exactly once.
    for id in expected {
        assert_eq!(read(pool, id).await.attempts, 1);
    }
}

/// A failed send backs off: the attempt is recorded, the error is kept, the
/// lease is released, and the row is not due again until the backoff elapses.
async fn a_failed_send_backs_off_and_is_not_immediately_due(pool: &PgPool) {
    quiesce(pool).await;

    let recipient = unique_recipient("retry");
    let id = enqueue(pool, &queued(&recipient), now())
        .await
        .expect("enqueue");
    let claimed = claim_due(pool, now(), SWEEP).await.expect("claim");
    let mine = claimed
        .iter()
        .find(|c| c.id == id)
        .expect("our row is claimed");

    let at = now();
    let outcome = plan_failure(mine.attempts, at);
    assert!(matches!(outcome, FailureOutcome::Retry { .. }));
    mark_failed(pool, id, outcome, at, "smtp unavailable")
        .await
        .expect("mark failed");

    let row = read(pool, id).await;
    assert_eq!(row.attempts, 1);
    assert!(row.locked_until.is_none(), "the lease must be released");
    assert_eq!(row.last_error.as_deref(), Some("smtp unavailable"));
    assert!(row.next_attempt_at > at, "the retry must be pushed out");
    assert!(row.sent_at.is_none() && row.dead_at.is_none());
    assert!(
        row.token.is_some(),
        "a retryable message must keep the token it still has to deliver"
    );

    // Not due yet...
    let early = claim_due(pool, at, SWEEP).await.expect("early claim");
    assert!(!early.iter().any(|c| c.id == id));
    // ...but due once the backoff has elapsed.
    let late = claim_due(pool, row.next_attempt_at, SWEEP)
        .await
        .expect("late claim");
    assert!(late.iter().any(|c| c.id == id));
}

/// After [`MAX_ATTEMPTS`] failures the message is dead-lettered: it stops being
/// claimed, and its raw token is cleared (it will never be delivered, so the
/// secret has no reason to persist).
async fn exhausted_messages_are_dead_lettered_and_stop_being_claimed(pool: &PgPool) {
    quiesce(pool).await;

    let recipient = unique_recipient("dead");
    let id = enqueue(pool, &queued(&recipient), now())
        .await
        .expect("enqueue");

    let mut at = now();
    loop {
        let claimed = claim_due(pool, at, SWEEP).await.expect("claim");
        let Some(mine) = claimed.iter().find(|c| c.id == id) else {
            break;
        };
        let outcome = plan_failure(mine.attempts, at);
        mark_failed(pool, id, outcome, at, "permanent failure")
            .await
            .expect("mark failed");
        // Jump straight past whatever backoff was just scheduled.
        at = read(pool, id).await.next_attempt_at.max(at) + 1;
        if matches!(outcome, FailureOutcome::DeadLetter) {
            break;
        }
    }

    let row = read(pool, id).await;
    assert_eq!(row.attempts, MAX_ATTEMPTS);
    assert!(row.dead_at.is_some(), "the message must be dead-lettered");
    assert!(row.sent_at.is_none());
    assert!(
        row.token.is_none(),
        "a dead-lettered message's token must be cleared"
    );

    let after = claim_due(pool, at + 86_400, SWEEP)
        .await
        .expect("claim long after");
    assert!(
        !after.iter().any(|c| c.id == id),
        "a dead-lettered message must never be claimed again"
    );
}

/// Every claiming scenario, in sequence. See the module docs for why they share
/// one test rather than being four.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; see this file's module docs"]
async fn outbox_claiming_lifecycle() {
    let pool = pool().await;
    enqueue_then_claim_then_mark_sent(&pool).await;
    concurrent_claimers_never_receive_the_same_message(&pool).await;
    a_failed_send_backs_off_and_is_not_immediately_due(&pool).await;
    exhausted_messages_are_dead_lettered_and_stop_being_claimed(&pool).await;
}

/// [`enqueue`] takes an executor, so a caller can make the queued email part of
/// its OWN transaction: roll that transaction back and the email is gone too -
/// which is exactly what keeps a reset token and the mail that delivers it
/// committing together or not at all.
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; see this file's module docs"]
async fn enqueue_joins_the_callers_transaction() {
    let pool = pool().await;

    let recipient = unique_recipient("rollback");
    let mut tx = pool.begin().await.expect("begin");
    let id = enqueue(&mut *tx, &queued(&recipient), now())
        .await
        .expect("enqueue inside the caller's transaction");
    tx.rollback().await.expect("rollback");

    let found: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM email_outbox WHERE id = $1")
        .bind(id)
        .fetch_optional(&pool)
        .await
        .expect("look the row up");
    assert!(
        found.is_none(),
        "an enqueue inside a rolled-back transaction must leave nothing behind"
    );
}

/// The atomicity andypalmi/amicu-planning#141 asks for, end to end: the Postgres
/// [`ResetStore::insert_and_notify`] override writes the reset-token row and the
/// queued email in ONE transaction, so the queue can never hold a link for a
/// token that does not exist (nor the reverse).
#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; see this file's module docs"]
async fn reset_token_and_its_email_commit_together() {
    let pool = pool().await;

    // A real user to hang the token's foreign key off, created through the
    // production store so the personal workspace it needs is provisioned too.
    let user_id = Uuid::new_v4().to_string();
    let email = unique_recipient("atomic");
    PostgresUserStore::new(pool.clone())
        .create(User::new(
            user_id.clone(),
            email.clone(),
            "argon2-phc-string".to_string(),
            Tier::Community,
        ))
        .await
        .expect("create the user");

    let minted = mint_reset(&user_id, 9_999_999_999);
    let token_hash = minted.record.token_hash;
    PostgresResetStore::new(pool.clone())
        .insert_and_notify(
            minted.record,
            Notification {
                recipient: email.clone(),
                kind: NotificationKind::PasswordReset {
                    reset_token: minted.token,
                },
            },
            // Ignored by the Postgres override - the outbox IS the delivery.
            &LoggingNotifier,
        )
        .await
        .expect("insert the token and queue its email");

    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM reset_tokens WHERE token_hash = $1")
        .bind(token_hash.as_slice())
        .fetch_one(&pool)
        .await
        .expect("count reset tokens");
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM email_outbox WHERE recipient = $1 AND kind = 'password_reset'",
    )
    .bind(&email)
    .fetch_one(&pool)
    .await
    .expect("count queued emails");

    assert_eq!(tokens, 1, "the reset token must be persisted");
    assert_eq!(
        queued, 1,
        "its delivery must be queued in the same transaction"
    );
}
