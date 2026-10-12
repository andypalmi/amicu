//! End-to-end HTTP tests over the internal outbox-drain router
//! (andypalmi/amicu-planning#141), in-process via `oneshot`.
//!
//! The Google-OIDC claim checking itself (issuer, signature, `aud`, allowlisted
//! `email`) is unit-tested against a locally-signed RS256 token and an injected
//! certs source in `amicu-auth`'s `google` module - no network, there or here.
//! What this file pins is the HTTP contract around it: which rejection maps to
//! which status, and that the drain actually delivers, retries, dead-letters,
//! and respects its batch and time budgets.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use amicu_api::{DrainAuth, InlineSend, OutboxApiState, outbox_router};
use amicu_auth::error::AuthError;
use amicu_auth::notify::{Notification, Notifier};
use amicu_store::StoreError;
use amicu_store::outbox::{ClaimedEmail, FailureOutcome, MAX_ATTEMPTS, OutboxStore};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

const DRAIN: &str = "/internal/outbox/drain";
const TOKEN: &str = "a-static-internal-token";

/// An in-memory [`OutboxStore`]: a queue of pending messages plus a record of
/// every outcome the drain reported.
#[derive(Default)]
struct FakeStore {
    pending: Mutex<Vec<ClaimedEmail>>,
    sent: Mutex<Vec<Uuid>>,
    failed: Mutex<Vec<(Uuid, FailureOutcome, String)>>,
}

impl FakeStore {
    fn with_messages(messages: Vec<ClaimedEmail>) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(messages),
            ..Self::default()
        })
    }

    fn sent_count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }

    fn failures(&self) -> Vec<(Uuid, FailureOutcome, String)> {
        self.failed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl OutboxStore for FakeStore {
    async fn claim_due(&self, _now: i64, limit: i64) -> Result<Vec<ClaimedEmail>, StoreError> {
        let mut pending = self.pending.lock().unwrap();
        let take = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(pending.len());
        Ok(pending.drain(..take).collect())
    }

    async fn mark_sent(&self, id: Uuid, _now: i64) -> Result<(), StoreError> {
        self.sent.lock().unwrap().push(id);
        Ok(())
    }

    async fn mark_failed(
        &self,
        id: Uuid,
        outcome: FailureOutcome,
        _now: i64,
        error: &str,
    ) -> Result<(), StoreError> {
        self.failed
            .lock()
            .unwrap()
            .push((id, outcome, error.to_string()));
        Ok(())
    }
}

/// A mailer that either delivers everything or fails everything, and that can
/// be told to take a while (to exercise the per-request time budget).
struct FakeMailer {
    fail: AtomicBool,
    delay: Duration,
    delivered: Mutex<Vec<String>>,
}

impl FakeMailer {
    fn delivering() -> Arc<Self> {
        Arc::new(Self {
            fail: AtomicBool::new(false),
            delay: Duration::ZERO,
            delivered: Mutex::new(Vec::new()),
        })
    }

    fn failing() -> Arc<Self> {
        let mailer = Self::delivering();
        mailer.fail.store(true, Ordering::SeqCst);
        mailer
    }

    fn slow(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            fail: AtomicBool::new(false),
            delay,
            delivered: Mutex::new(Vec::new()),
        })
    }

    fn delivered(&self) -> Vec<String> {
        self.delivered.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Notifier for FakeMailer {
    async fn notify(&self, message: Notification) -> Result<(), AuthError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(AuthError::Backend);
        }
        self.delivered.lock().unwrap().push(message.recipient);
        Ok(())
    }
}

/// A claimable message on its `attempts`-th attempt.
fn message(recipient: &str, attempts: i32) -> ClaimedEmail {
    ClaimedEmail {
        id: Uuid::new_v4(),
        recipient: recipient.to_string(),
        kind: "verify_email".to_string(),
        token: Some("opaque-token".to_string()),
        attempts,
    }
}

fn app(store: Arc<dyn OutboxStore>, mailer: Arc<dyn Notifier>, auth: DrainAuth) -> Router {
    outbox_router(OutboxApiState::new(store, mailer, auth))
}

fn configured(store: Arc<dyn OutboxStore>, mailer: Arc<dyn Notifier>) -> Router {
    app(
        store,
        mailer,
        DrainAuth::disabled().with_static_token(TOKEN),
    )
}

async fn post(app: &Router, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("POST").uri(DRAIN);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).expect("build request"))
        .await
        .expect("route the request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON body")
    };
    (status, body)
}

/// With no authentication mechanism configured the endpoint is switched OFF: it
/// must never become an unauthenticated way to make a deployment send mail, or
/// to probe how much mail it has queued.
#[tokio::test]
async fn unconfigured_drain_is_503_for_everyone() {
    let store = FakeStore::with_messages(vec![message("a@example.test", 1)]);
    let mailer = FakeMailer::delivering();
    let app = app(store.clone(), mailer.clone(), DrainAuth::disabled());

    for bearer in [None, Some(TOKEN), Some("anything-at-all")] {
        let (status, body) = post(&app, bearer).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "bearer={bearer:?}");
        assert_eq!(body["error"], "outbox drain is not configured");
    }
    assert_eq!(
        store.sent_count(),
        0,
        "nothing may be drained while disabled"
    );
    assert_eq!(mailer.delivered().len(), 0);
}

#[tokio::test]
async fn missing_bearer_token_is_401() {
    let store = FakeStore::with_messages(vec![message("a@example.test", 1)]);
    let mailer = FakeMailer::delivering();
    let (status, body) = post(&configured(store.clone(), mailer.clone()), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");
    assert_eq!(store.sent_count(), 0);
}

#[tokio::test]
async fn wrong_static_token_is_401() {
    let store = FakeStore::with_messages(vec![message("a@example.test", 1)]);
    let mailer = FakeMailer::delivering();
    let app = configured(store.clone(), mailer);
    for wrong in ["not-the-token", "a-static-internal-toke", "", " "] {
        let (status, _) = post(&app, Some(wrong)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "bearer={wrong:?}");
    }
    assert_eq!(store.sent_count(), 0);
}

/// A non-`Bearer` scheme is not a credential this endpoint understands.
#[tokio::test]
async fn non_bearer_authorization_is_401() {
    let store = FakeStore::with_messages(vec![]);
    let app = configured(store, FakeMailer::delivering());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(DRAIN)
                .header("authorization", format!("Basic {TOKEN}"))
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("route the request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// The static-token path: authenticated, and the queue actually drains.
#[tokio::test]
async fn static_token_drains_the_queue() {
    let store = FakeStore::with_messages(vec![
        message("one@example.test", 1),
        message("two@example.test", 1),
    ]);
    let mailer = FakeMailer::delivering();
    let (status, body) = post(&configured(store.clone(), mailer.clone()), Some(TOKEN)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["claimed"], 2);
    assert_eq!(body["sent"], 2);
    assert_eq!(body["failed"], 0);
    assert_eq!(body["dead_lettered"], 0);
    assert_eq!(body["budget_exhausted"], false);
    assert_eq!(store.sent_count(), 2);
    assert_eq!(
        mailer.delivered(),
        vec![
            "one@example.test".to_string(),
            "two@example.test".to_string()
        ]
    );
}

/// An empty queue is a successful, zero-count drain - not an error. Cloud
/// Scheduler calls this every minute, and most minutes there is nothing to do.
#[tokio::test]
async fn an_empty_queue_drains_to_zero_counts() {
    let store = FakeStore::with_messages(vec![]);
    let (status, body) = post(&configured(store, FakeMailer::delivering()), Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["claimed"], 0);
    assert_eq!(body["sent"], 0);
    assert_eq!(body["budget_exhausted"], false);
}

/// A transport failure is recorded on the message and reported in the counts -
/// it is never a `5xx` on the drain itself, which would make Cloud Scheduler
/// retry the whole batch.
#[tokio::test]
async fn a_failed_send_is_counted_and_scheduled_for_retry() {
    let store = FakeStore::with_messages(vec![message("a@example.test", 1)]);
    let (status, body) = post(
        &configured(store.clone(), FakeMailer::failing()),
        Some(TOKEN),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["claimed"], 1);
    assert_eq!(body["sent"], 0);
    assert_eq!(body["failed"], 1);
    assert_eq!(body["dead_lettered"], 0);

    let failures = store.failures();
    assert_eq!(failures.len(), 1);
    assert!(matches!(failures[0].1, FailureOutcome::Retry { .. }));
    // The persisted error is the mailer's coarse, material-free Display text.
    assert_eq!(failures[0].2, AuthError::Backend.to_string());
    assert!(!failures[0].2.contains("opaque-token"));
}

/// A message that has already used its last attempt is dead-lettered instead of
/// retried forever.
#[tokio::test]
async fn an_exhausted_message_is_dead_lettered() {
    let store = FakeStore::with_messages(vec![message("a@example.test", MAX_ATTEMPTS)]);
    let (status, body) = post(
        &configured(store.clone(), FakeMailer::failing()),
        Some(TOKEN),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["failed"], 0);
    assert_eq!(body["dead_lettered"], 1);
    assert!(matches!(
        store.failures().as_slice(),
        [(_, FailureOutcome::DeadLetter, _)]
    ));
}

/// A stored payload this build cannot render can never succeed, so it is
/// dead-lettered immediately rather than burning eight attempts.
#[tokio::test]
async fn an_unrenderable_payload_is_dead_lettered_without_being_sent() {
    let mut broken = message("a@example.test", 1);
    broken.kind = "a_kind_that_does_not_exist".to_string();
    let store = FakeStore::with_messages(vec![broken]);
    let mailer = FakeMailer::delivering();
    let (status, body) = post(&configured(store.clone(), mailer.clone()), Some(TOKEN)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["claimed"], 1);
    assert_eq!(body["sent"], 0);
    assert_eq!(body["dead_lettered"], 1);
    assert!(
        mailer.delivered().is_empty(),
        "an unrenderable message must never reach the mailer"
    );
    assert!(matches!(
        store.failures().as_slice(),
        [(_, FailureOutcome::DeadLetter, _)]
    ));
}

/// The per-request time budget: the drain stops early and returns PARTIAL
/// counts rather than running past Cloud Scheduler's attempt deadline. The
/// leftovers are simply claimed by the next invocation.
#[tokio::test]
async fn the_time_budget_stops_the_drain_and_reports_partial_counts() {
    let messages: Vec<ClaimedEmail> = (0..10)
        .map(|i| message(&format!("slow-{i}@example.test"), 1))
        .collect();
    let store = FakeStore::with_messages(messages);
    let mailer = FakeMailer::slow(Duration::from_millis(40));
    let app = outbox_router(
        OutboxApiState::new(
            store.clone(),
            mailer.clone(),
            DrainAuth::disabled().with_static_token(TOKEN),
        )
        .with_budget(Duration::from_millis(60)),
    );

    let (status, body) = post(&app, Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["budget_exhausted"], true,
        "the drain must report that it ran out of time: {body}"
    );
    let sent = body["sent"].as_u64().expect("sent count");
    assert!(
        (1..10).contains(&sent),
        "the drain must deliver some but not all of the batch, got {sent}"
    );
    // Whatever it did NOT send is still queued for the next invocation.
    assert!(store.sent_count() < 10);
}

/// The batch cap bounds how much one request claims, so a huge backlog is
/// drained across several invocations instead of one enormous request.
#[tokio::test]
async fn the_batch_size_bounds_one_claim() {
    let messages: Vec<ClaimedEmail> = (0..7)
        .map(|i| message(&format!("batched-{i}@example.test"), 1))
        .collect();
    let store = FakeStore::with_messages(messages);
    let app = outbox_router(
        OutboxApiState::new(
            store.clone(),
            FakeMailer::delivering(),
            DrainAuth::disabled().with_static_token(TOKEN),
        )
        .with_batch_size(3),
    );

    // Batches of 3 keep being claimed until a short batch says the queue is
    // empty, so one request still drains everything - just in bounded chunks.
    let (status, body) = post(&app, Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["claimed"], 7);
    assert_eq!(body["sent"], 7);
    assert_eq!(store.sent_count(), 7);
}

// ---- inline send (post-enqueue, best-effort) --------------------------------

fn inline(store: Arc<dyn OutboxStore>, mailer: Arc<dyn Notifier>, ms: u64) -> InlineSend {
    InlineSend::new(store, mailer).with_budget(Duration::from_millis(ms))
}

/// The inline pass delivers the queued message and marks it sent.
#[tokio::test]
async fn inline_send_delivers_and_marks_sent() {
    let store = FakeStore::with_messages(vec![message("a@example.test", 1)]);
    let mailer = FakeMailer::delivering();
    inline(store.clone(), mailer.clone(), 100)
        .run_padded()
        .await;

    assert_eq!(mailer.delivered(), vec!["a@example.test".to_string()]);
    assert_eq!(store.sent_count(), 1);
    assert_eq!(store.failures().len(), 0);
}

/// A failing mailer is swallowed (no panic, nothing returned) and recorded with
/// the SAME failure accounting as the scheduled drain (retry + backoff).
#[tokio::test]
async fn inline_failure_is_recorded_for_the_scheduler_retry() {
    let store = FakeStore::with_messages(vec![message("a@example.test", 1)]);
    inline(store.clone(), FakeMailer::failing(), 100)
        .run_padded()
        .await;

    assert_eq!(store.sent_count(), 0);
    let failures = store.failures();
    assert_eq!(failures.len(), 1);
    assert!(matches!(failures[0].1, FailureOutcome::Retry { .. }));
    assert_eq!(failures[0].2, AuthError::Backend.to_string());
}

/// After an inline success the scheduled drain finds nothing to resend.
#[tokio::test]
async fn scheduler_drain_after_inline_success_does_not_resend() {
    let store = FakeStore::with_messages(vec![message("a@example.test", 1)]);
    let mailer = FakeMailer::delivering();
    inline(store.clone(), mailer.clone(), 100)
        .run_padded()
        .await;

    let (status, body) = post(&configured(store.clone(), mailer.clone()), Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["claimed"], 0);
    assert_eq!(mailer.delivered().len(), 1);
    assert_eq!(store.sent_count(), 1);
}

/// Anti-enumeration: the phase lasts the full budget whether or not there was
/// anything to send, and a slow mailer is cut off at the budget rather than
/// stretching the request.
#[tokio::test]
async fn inline_phase_takes_the_fixed_budget_regardless_of_work() {
    let budget = Duration::from_millis(200);
    let empty = FakeStore::with_messages(vec![]);
    let t = std::time::Instant::now();
    inline(empty, FakeMailer::delivering(), 200)
        .run_padded()
        .await;
    let idle = t.elapsed();

    let busy = FakeStore::with_messages(vec![message("a@example.test", 1)]);
    let t = std::time::Instant::now();
    inline(busy, FakeMailer::delivering(), 200)
        .run_padded()
        .await;
    let sent = t.elapsed();

    let stuck = FakeStore::with_messages(vec![message("a@example.test", 1)]);
    let t = std::time::Instant::now();
    inline(stuck, FakeMailer::slow(Duration::from_secs(30)), 200)
        .run_padded()
        .await;
    let timed_out = t.elapsed();

    for d in [idle, sent, timed_out] {
        assert!(d >= budget, "{d:?} shorter than budget");
        assert!(
            d < budget + Duration::from_millis(150),
            "{d:?} overran budget"
        );
    }
}
