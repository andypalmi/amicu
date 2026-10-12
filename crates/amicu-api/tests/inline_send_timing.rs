//! The inline post-enqueue send must not be a timing oracle: `/signup`,
//! `/recover` and `/verify/resend` each spend at least the inline budget
//! whether or not the email belongs to an account (and whether or not any mail
//! was queued).

use std::sync::Arc;
use std::time::{Duration, Instant};

use amicu_api::{AuthApiState, InMemoryRateLimiter, InlineSend, auth_router};
use amicu_auth::error::AuthError;
use amicu_auth::issuer::{IssuerConfig, TokenIssuer};
use amicu_auth::key::SigningKey;
use amicu_auth::keyring::Keyring;
use amicu_auth::notify::{LoggingNotifier, Notification, Notifier};
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::service::AuthService;
use amicu_auth::user::InMemoryUserStore;
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_store::StoreError;
use amicu_store::outbox::{ClaimedEmail, FailureOutcome, OutboxStore};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use uuid::Uuid;

const BUDGET: Duration = Duration::from_millis(100);

struct EmptyStore;

#[async_trait::async_trait]
impl OutboxStore for EmptyStore {
    async fn claim_due(&self, _: i64, _: i64) -> Result<Vec<ClaimedEmail>, StoreError> {
        Ok(Vec::new())
    }
    async fn mark_sent(&self, _: Uuid, _: i64) -> Result<(), StoreError> {
        Ok(())
    }
    async fn mark_failed(
        &self,
        _: Uuid,
        _: FailureOutcome,
        _: i64,
        _: &str,
    ) -> Result<(), StoreError> {
        Ok(())
    }
}

struct OkMailer;

#[async_trait::async_trait]
impl Notifier for OkMailer {
    async fn notify(&self, _: Notification) -> Result<(), AuthError> {
        Ok(())
    }
}

fn app() -> Router {
    let service = Arc::new(AuthService::new(
        Keyring::new(SigningKey::generate().unwrap()),
        TokenIssuer::new(IssuerConfig::new("https://auth.amicu.dev", "amicu-api")),
        Arc::new(InMemoryUserStore::default()),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        "https://auth.amicu.dev",
    ));
    let state = AuthApiState::new(
        service,
        Arc::new(InMemoryRateLimiter::new(1000, Duration::from_secs(60))),
    )
    .with_inline_send(
        InlineSend::new(Arc::new(EmptyStore), Arc::new(OkMailer)).with_budget(BUDGET),
    );
    auth_router(state)
}

async fn timed(app: &Router, path: &str, body: serde_json::Value) -> (StatusCode, Duration) {
    let req = Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let t = Instant::now();
    let res = app.clone().oneshot(req).await.unwrap();
    (res.status(), t.elapsed())
}

#[tokio::test]
async fn enqueueing_endpoints_take_at_least_the_budget_for_unknown_emails() {
    let on = app();
    for (path, body) in [
        (
            "/signup",
            json!({"email": "unknown@example.com", "password": "correct-horse-battery-9X!"}),
        ),
        ("/recover", json!({"email": "nobody@example.com"})),
        ("/verify/resend", json!({"email": "nobody@example.com"})),
    ] {
        let (status, with) = timed(&on, path, body).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{path}");
        assert!(with >= BUDGET, "{path} took {with:?}, under the budget");
    }
}
