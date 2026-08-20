//! HTTP surface for the subscription-billing vertical: Stripe-backed checkout,
//! the customer billing portal, and the subscription-lifecycle webhook.
//!
//! # Shape
//! - [`POST /billing/checkout`](checkout_handler): auth-gated. Creates a Stripe
//!   Checkout Session for the logged-in user for a target plan and returns the
//!   hosted redirect URL. The user's Stripe *customer* is created (once) and its
//!   id persisted so later webhooks map back to the local user.
//! - [`POST /billing/portal`](portal_handler): auth-gated. Creates a Stripe
//!   Billing Portal session so the user can manage/cancel an existing
//!   subscription, and returns its URL.
//! - [`GET /billing/status`](status_handler): auth-gated. Reports whether billing
//!   is configured server-side and the user's authoritative current plan and
//!   whether they hold an active subscription — so the UI shows a real plan and
//!   the right CTA (upgrade vs. manage) instead of guessing.
//! - [`POST /billing/webhook`](webhook_handler): **unauthenticated by the bearer
//!   gate** (Stripe cannot send a first-party JWT). It authenticates instead by
//!   verifying the `Stripe-Signature` HMAC over the raw body (see
//!   [`verify_signature`]); it MUST be mounted outside the auth-gated router.
//!
//! # Real, but gated behind env
//! Stripe is a real integration, activated only when the secret material is
//! present ([`StripeConfig`]). With no keys configured every endpoint returns a
//! clean, documented `503 { "error": "billing is not configured" }` — never a
//! panic and never a faked success. Signature verification is never skipped.
//!
//! # Tier takes effect on next token issuance
//! A billing change updates the user's tier in the [`UserStore`]; it does **not**
//! revoke live access JWTs. The new tier is carried by the next token the user is
//! issued (sign-in or refresh). This is deliberate — see the crate docs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_auth::user::UserStore;
use amicu_auth::{Claims, Tier};
use amicu_core::secret::SecretValue;
use axum::body::Bytes;
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use utoipa::ToSchema;

use crate::engine::AccessVerifier;

type HmacSha256 = Hmac<Sha256>;

/// Default clock-skew tolerance for the webhook timestamp (5 minutes), matching
/// Stripe's own default replay window.
const DEFAULT_WEBHOOK_TOLERANCE_SECS: i64 = 300;

/// Default Stripe REST API base. Overridable so tests can point at a stub.
const DEFAULT_STRIPE_API_BASE: &str = "https://api.stripe.com";

// ---------------------------------------------------------------------------
// Configuration + state
// ---------------------------------------------------------------------------

/// The activated-Stripe configuration. Its presence in [`BillingApiState`] is
/// what flips billing from "not configured" (`503`) to live.
///
/// Secrets are held in [`SecretValue`] wrappers so they are never logged or
/// serialized.
pub struct StripeConfig {
    secret_key: SecretValue,
    webhook_secret: SecretValue,
    /// Bi-directional plan ↔ price mapping. Checkout maps a target [`Tier`] to a
    /// price id; webhooks map an incoming price id back to a [`Tier`].
    prices: Vec<(Tier, String)>,
    /// Where Stripe returns the user after a completed/cancelled checkout.
    success_url: String,
    cancel_url: String,
    /// Where the billing portal returns the user.
    portal_return_url: String,
    /// REST API base (`https://api.stripe.com` in production).
    api_base: String,
    /// Accepted webhook timestamp skew, seconds.
    tolerance_secs: i64,
    http: reqwest::Client,
}

impl StripeConfig {
    /// Assemble a live Stripe configuration.
    ///
    /// `prices` maps each purchasable [`Tier`] to its Stripe price id (`price_…`).
    #[must_use]
    pub fn new(
        secret_key: SecretValue,
        webhook_secret: SecretValue,
        prices: Vec<(Tier, String)>,
        success_url: impl Into<String>,
        cancel_url: impl Into<String>,
        portal_return_url: impl Into<String>,
    ) -> Self {
        Self {
            secret_key,
            webhook_secret,
            prices,
            success_url: success_url.into(),
            cancel_url: cancel_url.into(),
            portal_return_url: portal_return_url.into(),
            api_base: DEFAULT_STRIPE_API_BASE.to_owned(),
            tolerance_secs: DEFAULT_WEBHOOK_TOLERANCE_SECS,
            http: reqwest::Client::new(),
        }
    }

    /// Override the REST API base (tests point this at a local stub).
    #[must_use]
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    /// The Stripe price id configured for a purchasable tier, if any.
    fn price_for(&self, tier: Tier) -> Option<&str> {
        self.prices
            .iter()
            .find(|(t, _)| *t == tier)
            .map(|(_, p)| p.as_str())
    }

    /// The tier a Stripe price id grants, if it is one we recognize.
    fn tier_for_price(&self, price_id: &str) -> Option<Tier> {
        self.prices
            .iter()
            .find(|(_, p)| p == price_id)
            .map(|(t, _)| *t)
    }
}

/// Shared state for the billing endpoints.
///
/// Holds the same [`UserStore`] the auth service writes to (so tier/customer
/// changes land on the one user record), the token verifier that backs the
/// auth gate on checkout/portal/status, and the optional live [`StripeConfig`].
#[derive(Clone)]
pub struct BillingApiState {
    verifier: Arc<dyn AccessVerifier>,
    users: Arc<dyn UserStore>,
    stripe: Option<Arc<StripeConfig>>,
}

impl BillingApiState {
    /// State with Stripe **not** configured: every endpoint returns `503`.
    #[must_use]
    pub fn unconfigured(verifier: Arc<dyn AccessVerifier>, users: Arc<dyn UserStore>) -> Self {
        Self {
            verifier,
            users,
            stripe: None,
        }
    }

    /// State with a live Stripe configuration.
    #[must_use]
    pub fn with_stripe(
        verifier: Arc<dyn AccessVerifier>,
        users: Arc<dyn UserStore>,
        stripe: StripeConfig,
    ) -> Self {
        Self {
            verifier,
            users,
            stripe: Some(Arc::new(stripe)),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /billing/checkout`: the plan the user wants to buy.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub(crate) struct CheckoutRequest {
    /// Target plan, e.g. `"pro"` or `"enterprise"` (a [`Tier`] name).
    #[schema(example = "pro")]
    pub plan: String,
}

/// A hosted Stripe URL to redirect the browser to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub(crate) struct RedirectResponse {
    /// The Stripe-hosted URL (checkout or billing portal).
    pub url: String,
}

/// The user's billing status: whether billing is live server-side, the
/// authoritative current plan, and whether an active subscription exists.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub(crate) struct BillingStatusResponse {
    /// Whether Stripe is configured on the server. When `false` the UI must not
    /// present checkout/manage actions (they would `503`).
    pub configured: bool,
    /// The user's current plan, read from the durable user record (not just the
    /// possibly-stale token). Lowercase tier name, e.g. `"community"`.
    pub plan: String,
    /// Whether the user has an active Stripe subscription (drives manage-vs-buy).
    pub has_subscription: bool,
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

/// Errors surfaced by the billing handlers, each mapped to a clean HTTP status
/// with a body that leaks no server internals.
pub(crate) enum BillingError {
    /// Stripe secrets are absent: the feature is not activated (`503`).
    NotConfigured,
    /// The request body/params are invalid, e.g. an unknown plan (`400`).
    BadRequest(String),
    /// The user has no billing relationship yet (portal with no customer) (`409`).
    NoBillingAccount,
    /// A call to Stripe failed (network/API). Logged server-side; generic to the
    /// client (`502`).
    Upstream,
    /// The webhook signature was missing, malformed, stale, or wrong (`400`).
    BadSignature,
}

impl IntoResponse for BillingError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            BillingError::NotConfigured => {
                (StatusCode::SERVICE_UNAVAILABLE, "billing is not configured")
            }
            BillingError::BadRequest(m) => {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": m }))).into_response();
            }
            BillingError::NoBillingAccount => (
                StatusCode::CONFLICT,
                "no billing account yet; start a checkout first",
            ),
            BillingError::Upstream => (StatusCode::BAD_GATEWAY, "billing provider is unreachable"),
            BillingError::BadSignature => (StatusCode::BAD_REQUEST, "invalid webhook signature"),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Router + auth gate
// ---------------------------------------------------------------------------

/// Extract a `Bearer` token from the `Authorization` header.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Auth middleware for the user-facing billing endpoints (checkout, portal,
/// status). Any authenticated user (min tier Community) may manage their own
/// billing — a Community user upgrading is the primary flow. On success the
/// verified [`Claims`] are stashed in the request extensions.
///
/// The webhook route is deliberately **not** wrapped by this gate: Stripe
/// authenticates by request signature, not a bearer token.
async fn billing_auth_gate(
    State(state): State<BillingApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_token(request.headers()) else {
        return unauthorized("missing or malformed bearer token");
    };
    match state.verifier.verify(token).await {
        Ok(claims) => {
            request.extensions_mut().insert(claims);
            next.run(request).await
        }
        Err(_) => unauthorized("invalid or expired access token"),
    }
}

fn unauthorized(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
}

/// Maximum accepted webhook body size (64 KiB): Stripe events are small; the cap
/// bounds parse cost and rejects oversized payloads before buffering.
pub const MAX_WEBHOOK_BODY_BYTES: usize = 64 * 1024;

/// Build the billing sub-router: the auth-gated user endpoints plus the
/// signature-authenticated (bearer-unauthenticated) webhook.
pub fn billing_router(state: BillingApiState) -> Router {
    let gated = Router::new()
        .route("/billing/checkout", post(checkout_handler))
        .route("/billing/portal", post(portal_handler))
        .route("/billing/status", get(status_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            billing_auth_gate,
        ));

    // The webhook authenticates by Stripe signature, so it stays outside the
    // bearer gate. It caps its own body size.
    let webhook = Router::new()
        .route("/billing/webhook", post(webhook_handler))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES));

    gated.merge(webhook).with_state(state)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Parse a lowercase tier name from a checkout request.
fn parse_plan(plan: &str) -> Option<Tier> {
    match plan.trim().to_ascii_lowercase().as_str() {
        "pro" => Some(Tier::Pro),
        "enterprise" => Some(Tier::Enterprise),
        "hosted" => Some(Tier::Hosted),
        _ => None,
    }
}

/// Create a Stripe Checkout Session for the logged-in user and return its URL.
///
/// Ensures the user has a Stripe customer (creating and persisting one on first
/// use so later webhooks map back), then opens a subscription checkout for the
/// requested plan's price. Returns `503` when billing is unconfigured.
#[utoipa::path(
    post,
    path = "/billing/checkout",
    tag = "billing",
    request_body = CheckoutRequest,
    responses(
        (status = 200, description = "A Stripe Checkout Session URL to redirect the browser to.", body = RedirectResponse),
        (status = 400, description = "Unknown or non-purchasable plan."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 502, description = "The billing provider could not be reached."),
        (status = 503, description = "Billing is not configured on this server."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn checkout_handler(
    State(state): State<BillingApiState>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<CheckoutRequest>,
) -> Result<Json<RedirectResponse>, BillingError> {
    let stripe = state.stripe.as_ref().ok_or(BillingError::NotConfigured)?;

    let tier = parse_plan(&body.plan)
        .ok_or_else(|| BillingError::BadRequest(format!("unknown plan: {}", body.plan)))?;
    let price_id = stripe
        .price_for(tier)
        .ok_or_else(|| BillingError::BadRequest(format!("plan is not purchasable: {}", body.plan)))?
        .to_owned();

    // Look up the user record for their email and any existing customer id.
    let user = state
        .users
        .get_by_id(&claims.sub)
        .await
        .map_err(|_| BillingError::Upstream)?
        .ok_or(BillingError::Upstream)?;

    // Ensure a persisted Stripe customer so webhooks can map back to this user.
    let customer_id = if let Some(id) = user.stripe_customer_id.clone() {
        id
    } else {
        let id = create_customer(stripe, &user.email, &user.id).await?;
        state
            .users
            .set_stripe_customer(&user.id, id.clone())
            .await
            .map_err(|_| BillingError::Upstream)?;
        id
    };

    let url = create_checkout_session(stripe, &customer_id, &price_id, &claims.sub).await?;
    Ok(Json(RedirectResponse { url }))
}

/// Create a Stripe Billing Portal session so the user can manage their
/// subscription, and return its URL. `409` if the user has no customer yet.
#[utoipa::path(
    post,
    path = "/billing/portal",
    tag = "billing",
    responses(
        (status = 200, description = "A Stripe Billing Portal URL to redirect the browser to.", body = RedirectResponse),
        (status = 401, description = "Missing or invalid access token."),
        (status = 409, description = "The user has no billing account yet."),
        (status = 502, description = "The billing provider could not be reached."),
        (status = 503, description = "Billing is not configured on this server."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn portal_handler(
    State(state): State<BillingApiState>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<RedirectResponse>, BillingError> {
    let stripe = state.stripe.as_ref().ok_or(BillingError::NotConfigured)?;

    let user = state
        .users
        .get_by_id(&claims.sub)
        .await
        .map_err(|_| BillingError::Upstream)?
        .ok_or(BillingError::Upstream)?;
    let customer_id = user
        .stripe_customer_id
        .ok_or(BillingError::NoBillingAccount)?;

    let url = create_portal_session(stripe, &customer_id).await?;
    Ok(Json(RedirectResponse { url }))
}

/// Report the user's billing status: whether Stripe is configured, their
/// authoritative current plan, and whether they hold a subscription.
#[utoipa::path(
    get,
    path = "/billing/status",
    tag = "billing",
    responses(
        (status = 200, description = "The user's billing status and current plan.", body = BillingStatusResponse),
        (status = 401, description = "Missing or invalid access token."),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn status_handler(
    State(state): State<BillingApiState>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<BillingStatusResponse>, BillingError> {
    // Read the durable record so "current plan" reflects the latest webhook, not
    // just the (possibly older) tier baked into the presented token.
    let user = state
        .users
        .get_by_id(&claims.sub)
        .await
        .map_err(|_| BillingError::Upstream)?;
    let (plan, has_subscription) = match user {
        Some(u) => (tier_name(u.tier), u.stripe_subscription_id.is_some()),
        // Token valid but user not in this store (e.g. verifier trusts an issuer
        // whose users live elsewhere): fall back to the token's own tier claim.
        None => (tier_name(claims.tier), false),
    };
    Ok(Json(BillingStatusResponse {
        configured: state.stripe.is_some(),
        plan,
        has_subscription,
    }))
}

/// Lowercase wire name for a tier (matches the token claim's serde rename).
fn tier_name(tier: Tier) -> String {
    match tier {
        Tier::Community => "community",
        Tier::Pro => "pro",
        Tier::Enterprise => "enterprise",
        Tier::Hosted => "hosted",
    }
    .to_owned()
}

/// The Stripe subscription-lifecycle webhook.
///
/// Authenticated by the `Stripe-Signature` HMAC over the raw body (no bearer
/// token). On `customer.subscription.created|updated` the subscription's price
/// is mapped to a tier and persisted; on `customer.subscription.deleted` the
/// user is dropped back to Community. Unknown event types are acknowledged with
/// `200` and ignored. The new tier applies on the user's next token issuance.
#[utoipa::path(
    post,
    path = "/billing/webhook",
    tag = "billing",
    request_body(content = String, description = "Raw Stripe event JSON (signature-verified)."),
    responses(
        (status = 200, description = "Event received (and applied, if relevant)."),
        (status = 400, description = "Missing/invalid signature or malformed body."),
        (status = 503, description = "Billing is not configured on this server."),
    ),
)]
pub(crate) async fn webhook_handler(
    State(state): State<BillingApiState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, BillingError> {
    let stripe = state.stripe.as_ref().ok_or(BillingError::NotConfigured)?;

    let signature = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(BillingError::BadSignature)?;

    verify_signature(
        &body,
        signature,
        stripe.webhook_secret.expose_secret(),
        stripe.tolerance_secs,
        now_unix(),
    )
    .map_err(|_| BillingError::BadSignature)?;

    // Signature is valid: parse the event and apply subscription-lifecycle ones.
    let event: Value = serde_json::from_slice(&body)
        .map_err(|_| BillingError::BadRequest("webhook body is not valid JSON".to_owned()))?;
    apply_event(stripe, state.users.as_ref(), &event).await;
    Ok(StatusCode::OK)
}

// ---------------------------------------------------------------------------
// Webhook signature verification
// ---------------------------------------------------------------------------

/// A webhook signature that did not verify. Deliberately opaque: the handler
/// maps every cause (missing/malformed header, stale timestamp, no matching
/// signature) to the same generic `400`, never telling the caller which check
/// failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureError;

/// Verify a Stripe webhook signature.
///
/// Implements Stripe's scheme: the `Stripe-Signature` header is
/// `t=<unix>,v1=<hex-hmac>[,v1=<hex-hmac>…]`; the signed payload is
/// `"{t}.{raw_body}"`, HMAC-SHA256'd with the endpoint's webhook secret. The
/// comparison is constant-time (via [`Mac::verify_slice`]) and the timestamp
/// must be within `tolerance_secs` of `now` (replay defense). At least one `v1`
/// signature must match.
///
/// # Errors
/// Returns [`SignatureError`] on a missing/malformed header, a stale timestamp,
/// or no matching signature.
pub fn verify_signature(
    payload: &[u8],
    signature_header: &str,
    secret: &[u8],
    tolerance_secs: i64,
    now: i64,
) -> Result<(), SignatureError> {
    // Parse `t=…` and all `v1=…` fields.
    let mut timestamp: Option<i64> = None;
    let mut v1_sigs: Vec<Vec<u8>> = Vec::new();
    for part in signature_header.split(',') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k.trim() {
            "t" => timestamp = v.trim().parse::<i64>().ok(),
            "v1" => {
                if let Ok(bytes) = hex::decode(v.trim()) {
                    v1_sigs.push(bytes);
                }
            }
            _ => {}
        }
    }

    let timestamp = timestamp.ok_or(SignatureError)?;
    if v1_sigs.is_empty() {
        return Err(SignatureError);
    }
    // Replay defense: reject timestamps outside the tolerance window in either
    // direction. Overflow-safe against a hostile `t=` (e.g. `i64::MIN`): the
    // saturating ops clamp rather than panic in debug / wrap in release, so an
    // extreme timestamp is simply treated as far out of window and rejected.
    if now.saturating_sub(timestamp).saturating_abs() > tolerance_secs {
        return Err(SignatureError);
    }

    // Signed payload is `"<t>.<raw body>"`.
    let mut signed = Vec::with_capacity(payload.len() + 16);
    signed.extend_from_slice(timestamp.to_string().as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(payload);

    // At least one provided v1 signature must match (constant-time).
    for candidate in &v1_sigs {
        let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts a key of any length");
        mac.update(&signed);
        if mac.verify_slice(candidate).is_ok() {
            return Ok(());
        }
    }
    Err(SignatureError)
}

// ---------------------------------------------------------------------------
// Event application
// ---------------------------------------------------------------------------

/// Apply a signature-verified Stripe event to the user store. Best-effort:
/// unknown/irrelevant events and unmapped customers are silently ignored (the
/// `200` ack tells Stripe not to retry).
async fn apply_event(stripe: &StripeConfig, users: &dyn UserStore, event: &Value) {
    let event_type = event["type"].as_str().unwrap_or_default();
    let object = &event["data"]["object"];

    match event_type {
        "customer.subscription.created" | "customer.subscription.updated" => {
            let Some(customer) = object["customer"].as_str() else {
                return;
            };
            let subscription_id = object["id"].as_str().map(str::to_owned);
            let status = object["status"].as_str().unwrap_or_default();
            // The first line item's price determines the tier.
            let price_id = object["items"]["data"][0]["price"]["id"].as_str();

            // A canceled/incomplete/unpaid subscription grants nothing.
            let active = matches!(status, "active" | "trialing" | "past_due");
            let tier = match (active, price_id.and_then(|p| stripe.tier_for_price(p))) {
                (true, Some(t)) => t,
                // Recognized-but-inactive, or an unknown price: no entitlement.
                _ => Tier::Community,
            };

            if let Ok(Some(user)) = users.get_by_stripe_customer(customer).await {
                let sub = if tier == Tier::Community {
                    None
                } else {
                    subscription_id
                };
                let _ = users.set_billing(&user.id, tier, sub).await;
            }
        }
        "customer.subscription.deleted" => {
            if let Some(customer) = object["customer"].as_str()
                && let Ok(Some(user)) = users.get_by_stripe_customer(customer).await
            {
                let _ = users.set_billing(&user.id, Tier::Community, None).await;
            }
        }
        _ => {} // Unhandled event types are acknowledged and ignored.
    }
}

// ---------------------------------------------------------------------------
// Stripe REST client (thin, typed, form-encoded)
// ---------------------------------------------------------------------------

/// Create a Stripe customer for the user and return its id (`cus_…`).
async fn create_customer(
    stripe: &StripeConfig,
    email: &str,
    user_id: &str,
) -> Result<String, BillingError> {
    let params = [
        ("email", email.to_owned()),
        ("metadata[user_id]", user_id.to_owned()),
    ];
    let value = stripe_post(stripe, "/v1/customers", &params).await?;
    value["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or(BillingError::Upstream)
}

/// Create a subscription Checkout Session and return its hosted URL.
async fn create_checkout_session(
    stripe: &StripeConfig,
    customer_id: &str,
    price_id: &str,
    user_id: &str,
) -> Result<String, BillingError> {
    let params = [
        ("mode", "subscription".to_owned()),
        ("customer", customer_id.to_owned()),
        ("client_reference_id", user_id.to_owned()),
        ("line_items[0][price]", price_id.to_owned()),
        ("line_items[0][quantity]", "1".to_owned()),
        ("success_url", stripe.success_url.clone()),
        ("cancel_url", stripe.cancel_url.clone()),
    ];
    let value = stripe_post(stripe, "/v1/checkout/sessions", &params).await?;
    value["url"]
        .as_str()
        .map(str::to_owned)
        .ok_or(BillingError::Upstream)
}

/// Create a Billing Portal session and return its hosted URL.
async fn create_portal_session(
    stripe: &StripeConfig,
    customer_id: &str,
) -> Result<String, BillingError> {
    let params = [
        ("customer", customer_id.to_owned()),
        ("return_url", stripe.portal_return_url.clone()),
    ];
    let value = stripe_post(stripe, "/v1/billing_portal/sessions", &params).await?;
    value["url"]
        .as_str()
        .map(str::to_owned)
        .ok_or(BillingError::Upstream)
}

/// POST form-encoded params to the Stripe REST API and parse the JSON body.
///
/// Failures (transport, non-2xx, unparseable body) collapse to
/// [`BillingError::Upstream`] after logging the real cause server-side, so
/// nothing about the provider leaks to the client.
async fn stripe_post(
    stripe: &StripeConfig,
    path: &str,
    params: &[(&str, String)],
) -> Result<Value, BillingError> {
    let url = format!("{}{}", stripe.api_base, path);
    let secret = stripe
        .secret_key
        .expose_secret_str()
        .map_err(|_| BillingError::Upstream)?;
    // Stripe expects `application/x-www-form-urlencoded` request bodies; encode
    // the params explicitly (reqwest's `.form()` helper is compiled out under our
    // minimal, default-features-off reqwest build).
    let form_body = serde_urlencoded::to_string(params).map_err(|e| {
        eprintln!("amicu-api: encoding stripe form for {path} failed: {e}");
        BillingError::Upstream
    })?;
    let response = stripe
        .http
        .post(&url)
        .bearer_auth(secret)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form_body)
        .send()
        .await
        .map_err(|e| {
            eprintln!("amicu-api: stripe request to {path} failed: {e}");
            BillingError::Upstream
        })?;

    let status = response.status();
    let body = response.bytes().await.map_err(|e| {
        eprintln!("amicu-api: reading stripe response from {path} failed: {e}");
        BillingError::Upstream
    })?;

    if !status.is_success() {
        // Do not echo Stripe's error body to the client; log it for operators.
        eprintln!(
            "amicu-api: stripe {path} returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
        return Err(BillingError::Upstream);
    }

    serde_json::from_slice(&body).map_err(|e| {
        eprintln!("amicu-api: stripe {path} response was not JSON: {e}");
        BillingError::Upstream
    })
}

/// Current wall-clock time in seconds since the Unix epoch.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------
// Env-driven construction helper (used by the server bootstrap)
// ---------------------------------------------------------------------------

/// Build a [`StripeConfig`] from a map of environment values, or `None` when the
/// required secrets are absent (billing stays unconfigured, endpoints `503`).
///
/// Required: `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET`. At least one price
/// mapping (`STRIPE_PRICE_PRO`, `STRIPE_PRICE_ENTERPRISE`, `STRIPE_PRICE_HOSTED`)
/// should be set for checkout to offer anything. URLs default to localhost dev
/// values when unset.
#[must_use]
pub fn stripe_config_from_env<S: std::hash::BuildHasher>(
    env: &HashMap<String, String, S>,
) -> Option<StripeConfig> {
    let secret_key = non_empty(env, "STRIPE_SECRET_KEY")?;
    let webhook_secret = non_empty(env, "STRIPE_WEBHOOK_SECRET")?;

    let mut prices = Vec::new();
    if let Some(p) = non_empty(env, "STRIPE_PRICE_PRO") {
        prices.push((Tier::Pro, p));
    }
    if let Some(p) = non_empty(env, "STRIPE_PRICE_ENTERPRISE") {
        prices.push((Tier::Enterprise, p));
    }
    if let Some(p) = non_empty(env, "STRIPE_PRICE_HOSTED") {
        prices.push((Tier::Hosted, p));
    }

    let success_url = non_empty(env, "STRIPE_SUCCESS_URL")
        .unwrap_or_else(|| "http://localhost:3000/account/billing?checkout=success".to_owned());
    let cancel_url = non_empty(env, "STRIPE_CANCEL_URL")
        .unwrap_or_else(|| "http://localhost:3000/account/billing?checkout=cancel".to_owned());
    let portal_return_url = non_empty(env, "STRIPE_PORTAL_RETURN_URL")
        .unwrap_or_else(|| "http://localhost:3000/account/billing".to_owned());

    let mut config = StripeConfig::new(
        SecretValue::new(secret_key.into_bytes()),
        SecretValue::new(webhook_secret.into_bytes()),
        prices,
        success_url,
        cancel_url,
        portal_return_url,
    );
    if let Some(base) = non_empty(env, "STRIPE_API_BASE") {
        config = config.with_api_base(base);
    }
    Some(config)
}

/// Fetch an env value, treating empty/whitespace as absent.
fn non_empty<S: std::hash::BuildHasher>(
    env: &HashMap<String, String, S>,
    key: &str,
) -> Option<String> {
    env.get(key)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}
