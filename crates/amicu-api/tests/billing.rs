//! Integration tests for the Stripe-backed billing endpoints.
//!
//! These exercise everything that is verifiable **without** a live Stripe
//! account: the auth gate, the unconfigured-Stripe `503` path, webhook signature
//! verification (good and bad), and the tier persistence a verified subscription
//! event drives. The actual outbound calls to Stripe (checkout/portal session
//! creation) require real keys and are the documented activation gap.

// `Duration::from_mins` is unstable on our MSRV 1.97; keep `from_secs`.
#![allow(clippy::duration_suboptimal_units)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use amicu_api::{
    AuthApiState, BillingApiState, ConfigApiState, EngineApiState, InMemoryOrgStore,
    InMemoryRateLimiter, OrgApiState, SessionApiState, StripeConfig, api_router,
};
use amicu_auth::notify::LoggingNotifier;
use amicu_auth::refresh::InMemoryRefreshStore;
use amicu_auth::reset::InMemoryResetStore;
use amicu_auth::user::{InMemoryUserStore, User, UserStore};
use amicu_auth::verifier::StaticJwksProvider;
use amicu_auth::verify::InMemoryVerifyStore;
use amicu_auth::{
    AuthService, IssuerConfig, Keyring, SigningKey, Tier, TokenIssuer, Verifier, VerifierConfig,
};
use amicu_auth::{Role, WorkspaceId};
use amicu_core::secret::SecretValue;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;

const ISS: &str = "https://issuer.test";
const AUD: &str = "amicu-api";
const USER_ID: &str = "usr_billing_test";
const CUSTOMER_ID: &str = "cus_test123";
const WEBHOOK_SECRET: &str = "whsec_test_secret";
const PRICE_PRO: &str = "price_pro_123";

type HmacSha256 = Hmac<Sha256>;

/// A test harness: the assembled app, the shared user store (so tests can seed
/// users and assert persisted tier changes), and a freshly minted token.
struct Harness {
    app: Router,
    users: Arc<InMemoryUserStore>,
    token: String,
}

/// Build the app. When `configure_stripe` is true the billing state carries a
/// live [`StripeConfig`] (known webhook secret, one Pro price) - enough to
/// exercise webhook verification; its API base is unused by these tests. When
/// false, billing is unconfigured and endpoints must answer `503`.
async fn harness(tier: Tier, configure_stripe: bool) -> Harness {
    let key = SigningKey::generate().expect("generate key");
    let keyring = Keyring::new(key);
    let issuer = TokenIssuer::new(IssuerConfig::new(ISS, AUD));

    let jwks = keyring.jwks();
    let token = issuer
        .issue_access(
            keyring.active(),
            USER_ID,
            WorkspaceId::new(),
            Role::Owner,
            tier,
            Vec::new(),
        )
        .expect("mint token");

    let users = Arc::new(InMemoryUserStore::default());
    // Seed the token's subject as a real user so `/status`, `/checkout`, and the
    // webhook can resolve it.
    users
        .create(User::new(
            USER_ID.to_owned(),
            "billing@example.com".to_owned(),
            "argon2-dummy-hash".to_owned(),
            tier,
        ))
        .await
        .expect("seed user");

    let service = Arc::new(AuthService::new(
        keyring,
        issuer,
        users.clone(),
        Arc::new(InMemoryRefreshStore::default()),
        Arc::new(InMemoryResetStore::default()),
        Arc::new(InMemoryVerifyStore::default()),
        Arc::new(LoggingNotifier),
        ISS,
    ));
    let auth_state = AuthApiState::new(
        service.clone(),
        Arc::new(InMemoryRateLimiter::new(100, Duration::from_secs(60))),
    );

    let verifier = Arc::new(Verifier::new(
        StaticJwksProvider::new(jwks),
        VerifierConfig::new(ISS, AUD),
    ));
    let engine_state = EngineApiState::new(verifier.clone());

    let billing_state = if configure_stripe {
        let stripe = StripeConfig::new(
            SecretValue::new(b"sk_test_dummy".to_vec()),
            SecretValue::new(WEBHOOK_SECRET.as_bytes().to_vec()),
            vec![(Tier::Pro, PRICE_PRO.to_owned())],
            "http://localhost:3000/billing?checkout=success",
            "http://localhost:3000/billing?checkout=cancel",
            "http://localhost:3000/billing",
        );
        BillingApiState::with_stripe(verifier.clone(), users.clone(), stripe)
    } else {
        BillingApiState::unconfigured(verifier.clone(), users.clone())
    };
    let org_state = OrgApiState::new(verifier.clone(), Arc::new(InMemoryOrgStore::new()));
    let config_state = ConfigApiState::new(Arc::new(amicu_store::InMemoryConfigStore::new()));

    Harness {
        app: api_router(
            auth_state,
            engine_state,
            billing_state,
            org_state,
            config_state,
            SessionApiState::new(service, verifier),
        ),
        users,
        token,
    }
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.expect("request");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, json)
}

fn now_unix() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// Forge a valid `Stripe-Signature` header for `payload` at time `t` using
/// `secret`, matching Stripe's scheme (`t=…,v1=<hex HMAC-SHA256 of "t.payload">`).
fn sign(payload: &str, secret: &str, t: i64) -> String {
    let signed = format!("{t}.{payload}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(signed.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    format!("t={t},v1={sig}")
}

/// A `customer.subscription.updated` event body for the seeded customer at the
/// given price and status.
fn subscription_event(kind: &str, price_id: &str, status: &str) -> Value {
    json!({
        "id": "evt_test",
        "type": kind,
        "data": { "object": {
            "id": "sub_test123",
            "customer": CUSTOMER_ID,
            "status": status,
            "items": { "data": [ { "price": { "id": price_id } } ] }
        }}
    })
}

fn post_json(path: &str, token: Option<&str>, body: &Value) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn post_webhook(payload: &str, signature: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/billing/webhook")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(s) = signature {
        b = b.header("stripe-signature", s);
    }
    b.body(Body::from(payload.to_owned())).unwrap()
}

// ---------------------------------------------------------------------------
// Auth gate + unconfigured behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checkout_unauthenticated_is_401() {
    let h = harness(Tier::Community, true).await;
    let (status, _) = send(
        &h.app,
        post_json("/billing/checkout", None, &json!({"plan": "pro"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn portal_unauthenticated_is_401() {
    let h = harness(Tier::Community, true).await;
    let (status, _) = send(&h.app, post_json("/billing/portal", None, &json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn checkout_unconfigured_is_503() {
    // Authenticated, but Stripe is not configured: a clean 503, never a panic or
    // a faked checkout URL.
    let h = harness(Tier::Community, false).await;
    let (status, body) = send(
        &h.app,
        post_json("/billing/checkout", Some(&h.token), &json!({"plan": "pro"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body["error"].as_str().unwrap().contains("not configured"));
}

#[tokio::test]
async fn portal_unconfigured_is_503() {
    let h = harness(Tier::Pro, false).await;
    let (status, _) = send(
        &h.app,
        post_json("/billing/portal", Some(&h.token), &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn webhook_unconfigured_is_503() {
    let h = harness(Tier::Community, false).await;
    let (status, _) = send(&h.app, post_webhook("{}", Some("t=1,v1=deadbeef"))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_plan_and_configuration() {
    let h = harness(Tier::Community, true).await;
    let req = Request::builder()
        .method("GET")
        .uri("/billing/status")
        .header(header::AUTHORIZATION, format!("Bearer {}", h.token))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], true);
    assert_eq!(body["plan"], "community");
    assert_eq!(body["has_subscription"], false);
}

#[tokio::test]
async fn status_unauthenticated_is_401() {
    let h = harness(Tier::Community, true).await;
    let req = Request::builder()
        .method("GET")
        .uri("/billing/status")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(&h.app, req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Checkout param validation (before any Stripe call)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checkout_unknown_plan_is_400() {
    let h = harness(Tier::Community, true).await;
    let (status, body) = send(
        &h.app,
        post_json(
            "/billing/checkout",
            Some(&h.token),
            &json!({"plan": "wizard"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("unknown plan"));
}

// ---------------------------------------------------------------------------
// Webhook signature verification + tier persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn webhook_missing_signature_is_400() {
    let h = harness(Tier::Community, true).await;
    let payload =
        subscription_event("customer.subscription.updated", PRICE_PRO, "active").to_string();
    let (status, _) = send(&h.app, post_webhook(&payload, None)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn webhook_bad_signature_is_400() {
    let h = harness(Tier::Community, true).await;
    let payload =
        subscription_event("customer.subscription.updated", PRICE_PRO, "active").to_string();
    // A well-formed header signed with the WRONG secret must be rejected.
    let sig = sign(&payload, "whsec_wrong_secret", now_unix());
    let (status, body) = send(&h.app, post_webhook(&payload, Some(&sig))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("signature"));
    // The tier must be untouched by a rejected webhook.
    let user = h.users.get_by_id(USER_ID).await.unwrap().unwrap();
    assert_eq!(user.tier, Tier::Community);
}

#[tokio::test]
async fn webhook_stale_timestamp_is_400() {
    let h = harness(Tier::Community, true).await;
    let payload =
        subscription_event("customer.subscription.updated", PRICE_PRO, "active").to_string();
    // Correct secret, but a timestamp far outside the tolerance window (replay).
    let sig = sign(&payload, WEBHOOK_SECRET, now_unix() - 10_000);
    let (status, _) = send(&h.app, post_webhook(&payload, Some(&sig))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn webhook_extreme_timestamp_is_rejected_without_panic() {
    // A hostile `t=` at the i64 boundary must not overflow the staleness check
    // (it once could: `now - i64::MIN` panics in debug). It is rejected as far
    // out of window (400) and mutates nothing.
    let h = harness(Tier::Community, true).await;
    h.users
        .set_stripe_customer(USER_ID, CUSTOMER_ID.to_owned())
        .await
        .unwrap();

    let payload =
        subscription_event("customer.subscription.updated", PRICE_PRO, "active").to_string();
    // Signed correctly, but with an extreme (negative) timestamp.
    let signed = format!("{}.{payload}", i64::MIN);
    let mut mac = HmacSha256::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(signed.as_bytes());
    let sig = format!(
        "t={},v1={}",
        i64::MIN,
        hex::encode(mac.finalize().into_bytes())
    );

    let (status, _) = send(&h.app, post_webhook(&payload, Some(&sig))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let user = h.users.get_by_id(USER_ID).await.unwrap().unwrap();
    assert_eq!(user.tier, Tier::Community);
}

#[tokio::test]
async fn webhook_good_signature_upgrades_tier() {
    let h = harness(Tier::Community, true).await;
    // The webhook maps a customer id back to the user, so it must already be
    // bound (checkout would have persisted it; seed it directly here).
    h.users
        .set_stripe_customer(USER_ID, CUSTOMER_ID.to_owned())
        .await
        .unwrap();

    let payload =
        subscription_event("customer.subscription.updated", PRICE_PRO, "active").to_string();
    let sig = sign(&payload, WEBHOOK_SECRET, now_unix());
    let (status, _) = send(&h.app, post_webhook(&payload, Some(&sig))).await;
    assert_eq!(status, StatusCode::OK);

    // The verified event persisted the Pro tier + subscription id. (Takes effect
    // on the user's next token issuance; live JWTs are not revoked.)
    let user = h.users.get_by_id(USER_ID).await.unwrap().unwrap();
    assert_eq!(user.tier, Tier::Pro);
    assert_eq!(user.stripe_subscription_id.as_deref(), Some("sub_test123"));
}

#[tokio::test]
async fn webhook_unknown_price_does_not_grant_tier() {
    let h = harness(Tier::Community, true).await;
    h.users
        .set_stripe_customer(USER_ID, CUSTOMER_ID.to_owned())
        .await
        .unwrap();

    // A valid signature but a price we do not recognize grants no entitlement.
    let payload =
        subscription_event("customer.subscription.updated", "price_unknown", "active").to_string();
    let sig = sign(&payload, WEBHOOK_SECRET, now_unix());
    let (status, _) = send(&h.app, post_webhook(&payload, Some(&sig))).await;
    assert_eq!(status, StatusCode::OK);
    let user = h.users.get_by_id(USER_ID).await.unwrap().unwrap();
    assert_eq!(user.tier, Tier::Community);
}

#[tokio::test]
async fn webhook_subscription_deleted_downgrades_to_community() {
    let h = harness(Tier::Pro, true).await;
    h.users
        .set_stripe_customer(USER_ID, CUSTOMER_ID.to_owned())
        .await
        .unwrap();
    h.users
        .set_billing(USER_ID, Tier::Pro, Some("sub_test123".to_owned()))
        .await
        .unwrap();

    let payload =
        subscription_event("customer.subscription.deleted", PRICE_PRO, "canceled").to_string();
    let sig = sign(&payload, WEBHOOK_SECRET, now_unix());
    let (status, _) = send(&h.app, post_webhook(&payload, Some(&sig))).await;
    assert_eq!(status, StatusCode::OK);

    let user = h.users.get_by_id(USER_ID).await.unwrap().unwrap();
    assert_eq!(user.tier, Tier::Community);
    assert_eq!(user.stripe_subscription_id, None);
}
