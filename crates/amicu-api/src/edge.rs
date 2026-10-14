//! Cloudflare edge trust.
//!
//! On the target Cloud Run topology, the request chain is: browser ->
//! Cloudflare edge (proxied CNAME) -> the Cloudflare **api-proxy Worker**
//! (`amicu-infra/fallback/api-proxy-worker`, which rewrites `Host` and strips
//! hop-by-hop headers) -> Google's front end -> Cloud Run. The direct peer
//! Cloud Run sees is Google's own proxy, and `X-Forwarded-For` ends with
//! Cloudflare's egress IP - it never carries the real client IP. The real
//! client IP is in `CF-Connecting-IP`, set by the Cloudflare edge itself.
//!
//! `CF-Connecting-IP` is only as trustworthy as the hop that set it, though:
//! a request straight to the raw `*.run.app` URL (bypassing Cloudflare
//! entirely - Cloud Run's ingress is `ALL` by default) can set ANY
//! `CF-Connecting-IP` value it likes. The api-proxy Worker closes that gap by
//! attaching a shared-secret header, [`EDGE_TOKEN_HEADER`], to every request
//! it forwards (`env.EDGE_TOKEN`, provisioned via `wrangler secret put` -
//! never in Terraform state; see `amicu-infra`'s `AMICU_EDGE_TOKEN` secret
//! shell and docs/env-matrix.md). A raw `*.run.app` request can never produce
//! a matching token, since it never passed through the Worker.
//!
//! [`EdgeTrust`] is the policy this module centers on:
//! * [`EdgeTrust::token_valid`] - constant-time comparison against
//!   `AMICU_EDGE_TOKEN`.
//! * [`EdgeTrust::trusted_client_ip`] - `CF-Connecting-IP`, parsed, but ONLY
//!   when the token is valid; used by [`crate::auth`]'s rate limiter to key
//!   on the real client IP instead of the Cloud Run/Google LB peer address.
//! * [`edge_token_gate`] - optional (`AMICU_REQUIRE_EDGE_TOKEN`) global
//!   middleware that rejects (`403`, before any handler runs) a request
//!   without a valid token, closing the raw-`*.run.app`-bypass gap outright.
//!   Mount it over the whole API router, NOT over `GET /healthz` / `GET
//!   /readyz` (see [`crate::health`]) - Cloud Run's own liveness/readiness
//!   probes carry no such header and must never be throttled or blocked by
//!   it (see `amicu-server`'s `build_app_with_database_url`, which layers
//!   this before merging the health router).
//!
//! The token is never logged: [`EdgeTrust`] stores it as a
//! [`SecretValue`](amicu_core::secret::SecretValue) (redacted `Debug`,
//! zeroized on drop), and neither [`token_valid`](EdgeTrust::token_valid) nor
//! [`edge_token_gate`] ever echoes the presented or configured value back in
//! a response or error.

use std::net::IpAddr;

use amicu_core::secret::SecretValue;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Header the api-proxy Cloudflare Worker sets (`X-Amicu-Edge-Token`) on
/// every request it forwards. Compared case-insensitively by
/// [`axum::http::HeaderMap`] lookup, so the lowercase form here matches any
/// casing on the wire.
pub const EDGE_TOKEN_HEADER: &str = "x-amicu-edge-token";

/// Header the Cloudflare edge itself sets to the true client IP
/// (`CF-Connecting-IP`). Trusted ONLY alongside a valid [`EDGE_TOKEN_HEADER`]
/// - see [`EdgeTrust::trusted_client_ip`].
pub const CF_CONNECTING_IP_HEADER: &str = "cf-connecting-ip";

/// Configured edge-trust policy: the shared secret (`AMICU_EDGE_TOKEN`, if
/// any) and whether presenting it is required (`AMICU_REQUIRE_EDGE_TOKEN`).
///
/// The default (`token: None, require: false`) is a no-op: no request can
/// ever present a matching token, so `CF-Connecting-IP` is never trusted and
/// [`edge_token_gate`] never rejects anything - exactly the pre-#142 behavior,
/// for zero-config dev/self-host and any deployment that has not yet
/// provisioned the Worker/token pair.
#[derive(Clone, Default)]
pub struct EdgeTrust {
    /// `AMICU_EDGE_TOKEN`. Read once at boot (see `amicu-server`'s
    /// `Config`-adjacent env docs) - unlike
    /// `AMICU_REQUIRE_EMAIL_VERIFICATION`'s read-on-every-call convention,
    /// this is a secret best read once and held, not re-read from the
    /// environment on every request.
    token: Option<SecretValue>,
    /// `AMICU_REQUIRE_EDGE_TOKEN`.
    require: bool,
}

impl EdgeTrust {
    /// `token = None` disables edge trust entirely, regardless of `require`
    /// (see [`Self::require_without_token_is_a_config_error`], which the
    /// `amicu-server` boot sequence checks explicitly rather than silently
    /// degrading to "never valid, always 403").
    #[must_use]
    pub fn new(token: Option<SecretValue>, require: bool) -> Self {
        Self { token, require }
    }

    /// Whether requests are rejected outright when no valid token is
    /// presented (`AMICU_REQUIRE_EDGE_TOKEN`).
    #[must_use]
    pub fn requires_token(&self) -> bool {
        self.require
    }

    /// `true` if `require` is set but no token is configured - a deployment
    /// that would reject every public request unconditionally. `amicu-server`
    /// treats this as a hard startup error rather than booting a service that
    /// can never serve a request.
    #[must_use]
    pub fn require_without_token_is_a_config_error(&self) -> bool {
        self.require && self.token.is_none()
    }

    /// `true` if `headers` carries [`EDGE_TOKEN_HEADER`] equal to the
    /// configured token, compared in constant time via [`constant_time_eq`].
    /// Always `false` when no token is configured, or none is presented.
    #[must_use]
    pub fn token_valid(&self, headers: &HeaderMap) -> bool {
        let Some(configured) = self.token.as_ref() else {
            return false;
        };
        let Some(presented) = headers.get(EDGE_TOKEN_HEADER).and_then(|v| v.to_str().ok()) else {
            return false;
        };
        constant_time_eq(configured.expose_secret(), presented.as_bytes())
    }

    /// The rate-limit client IP for this request: [`CF_CONNECTING_IP_HEADER`],
    /// parsed, but ONLY when [`Self::token_valid`] - a request without a
    /// valid edge token can never have its client-supplied
    /// `CF-Connecting-IP` believed (it would otherwise be a trivial
    /// self-reported spoof), so this returns `None` and the caller falls back
    /// to the existing `AMICU_TRUSTED_PROXIES`/`X-Forwarded-For` peer-address
    /// logic (see [`crate::ratelimit::TrustedProxies::client_key_with_edge`]).
    #[must_use]
    pub fn trusted_client_ip(&self, headers: &HeaderMap) -> Option<IpAddr> {
        if !self.token_valid(headers) {
            return None;
        }
        headers
            .get(CF_CONNECTING_IP_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
    }
}

/// Constant-time byte-slice equality for the edge-token comparison: every
/// byte pair is compared regardless of an earlier mismatch (no early exit),
/// so response timing cannot leak how many leading bytes matched - the same
/// property [`crate::billing::verify_signature`] gets from `hmac`'s
/// `Mac::verify_slice` for the Stripe webhook signature. The length check
/// DOES short-circuit: a header's length is not itself confidential, only its
/// content is, so leaking "the lengths differ" costs nothing (this mirrors
/// `Mac::verify_slice`'s own behavior, which also checks length up front).
#[must_use]
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// State for [`edge_token_gate`].
#[derive(Clone, Default)]
pub struct EdgeGateState {
    /// The configured policy this gate enforces.
    pub trust: EdgeTrust,
}

/// Global middleware: when `AMICU_REQUIRE_EDGE_TOKEN=true`
/// ([`EdgeTrust::requires_token`]), reject (`403 Forbidden`, before any
/// handler runs or any body is read) a request whose `X-Amicu-Edge-Token` is
/// missing or does not match. A no-op - always calls through - when
/// `AMICU_REQUIRE_EDGE_TOKEN=false` (the default).
///
/// Mount this over the whole API router (see the module docs); health routes
/// are wired separately and must stay outside it, since Cloud Run's
/// liveness/readiness probes carry no such header.
pub async fn edge_token_gate(
    State(state): State<EdgeGateState>,
    request: Request,
    next: Next,
) -> Response {
    if state.trust.requires_token() && !state.trust.token_valid(request.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (k, v) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn constant_time_eq_matches_equal_slices() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
    }

    #[test]
    fn constant_time_eq_rejects_different_content_same_length() {
        assert!(!constant_time_eq(b"secret-token", b"secret-tokeN"));
    }

    #[test]
    fn constant_time_eq_rejects_different_length() {
        assert!(!constant_time_eq(b"short", b"much-longer-value"));
    }

    #[test]
    fn no_token_configured_never_validates() {
        let trust = EdgeTrust::new(None, false);
        let headers = headers_with(&[(EDGE_TOKEN_HEADER, "anything")]);
        assert!(!trust.token_valid(&headers));
        assert_eq!(trust.trusted_client_ip(&headers), None);
    }

    #[test]
    fn matching_token_validates() {
        let trust = EdgeTrust::new(Some(SecretValue::new("s3cr3t")), false);
        let headers = headers_with(&[(EDGE_TOKEN_HEADER, "s3cr3t")]);
        assert!(trust.token_valid(&headers));
    }

    #[test]
    fn mismatched_token_does_not_validate() {
        let trust = EdgeTrust::new(Some(SecretValue::new("s3cr3t")), false);
        let headers = headers_with(&[(EDGE_TOKEN_HEADER, "wrong")]);
        assert!(!trust.token_valid(&headers));
    }

    #[test]
    fn missing_token_header_does_not_validate() {
        let trust = EdgeTrust::new(Some(SecretValue::new("s3cr3t")), false);
        let headers = HeaderMap::new();
        assert!(!trust.token_valid(&headers));
    }

    #[test]
    fn cf_connecting_ip_trusted_only_with_valid_token() {
        let trust = EdgeTrust::new(Some(SecretValue::new("s3cr3t")), false);

        // Valid token: CF-Connecting-IP is trusted and parsed.
        let ok = headers_with(&[
            (EDGE_TOKEN_HEADER, "s3cr3t"),
            (CF_CONNECTING_IP_HEADER, "203.0.113.7"),
        ]);
        assert_eq!(
            trust.trusted_client_ip(&ok),
            Some("203.0.113.7".parse().unwrap())
        );

        // Missing/wrong token: CF-Connecting-IP is ignored even though present
        // (a client could otherwise self-report any IP it likes).
        let spoofed = headers_with(&[(CF_CONNECTING_IP_HEADER, "198.51.100.9")]);
        assert_eq!(trust.trusted_client_ip(&spoofed), None);

        let wrong_token = headers_with(&[
            (EDGE_TOKEN_HEADER, "nope"),
            (CF_CONNECTING_IP_HEADER, "198.51.100.9"),
        ]);
        assert_eq!(trust.trusted_client_ip(&wrong_token), None);
    }

    #[test]
    fn unparseable_cf_connecting_ip_yields_none_even_with_valid_token() {
        let trust = EdgeTrust::new(Some(SecretValue::new("s3cr3t")), false);
        let headers = headers_with(&[
            (EDGE_TOKEN_HEADER, "s3cr3t"),
            (CF_CONNECTING_IP_HEADER, "not-an-ip"),
        ]);
        assert_eq!(trust.trusted_client_ip(&headers), None);
    }

    #[test]
    fn require_without_token_is_flagged_as_a_config_error() {
        assert!(EdgeTrust::new(None, true).require_without_token_is_a_config_error());
        assert!(!EdgeTrust::new(None, false).require_without_token_is_a_config_error());
        assert!(
            !EdgeTrust::new(Some(SecretValue::new("s3cr3t")), true)
                .require_without_token_is_a_config_error()
        );
    }

    /// A trivial one-route app with [`edge_token_gate`] layered over it, driven
    /// via `tower::ServiceExt::oneshot` (the workspace's standard in-process
    /// middleware-testing pattern - see e.g. `crates/amicu-api/tests/http.rs`).
    fn gated_app(trust: EdgeTrust) -> axum::Router {
        use axum::middleware::from_fn_with_state;
        use axum::routing::get;

        axum::Router::new()
            .route("/", get(|| async { StatusCode::OK }))
            .layer(from_fn_with_state(EdgeGateState { trust }, edge_token_gate))
    }

    #[tokio::test]
    async fn gate_allows_through_when_not_required() {
        use tower::ServiceExt;

        let app = gated_app(EdgeTrust::new(Some(SecretValue::new("s3cr3t")), false));
        let request = Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn gate_rejects_missing_token_when_required() {
        use tower::ServiceExt;

        let app = gated_app(EdgeTrust::new(Some(SecretValue::new("s3cr3t")), true));
        let request = Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn gate_rejects_wrong_token_when_required() {
        use tower::ServiceExt;

        let app = gated_app(EdgeTrust::new(Some(SecretValue::new("s3cr3t")), true));
        let request = Request::builder()
            .uri("/")
            .header(EDGE_TOKEN_HEADER, "wrong")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn gate_allows_correct_token_when_required() {
        use tower::ServiceExt;

        let app = gated_app(EdgeTrust::new(Some(SecretValue::new("s3cr3t")), true));
        let request = Request::builder()
            .uri("/")
            .header(EDGE_TOKEN_HEADER, "s3cr3t")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(request).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
