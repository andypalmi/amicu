//! The internal outbox drain: `POST /internal/outbox/drain`
//!
//! # Why this endpoint exists
//!
//! On Cloud Run a container's CPU is throttled to near-zero as soon as the
//! response is written, so email cannot be sent "after" a request. Every
//! transactional email is therefore persisted at request time (see
//! [`amicu_store::outbox`]) and actually delivered here, INSIDE a request, where
//! CPU is guaranteed. Cloud Scheduler calls this once a minute.
//!
//! # Not part of the public API
//!
//! Like `GET /healthz` / `GET /readyz` (see [`crate::health`]), this route is
//! deliberately kept out of [`crate::api_openapi`] and the committed
//! `openapi.json`: it is infra plumbing with a machine caller, not client-facing
//! API surface, so wiring it in never forces a web codegen regeneration. It is
//! merged onto the main router by the bootstrap, not routed through
//! [`crate::api_router`].
//!
//! # Authentication
//!
//! Two accepted mechanisms, tried in that order:
//!
//! 1. **Google OIDC** (production on Cloud Run). Cloud Scheduler mints a
//!    Google-signed ID token as a dedicated invoker service account. Verified by
//!    [`GoogleIdTokenVerifier`]: issuer, signature against Google's live JWKS,
//!    `aud` equal to the configured audience (the Cloud Run service URL), and an
//!    allowlisted, verified `email`. No shared secret exists anywhere in this
//!    path.
//! 2. **A static bearer token** (`AMICU_INTERNAL_TOKEN`), for self-hosters
//!    driving the drain from cron + curl with no Google identity to present.
//!    Compared in CONSTANT TIME over SHA-256 digests (see [`ct_eq`]) so the
//!    comparison leaks neither the secret's length nor a matching prefix.
//!
//! With NEITHER configured the endpoint is disabled and answers `503`: it must
//! never be an unauthenticated way to make a deployment send mail (or to probe
//! how much mail it has queued).
//!
//! Token values - the bearer header, the OIDC assertion, the configured static
//! secret - are never logged, at any level. Only the outcome is.

use std::sync::Arc;
use std::time::{Duration, Instant};

use amicu_auth::error::AuthError;
use amicu_auth::google::{GoogleCertsSource, GoogleIdTokenVerifier};
use amicu_auth::notify::Notifier;
use amicu_store::outbox::{FailureOutcome, OutboxStore, plan_failure};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

/// Default cap on how many messages one drain request claims and sends.
pub const DEFAULT_DRAIN_BATCH_SIZE: u32 = 50;

/// Default wall-clock budget for one drain request, in milliseconds.
///
/// Cloud Scheduler's HTTP attempt deadline for this job is 30s
/// (the infrastructure repo's `modules/scheduler`), so the drain must return comfortably
/// inside that even when every send is slow. 20s leaves 10s of headroom; the
/// remaining messages are simply claimed by the next scheduled invocation.
pub const DEFAULT_DRAIN_BUDGET_MS: u64 = 20_000;

/// Default for the inline (post-enqueue) send: at most this many messages.
pub const DEFAULT_INLINE_BATCH_SIZE: u32 = 1;

/// Default wall-clock budget, in milliseconds, of the inline send performed by
/// the sign-up / recover / resend-verification handlers. It is BOTH the cap on
/// the send and the fixed duration of that phase (see [`InlineSend`]).
pub const DEFAULT_INLINE_BUDGET_MS: u64 = 1_000;

/// Hard ceiling on the batch size, whatever the configuration asks for. A
/// runaway value would let one request hold a claim lease over the whole queue.
const MAX_DRAIN_BATCH_SIZE: u32 = 500;

/// How the drain endpoint authenticates its caller.
///
/// Both mechanisms are optional and independent; see the module docs. All-`None`
/// means the endpoint is disabled.
pub struct DrainAuth {
    /// SHA-256 of the configured static bearer token, when one is set. The
    /// secret itself is never retained, so it cannot leak through a `Debug` or a
    /// core dump of this struct.
    static_token_digest: Option<[u8; 32]>,
    /// The Google OIDC verifier, when audience + caller allowlist are both
    /// configured.
    oidc: Option<Box<dyn VerifyGoogleIdToken>>,
}

impl std::fmt::Debug for DrainAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the digest: it is a (weak) oracle on the secret.
        f.debug_struct("DrainAuth")
            .field("static_token", &self.static_token_digest.is_some())
            .field("oidc", &self.oidc.is_some())
            .finish()
    }
}

/// Object-safe wrapper over [`GoogleIdTokenVerifier`], which is generic over its
/// certs source and clock. Lets [`DrainAuth`] hold one without leaking those
/// parameters into every downstream type.
#[async_trait::async_trait]
trait VerifyGoogleIdToken: Send + Sync {
    async fn verify(&self, token: &str) -> Result<(), AuthError>;
}

#[async_trait::async_trait]
impl<S: GoogleCertsSource + 'static> VerifyGoogleIdToken for GoogleIdTokenVerifier<S> {
    async fn verify(&self, token: &str) -> Result<(), AuthError> {
        GoogleIdTokenVerifier::verify(self, token).await.map(|_| ())
    }
}

impl Default for DrainAuth {
    fn default() -> Self {
        Self::disabled()
    }
}

impl DrainAuth {
    /// No mechanism configured: the endpoint answers `503`.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            static_token_digest: None,
            oidc: None,
        }
    }

    /// Accept a static bearer token, compared in constant time.
    ///
    /// An empty/whitespace-only value is treated as "not configured": an
    /// accidentally-blank secret must not become a token everybody knows.
    #[must_use]
    pub fn with_static_token(mut self, token: &str) -> Self {
        if !token.trim().is_empty() {
            self.static_token_digest = Some(Sha256::digest(token.as_bytes()).into());
        }
        self
    }

    /// Accept Google-issued OIDC ID tokens verified by `verifier`.
    #[must_use]
    pub fn with_google_oidc<S: GoogleCertsSource + 'static>(
        mut self,
        verifier: GoogleIdTokenVerifier<S>,
    ) -> Self {
        self.oidc = Some(Box::new(verifier));
        self
    }

    /// Whether any mechanism is configured at all.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.static_token_digest.is_some() || self.oidc.is_some()
    }

    /// Authorize a request's headers.
    ///
    /// The static-token comparison runs first and is cheap; the OIDC path is
    /// tried only if it did not match, so an OIDC-only deployment costs nothing
    /// extra and a static-token-only deployment never touches the network.
    ///
    /// # Errors
    /// [`DrainAuthError`] describing exactly how the caller was rejected.
    async fn authorize(&self, headers: &HeaderMap) -> Result<(), DrainAuthError> {
        if !self.is_configured() {
            return Err(DrainAuthError::Disabled);
        }
        let Some(presented) = bearer_token(headers) else {
            return Err(DrainAuthError::Missing);
        };

        if let Some(expected) = self.static_token_digest.as_ref() {
            let presented_digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
            if ct_eq(expected, &presented_digest) {
                return Ok(());
            }
        }

        match self.oidc.as_ref() {
            // A token that is neither the static secret nor an OIDC assertion we
            // can even attempt to verify is simply unauthenticated.
            None => Err(DrainAuthError::Unauthenticated),
            Some(verifier) => verifier.verify(presented).await.map_err(|e| match e {
                // A valid Google token from an identity we do not accept is a
                // rejection of the CALLER, not of the credential: 403.
                AuthError::Forbidden => DrainAuthError::ForbiddenCaller,
                _ => DrainAuthError::Unauthenticated,
            }),
        }
    }
}

/// Why a drain request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainAuthError {
    /// Neither auth mechanism is configured; the endpoint is switched off.
    Disabled,
    /// No `Authorization: Bearer` header at all.
    Missing,
    /// The credential did not verify (wrong static token, bad signature, wrong
    /// `aud`, wrong `iss`, expired, unknown `kid`, certs unavailable).
    Unauthenticated,
    /// The credential verified, but the identity it proves is not allowlisted.
    ForbiddenCaller,
}

impl DrainAuthError {
    /// The HTTP status and generic body for this rejection. Deliberately coarse:
    /// the response never distinguishes "wrong static token" from "bad
    /// signature" from "expired", so it is not an oracle on the configuration.
    fn into_response(self) -> Response {
        let (status, message) = match self {
            DrainAuthError::Disabled => (
                StatusCode::SERVICE_UNAVAILABLE,
                "outbox drain is not configured",
            ),
            DrainAuthError::Missing | DrainAuthError::Unauthenticated => {
                (StatusCode::UNAUTHORIZED, "unauthorized")
            }
            DrainAuthError::ForbiddenCaller => (StatusCode::FORBIDDEN, "caller not allowed"),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// Extract the value of an `Authorization: Bearer <token>` header.
///
/// The scheme match is case-insensitive (RFC 7235); the token itself is
/// returned verbatim apart from surrounding whitespace.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Constant-time equality over two fixed-length digests.
///
/// Hand-rolled rather than pulling in a new dependency (`subtle` is not in this
/// workspace): both inputs are fixed-size SHA-256 outputs, so there is no length
/// to leak, and the fold below always visits all 32 bytes - it cannot short
/// circuit on the first mismatching byte the way `==` may.
///
/// `#[inline(never)]` keeps a future optimizer from inlining this into a context
/// where it could be rewritten into an early-exit comparison.
#[inline(never)]
#[must_use]
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// State for the drain endpoint.
#[derive(Clone)]
pub struct OutboxApiState {
    store: Arc<dyn OutboxStore>,
    notifier: Arc<dyn Notifier>,
    auth: Arc<DrainAuth>,
    batch_size: u32,
    budget: Duration,
}

impl OutboxApiState {
    /// Assemble state from the outbox store, the mailer, and the caller
    /// authentication policy. Batch size and budget take their defaults; see
    /// [`with_batch_size`](Self::with_batch_size) /
    /// [`with_budget`](Self::with_budget).
    #[must_use]
    pub fn new(store: Arc<dyn OutboxStore>, notifier: Arc<dyn Notifier>, auth: DrainAuth) -> Self {
        Self {
            store,
            notifier,
            auth: Arc::new(auth),
            batch_size: DEFAULT_DRAIN_BATCH_SIZE,
            budget: Duration::from_millis(DEFAULT_DRAIN_BUDGET_MS),
        }
    }

    /// Cap the number of messages one request claims and sends. Clamped to
    /// `1..=`[`MAX_DRAIN_BATCH_SIZE`]; `0` would make the endpoint a no-op.
    #[must_use]
    pub fn with_batch_size(mut self, batch_size: u32) -> Self {
        self.batch_size = batch_size.clamp(1, MAX_DRAIN_BATCH_SIZE);
        self
    }

    /// Cap the wall-clock time one request spends draining.
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }
}

/// What one drain pass did. Every field is a plain count - nothing here
/// identifies a recipient or carries any message content.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DrainReport {
    /// Messages claimed (and therefore attempted) this pass.
    pub claimed: u32,
    /// Delivered successfully.
    pub sent: u32,
    /// Failed and scheduled for a later retry.
    pub failed: u32,
    /// Failed for the last time and dead-lettered.
    pub dead_lettered: u32,
    /// `true` if the pass stopped because it ran out of time budget rather than
    /// because the queue was empty - i.e. there is probably more to do.
    pub budget_exhausted: bool,
}

/// Run one drain pass: claim due messages in bounded batches, send each through
/// `notifier`, and record the outcome, stopping when the queue is empty or the
/// time budget is spent.
///
/// Exposed (rather than private to the handler) so the `amicu-server`
/// `outbox-drain` subcommand runs the EXACT same logic from the command line,
/// with no duplicated state machine.
///
/// # Errors
/// [`amicu_store::StoreError`] if the queue cannot be read or a result cannot be
/// recorded. A failure to SEND is never an error here: it is recorded on the
/// message and reflected in the returned counts.
pub async fn drain_once(
    store: &dyn OutboxStore,
    notifier: &dyn Notifier,
    batch_size: u32,
    budget: Duration,
) -> Result<DrainReport, amicu_store::StoreError> {
    let started = Instant::now();
    let mut report = DrainReport::default();
    let limit = i64::from(batch_size.clamp(1, MAX_DRAIN_BATCH_SIZE));

    loop {
        if started.elapsed() >= budget {
            report.budget_exhausted = true;
            break;
        }
        let batch = store.claim_due(now_unix(), limit).await?;
        if batch.is_empty() {
            break;
        }
        let claimed_in_batch = batch.len();
        for message in batch {
            report.claimed = report.claimed.saturating_add(1);
            let id = message.id;
            let attempts = message.attempts;

            // A payload this build cannot render can never succeed, so retrying
            // it would burn attempts for nothing: dead-letter it immediately.
            let Some(notification) = message.to_notification() else {
                store
                    .mark_failed(
                        id,
                        FailureOutcome::DeadLetter,
                        now_unix(),
                        "unrenderable stored payload",
                    )
                    .await?;
                report.dead_lettered = report.dead_lettered.saturating_add(1);
                continue;
            };

            match notifier.notify(notification).await {
                Ok(()) => {
                    store.mark_sent(id, now_unix()).await?;
                    report.sent = report.sent.saturating_add(1);
                }
                Err(err) => {
                    let now = now_unix();
                    let outcome = plan_failure(attempts, now);
                    // `AuthError`'s Display is a coarse, fixed string that
                    // carries no token, address, or credential material (see its
                    // module docs), so it is safe to persist and return.
                    store
                        .mark_failed(id, outcome, now, &err.to_string())
                        .await?;
                    match outcome {
                        FailureOutcome::Retry { .. } => {
                            report.failed = report.failed.saturating_add(1);
                        }
                        FailureOutcome::DeadLetter => {
                            report.dead_lettered = report.dead_lettered.saturating_add(1);
                        }
                    }
                }
            }

            if started.elapsed() >= budget {
                report.budget_exhausted = true;
                break;
            }
        }
        if report.budget_exhausted {
            break;
        }
        // A short batch means the queue is drained; another round trip would
        // just return nothing.
        if claimed_in_batch < usize::try_from(limit).unwrap_or(usize::MAX) {
            break;
        }
    }

    Ok(report)
}

/// Best-effort delivery right after a request enqueued mail.
///
/// Cloud Run throttles CPU once the response is written, so a detached task
/// cannot be trusted to send; this runs INSIDE the request, after the handler's
/// enqueue transaction has committed, and before the response is returned.
///
/// # Same claim path as the scheduler
///
/// It calls [`drain_once`], i.e. the same `claim_due` (`FOR UPDATE SKIP LOCKED`
/// plus a lease), `mark_sent` and `plan_failure`/`mark_failed` accounting. A
/// concurrent scheduler drain therefore cannot double-send, and a failed inline
/// attempt is recorded exactly like a failed scheduled one: the row stays
/// pending with backoff and the scheduler sweep retries it.
///
/// # Best-effort
///
/// Every failure (SMTP, DB, timeout) is swallowed and logged at `warn` with
/// counts only (no addresses, tokens, or error text from the mailer). The HTTP
/// response is never affected. If the budget elapses mid-send the claimed row
/// is NOT cancelled: it finishes on a detached task (possibly late on Cloud Run,
/// where CPU is throttled after the response) and records its outcome normally.
/// A task lost with its instance leaves the row leased; the sweep retries it
/// once the lease expires.
///
/// # Anti-enumeration
///
/// `/signup`, `/recover` and `/verify/resend` enqueue mail only when the account
/// state warrants it. Sending inline only in that case would make the response
/// time depend on whether the account exists (an SMTP round trip vs nothing).
/// So [`run_padded`](Self::run_padded) is called UNCONDITIONALLY by those
/// handlers and always occupies exactly `budget` of wall-clock time: the send is
/// cut off at the budget by a timeout, and if it finishes early we sleep out the
/// remainder. The observable duration is therefore the constant `budget`
/// whether there was something to send or not. (Same fixed-floor technique as
/// the service layer's `ANTI_ENUMERATION_FLOOR`.)
#[derive(Clone)]
pub struct InlineSend {
    store: Arc<dyn OutboxStore>,
    notifier: Arc<dyn Notifier>,
    batch_size: u32,
    budget: Duration,
}

impl InlineSend {
    /// Build an inline sender over the outbox `store` and the real `notifier`
    /// (the mailer, NOT the enqueueing notifier the auth flows use).
    #[must_use]
    pub fn new(store: Arc<dyn OutboxStore>, notifier: Arc<dyn Notifier>) -> Self {
        Self {
            store,
            notifier,
            batch_size: DEFAULT_INLINE_BATCH_SIZE,
            budget: Duration::from_millis(DEFAULT_INLINE_BUDGET_MS),
        }
    }

    /// Cap how many messages one inline pass claims (clamped to `1..=500`).
    #[must_use]
    pub fn with_batch_size(mut self, batch_size: u32) -> Self {
        self.batch_size = batch_size.clamp(1, MAX_DRAIN_BATCH_SIZE);
        self
    }

    /// Set the time budget (and fixed phase duration).
    #[must_use]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// Attempt delivery, never failing, and always taking exactly `budget`.
    ///
    /// A zero budget disables the inline send entirely (nothing is claimed and
    /// no padding is applied).
    ///
    /// The pass runs on its own spawned task and only the wait on its
    /// `JoinHandle` is bounded by the deadline. Dropping `drain_once` mid-flight
    /// would cancel it between "SMTP accepted" and `mark_sent` (the row would be
    /// re-sent after the lease expires) or strand claimed-but-untried rows. On
    /// timeout the task simply keeps running and records its outcome. On Cloud
    /// Run CPU is throttled after the response, so that tail may finish late
    /// (when the instance next gets CPU); that is acceptable because it is no
    /// longer cancelled mid-statement. The inner budget is half the outer one so
    /// the pass stops starting new sends well before the deadline.
    pub async fn run_padded(&self) {
        if self.budget.is_zero() {
            return;
        }
        let deadline = tokio::time::Instant::now() + self.budget;
        let store = self.store.clone();
        let notifier = self.notifier.clone();
        let (batch_size, inner) = (self.batch_size, self.budget / 2);
        let pass = tokio::spawn(async move {
            drain_once(store.as_ref(), notifier.as_ref(), batch_size, inner).await
        });
        match tokio::time::timeout_at(deadline, pass).await {
            Ok(Ok(Ok(report))) => {
                if report.failed > 0 || report.dead_lettered > 0 {
                    tracing::warn!(
                        sent = report.sent,
                        failed = report.failed,
                        dead_lettered = report.dead_lettered,
                        "inline outbox send: some messages left for the scheduler retry"
                    );
                } else if report.claimed > 0 {
                    tracing::debug!(sent = report.sent, "inline outbox send delivered");
                }
            }
            Ok(Ok(Err(_))) => tracing::warn!("inline outbox send failed: store error"),
            Ok(Err(_)) => tracing::warn!("inline outbox send task aborted"),
            Err(_) => tracing::warn!("inline outbox send still running at deadline; detached"),
        }
        tokio::time::sleep_until(deadline).await;
    }
}

/// `POST /internal/outbox/drain`.
async fn drain_handler(State(state): State<OutboxApiState>, headers: HeaderMap) -> Response {
    if let Err(rejection) = state.auth.authorize(&headers).await {
        return rejection.into_response();
    }

    match drain_once(
        state.store.as_ref(),
        state.notifier.as_ref(),
        state.batch_size,
        state.budget,
    )
    .await
    {
        Ok(report) => {
            // One concise line, and only when there was something to do: an
            // empty sweep every 15 minutes is noise against the log quota.
            if report.claimed == 0 {
                tracing::debug!("outbox drain: nothing due");
            } else {
                tracing::info!(
                    claimed = report.claimed,
                    sent = report.sent,
                    failed = report.failed,
                    dead_lettered = report.dead_lettered,
                    budget_exhausted = report.budget_exhausted,
                    "outbox drain"
                );
            }
            (StatusCode::OK, Json(report)).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "outbox drain failed" })),
        )
            .into_response(),
    }
}

/// Build the internal-outbox router. Unthrottled (its only caller is a
/// scheduler on a fixed cadence) and deliberately absent from the OpenAPI
/// document - merge this onto [`crate::api_router`]'s result, exactly like
/// [`crate::health_router`].
pub fn outbox_router(state: OutboxApiState) -> Router {
    Router::new()
        .route("/internal/outbox/drain", post(drain_handler))
        .with_state(state)
}

/// Current Unix time in seconds, saturating at 0 before the epoch.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(value).expect("valid header"),
        );
        headers
    }

    #[test]
    fn ct_eq_matches_equality_semantics() {
        let a = Sha256::digest(b"one").into();
        let b: [u8; 32] = Sha256::digest(b"one").into();
        let c: [u8; 32] = Sha256::digest(b"two").into();
        assert!(ct_eq(&a, &b));
        assert!(!ct_eq(&a, &c));
        // A digest differing only in its LAST byte must still compare false -
        // the fold visits every byte.
        let mut nearly = b;
        nearly[31] ^= 0x01;
        assert!(!ct_eq(&a, &nearly));
    }

    #[test]
    fn bearer_token_is_scheme_insensitive_and_trimmed() {
        assert_eq!(bearer_token(&headers_with("Bearer abc")), Some("abc"));
        assert_eq!(bearer_token(&headers_with("bearer   abc  ")), Some("abc"));
        assert_eq!(bearer_token(&headers_with("BEARER abc")), Some("abc"));
    }

    #[test]
    fn non_bearer_or_empty_authorization_yields_none() {
        assert_eq!(bearer_token(&headers_with("Basic abc")), None);
        assert_eq!(bearer_token(&headers_with("Bearer   ")), None);
        assert_eq!(bearer_token(&headers_with("Bearer")), None);
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn unconfigured_auth_is_disabled_even_with_a_credential() {
        let auth = DrainAuth::disabled();
        assert!(!auth.is_configured());
        assert_eq!(
            auth.authorize(&headers_with("Bearer anything")).await,
            Err(DrainAuthError::Disabled)
        );
    }

    #[tokio::test]
    async fn static_token_accepts_the_exact_secret_only() {
        let auth = DrainAuth::disabled().with_static_token("s3cret");
        assert!(auth.authorize(&headers_with("Bearer s3cret")).await.is_ok());
        assert_eq!(
            auth.authorize(&headers_with("Bearer s3cre")).await,
            Err(DrainAuthError::Unauthenticated)
        );
        assert_eq!(
            auth.authorize(&HeaderMap::new()).await,
            Err(DrainAuthError::Missing)
        );
    }

    /// A blank `AMICU_INTERNAL_TOKEN` must not switch the endpoint on with a
    /// secret every caller can guess.
    #[test]
    fn blank_static_token_is_treated_as_unconfigured() {
        assert!(
            !DrainAuth::disabled()
                .with_static_token("   ")
                .is_configured()
        );
        assert!(!DrainAuth::disabled().with_static_token("").is_configured());
    }

    /// Each rejection maps to exactly one status: a credential we could not
    /// verify is `401`, an identity we verified but do not accept is `403`, and
    /// an unconfigured endpoint is `503`. The 403 case is only reachable through
    /// the OIDC path, whose claim checking is unit-tested in `amicu-auth`'s
    /// `google` module against a locally-signed token; this pins the mapping.
    #[test]
    fn each_rejection_maps_to_its_own_status() {
        for (rejection, expected) in [
            (DrainAuthError::Disabled, StatusCode::SERVICE_UNAVAILABLE),
            (DrainAuthError::Missing, StatusCode::UNAUTHORIZED),
            (DrainAuthError::Unauthenticated, StatusCode::UNAUTHORIZED),
            (DrainAuthError::ForbiddenCaller, StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                rejection.into_response().status(),
                expected,
                "{rejection:?}"
            );
        }
    }

    /// `Debug` must never render the configured secret or its digest.
    #[test]
    fn debug_never_renders_the_secret() {
        let rendered = format!("{:?}", DrainAuth::disabled().with_static_token("s3cret"));
        assert!(!rendered.contains("s3cret"), "{rendered}");
        assert!(rendered.contains("static_token: true"), "{rendered}");
    }
}
