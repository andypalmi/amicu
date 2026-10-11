//! API-wide security response headers (andypalmi/amicu-planning#194).
//!
//! [`security_headers`] is a middleware applied to the whole router (see
//! [`crate::api_router`]; `amicu-server` also wraps the health/outbox routes):
//!
//! * `X-Content-Type-Options: nosniff`
//! * `Strict-Transport-Security: max-age=63072000; includeSubDomains`
//! * `Content-Security-Policy: default-src 'none'; frame-ancestors 'none'` and
//!   `X-Frame-Options: DENY` (this is a JSON API; nothing should be framed or
//!   render active content)
//! * `Referrer-Policy: no-referrer`
//! * `Cache-Control: no-store`, unless the handler already set one or the path
//!   is a public, cacheable document (`/.well-known/*`, `/openapi.json`). This
//!   covers every credential/token-bearing route (`/signin`, `/token`,
//!   `/token/refresh`, `/recover`, `/reset`, ...).

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, STRICT_TRANSPORT_SECURITY,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::middleware::Next;
use axum::response::Response;

/// Middleware adding the security headers described in the module docs.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let cacheable_public = {
        let p = request.uri().path();
        p.starts_with("/.well-known/") || p == "/openapi.json"
    };
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(
        STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=63072000; includeSubDomains"),
    );
    h.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
    );
    h.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if !cacheable_public && !h.contains_key(CACHE_CONTROL) {
        h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
