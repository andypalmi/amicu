//! JSON REST client for Google Cloud APIs on the [`HttpTransport`] seam: a
//! bearer token from a [`TokenSource`] on every call, and Google's error JSON
//! (`{"error": {"code", "status", "message", "errors": [{"reason"}]}}`) mapped
//! onto [`GcpError`]. The token is never `Debug`-printed or put in an error.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use amicu_providers::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport, TransportError};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::auth::TokenSource;
use crate::error::GcpError;

/// Compute Engine API root.
pub const COMPUTE_BASE_URL: &str = "https://compute.googleapis.com/compute/v1";
/// Cloud Run Admin API (v2) root.
pub const RUN_BASE_URL: &str = "https://run.googleapis.com/v2";

/// The longest Google error message kept in a [`GcpError`].
const MAX_MESSAGE: usize = 300;

/// Reasons and statuses Google uses for hard quotas.
const QUOTA_REASONS: [&str; 2] = ["quotaExceeded", "RESOURCE_EXHAUSTED"];

/// Reasons Google uses for rate limiting. A `429` is also rate limiting.
const RATE_LIMIT_REASONS: [&str; 3] = [
    "rateLimitExceeded",
    "userRateLimitExceeded",
    "RATE_LIMIT_EXCEEDED",
];

pub struct GcpClient {
    transport: Arc<dyn HttpTransport>,
    tokens: Arc<dyn TokenSource>,
}

impl fmt::Debug for GcpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcpClient").finish_non_exhaustive()
    }
}

impl GcpClient {
    /// A client over the `reqwest` transport.
    #[must_use]
    pub fn new(tokens: Arc<dyn TokenSource>) -> Self {
        Self::with_transport(tokens, Arc::new(ReqwestTransport::new()))
    }

    /// A client over an explicit transport (tests inject a mock).
    #[must_use]
    pub fn with_transport(tokens: Arc<dyn TokenSource>, transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport, tokens }
    }

    /// Sends `method url` with an optional JSON `body` and returns the JSON
    /// response (`null` for an empty `2xx` body, as a `DELETE` may return).
    ///
    /// A transport failure on a `POST` is a plain [`GcpError::Transport`]: the
    /// request may have been applied, so create `POST`s must not be retried
    /// automatically on it. Callers look the resource up before retrying a
    /// create. (`GET`s, `DELETE`s and operation polls are safe to retry.)
    ///
    /// # Errors
    /// A token failure, [`GcpError::Transport`], or the mapped Google error:
    /// [`GcpError::NotFound`] for `404`, [`GcpError::QuotaExceeded`] for hard quota,
    /// [`GcpError::RateLimited`] for rate limits, and so on.
    pub async fn request(
        &self,
        method: HttpMethod,
        url: &str,
        body: Option<Value>,
    ) -> Result<Value, GcpError> {
        let token = self.tokens.token().await?;
        let mut req = HttpRequest::new(method, url).with_bearer(token.expose());
        let body = match (body, method) {
            (None, HttpMethod::Post | HttpMethod::Patch) => {
                Some(Value::Object(serde_json::Map::new()))
            }
            (body, _) => body,
        };
        if let Some(body) = body {
            req = req.with_json(body);
        }
        let resp = self.transport.execute(req).await.map_err(|e| {
            GcpError::Transport(match e {
                TransportError::Timeout => "request timed out".to_owned(),
                TransportError::Connection(m) => m,
            })
        })?;
        if !resp.is_success() {
            return Err(map_error(resp.status, &resp.body, resp.retry_after, url));
        }
        if resp.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        resp.json().map_err(|e| GcpError::Decode(e.to_string()))
    }

    /// Like [`request`](Self::request), decoding the response into `T`.
    ///
    /// # Errors
    /// As [`request`](Self::request), plus [`GcpError::Decode`] when the body
    /// does not match `T`.
    pub async fn request_as<T: DeserializeOwned>(
        &self,
        method: HttpMethod,
        url: &str,
        body: Option<Value>,
    ) -> Result<T, GcpError> {
        let value = self.request(method, url, body).await?;
        serde_json::from_value(value).map_err(|e| GcpError::Decode(e.to_string()))
    }
}

/// Reasons Google uses when an API is not enabled in the project.
const DISABLED_REASONS: [&str; 3] = ["SERVICE_DISABLED", "accessNotConfigured", "API_DISABLED"];

/// The API a request URL addresses: its host (`secretmanager.googleapis.com`).
fn service_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default()
        .to_owned()
}

/// Maps a non-`2xx` response onto [`GcpError`]. `url` only names the resource
/// in a `404`.
fn map_error(status: u16, body: &[u8], retry_after: Option<Duration>, url: &str) -> GcpError {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let err = &parsed["error"];
    let code = err["status"]
        .as_str()
        .map_or_else(|| status.to_string(), str::to_owned);
    let message: String = err["message"]
        .as_str()
        .unwrap_or("no message")
        .chars()
        .take(MAX_MESSAGE)
        .collect();
    let reasons: Vec<&str> = err["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["reason"].as_str())
        .chain(
            err["details"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d["reason"].as_str()),
        )
        .chain(std::iter::once(code.as_str()))
        .collect();
    // Rate limiting wins over a quota reason: a `429` is always worth retrying.
    if status == 429 || reasons.iter().any(|r| RATE_LIMIT_REASONS.contains(r)) {
        return GcpError::RateLimited {
            message,
            retry_after,
        };
    }
    if reasons.iter().any(|r| QUOTA_REASONS.contains(r)) {
        return GcpError::QuotaExceeded { message };
    }
    if status == 403 && reasons.iter().any(|r| DISABLED_REASONS.contains(r)) {
        return GcpError::ServiceDisabled {
            service: service_of(url),
            message,
        };
    }
    match status {
        401 | 403 => GcpError::PermissionDenied {
            status,
            code,
            message,
        },
        404 => GcpError::NotFound {
            what: url.to_owned(),
        },
        409 => GcpError::Conflict { message },
        400 | 422 => GcpError::BadRequest {
            status,
            code,
            message,
        },
        _ => GcpError::Api {
            status,
            code,
            message,
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use amicu_providers::HttpResponse;
    use async_trait::async_trait;

    use super::*;
    use crate::auth::fake::FakeTokens;

    type Route = (
        HttpMethod,
        String,
        Vec<Result<HttpResponse, TransportError>>,
    );

    #[derive(Clone)]
    pub(crate) struct Seen {
        pub(crate) method: HttpMethod,
        pub(crate) url: String,
        pub(crate) bearer: Option<String>,
        pub(crate) body: Option<Value>,
    }

    /// Canned responses served in order per `(method, url-suffix)` route (the
    /// last one repeats); records every request.
    #[derive(Default)]
    pub(crate) struct Mock {
        routes: Mutex<Vec<Route>>,
        seen: Mutex<Vec<Seen>>,
    }

    impl Mock {
        pub(crate) fn on(self, method: HttpMethod, suffix: &str, status: u16, body: &str) -> Self {
            self.then(method, suffix, Ok(HttpResponse::new(status, body)))
        }

        pub(crate) fn then(
            self,
            method: HttpMethod,
            suffix: &str,
            r: Result<HttpResponse, TransportError>,
        ) -> Self {
            let mut routes = self.routes.lock().unwrap();
            match routes
                .iter_mut()
                .find(|(m, s, _)| *m == method && s == suffix)
            {
                Some((_, _, queue)) => queue.push(r),
                None => routes.push((method, suffix.to_owned(), vec![r])),
            }
            drop(routes);
            self
        }

        pub(crate) fn requests(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HttpTransport for Mock {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.seen.lock().unwrap().push(Seen {
                method: request.method,
                url: request.url.clone(),
                bearer: request.bearer.clone(),
                body: request.body.clone(),
            });
            let mut routes = self.routes.lock().unwrap();
            let hit = routes
                .iter_mut()
                .find(|(m, s, _)| *m == request.method && request.url.ends_with(s.as_str()));
            let Some((_, _, queue)) = hit else {
                return Ok(HttpResponse::new(500, "unrouted"));
            };
            let next = if queue.len() > 1 {
                queue.remove(0)
            } else {
                match &queue[0] {
                    Ok(r) => Ok(r.clone()),
                    Err(TransportError::Timeout) => Err(TransportError::Timeout),
                    Err(TransportError::Connection(m)) => {
                        Err(TransportError::Connection(m.clone()))
                    }
                }
            };
            drop(routes);
            next
        }
    }

    pub(crate) const TOKEN: &str = "ya29.SECRET-TOKEN";

    pub(crate) fn client(mock: Mock) -> (GcpClient, Arc<Mock>) {
        let mock = Arc::new(mock);
        let c = GcpClient::with_transport(Arc::new(FakeTokens(Ok(TOKEN))), mock.clone());
        (c, mock)
    }

    const URL: &str = "https://compute.googleapis.com/compute/v1/projects/p/zones/z/instances/i";

    #[tokio::test]
    async fn attaches_the_bearer_token_and_json_body() {
        let (c, mock) =
            client(Mock::default().on(HttpMethod::Post, "/instances", 200, r#"{"name":"op-1"}"#));
        let v = c
            .request(
                HttpMethod::Post,
                "https://compute.googleapis.com/compute/v1/projects/p/zones/z/instances",
                Some(serde_json::json!({"name": "i"})),
            )
            .await
            .unwrap();
        assert_eq!(v["name"], "op-1");
        let seen = mock.requests();
        assert_eq!(seen[0].method, HttpMethod::Post);
        assert_eq!(seen[0].bearer.as_deref(), Some(TOKEN));
        assert_eq!(seen[0].body, Some(serde_json::json!({"name": "i"})));
    }

    #[tokio::test]
    async fn bodiless_post_and_patch_send_an_empty_json_object() {
        for method in [HttpMethod::Post, HttpMethod::Patch] {
            let (c, mock) = client(Mock::default().on(method, "/wait", 200, "{}"));
            c.request(method, &format!("{URL}/wait"), None)
                .await
                .unwrap();
            assert_eq!(mock.requests()[0].body, Some(serde_json::json!({})));
        }
    }

    #[tokio::test]
    async fn bodiless_get_and_delete_send_no_body() {
        for method in [HttpMethod::Get, HttpMethod::Delete] {
            let (c, mock) = client(Mock::default().on(method, "/i", 200, "{}"));
            c.request(method, URL, None).await.unwrap();
            assert_eq!(mock.requests()[0].body, None);
        }
    }

    #[tokio::test]
    async fn empty_success_body_is_null_and_request_as_decodes() {
        let (c, _) = client(Mock::default().on(HttpMethod::Delete, "/i", 204, ""));
        assert_eq!(
            c.request(HttpMethod::Delete, URL, None).await.unwrap(),
            Value::Null
        );

        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/i", 200, r#"{"name":"i"}"#));
        let v: std::collections::BTreeMap<String, String> =
            c.request_as(HttpMethod::Get, URL, None).await.unwrap();
        assert_eq!(v["name"], "i");

        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/i", 200, "not json"));
        assert!(matches!(
            c.request(HttpMethod::Get, URL, None).await,
            Err(GcpError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn token_failure_sends_nothing() {
        let mock = Arc::new(Mock::default());
        let c = GcpClient::with_transport(
            Arc::new(FakeTokens(Err(GcpError::MissingCredentials))),
            mock.clone(),
        );
        assert_eq!(
            c.request(HttpMethod::Get, URL, None).await,
            Err(GcpError::MissingCredentials)
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn transport_failure_is_transient() {
        let (c, _) =
            client(Mock::default().then(HttpMethod::Get, "/i", Err(TransportError::Timeout)));
        let e = c.request(HttpMethod::Get, URL, None).await.unwrap_err();
        assert!(matches!(e, GcpError::Transport(_)) && e.is_retryable());
    }

    async fn status_error(status: u16, body: &str) -> GcpError {
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/i", status, body));
        c.request(HttpMethod::Get, URL, None).await.unwrap_err()
    }

    #[tokio::test]
    async fn not_found_is_distinguishable_for_teardown() {
        let e = status_error(
            404,
            r#"{"error":{"code":404,"status":"NOT_FOUND","message":"gone"}}"#,
        )
        .await;
        assert!(e.is_not_found());
        assert_eq!(
            e,
            GcpError::NotFound {
                what: URL.to_owned()
            }
        );
    }

    #[tokio::test]
    async fn quota_errors_are_distinguishable() {
        // Compute: 403 with a quotaExceeded reason.
        let e = status_error(
            403,
            r#"{"error":{"code":403,"message":"Quota 'GPUS' exceeded.","errors":[{"reason":"quotaExceeded"}]}}"#,
        )
        .await;
        assert!(e.is_quota() && !e.is_retryable(), "{e:?}");
        // gRPC style RESOURCE_EXHAUSTED on a non-429 status is a hard quota.
        let e = status_error(
            403,
            r#"{"error":{"code":403,"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#,
        )
        .await;
        assert!(e.is_quota() && !e.is_retryable(), "{e:?}");
        // A 429 is rate limiting, whatever its status string.
        let e = status_error(
            429,
            r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#,
        )
        .await;
        assert!(
            e.is_rate_limited() && !e.is_quota() && e.is_retryable(),
            "{e:?}"
        );
        let e = status_error(
            403,
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"nope","details":[{"reason":"RATE_LIMIT_EXCEEDED"}]}}"#,
        )
        .await;
        assert!(e.is_rate_limited() && e.is_retryable(), "{e:?}");
        let e = status_error(
            403,
            r#"{"error":{"code":403,"message":"slow down","errors":[{"reason":"rateLimitExceeded"}]}}"#,
        )
        .await;
        assert!(e.is_rate_limited() && e.is_retryable(), "{e:?}");
        // A plain 403 is a permission problem, not a quota.
        let e = status_error(
            403,
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no access"}}"#,
        )
        .await;
        assert_eq!(
            e,
            GcpError::PermissionDenied {
                status: 403,
                code: "PERMISSION_DENIED".into(),
                message: "no access".into()
            }
        );
    }

    #[tokio::test]
    async fn a_disabled_api_is_not_a_permission_problem() {
        let e = status_error(
            403,
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"Secret Manager API has not been used in project 1 before or it is disabled.","details":[{"reason":"SERVICE_DISABLED"}]}}"#,
        )
        .await;
        assert!(e.is_service_disabled(), "{e:?}");
        assert!(
            e.to_string()
                .contains("gcloud services enable compute.googleapis.com"),
            "{e}"
        );
        let e = status_error(
            403,
            r#"{"error":{"code":403,"message":"x","errors":[{"reason":"accessNotConfigured"}]}}"#,
        )
        .await;
        assert!(e.is_service_disabled(), "{e:?}");
    }

    #[tokio::test]
    async fn other_statuses_map_to_typed_errors() {
        let e = status_error(
            409,
            r#"{"error":{"code":409,"status":"ALREADY_EXISTS","message":"exists"}}"#,
        )
        .await;
        assert_eq!(
            e,
            GcpError::Conflict {
                message: "exists".into()
            }
        );
        let e = status_error(
            400,
            r#"{"error":{"code":400,"status":"INVALID_ARGUMENT","message":"bad"}}"#,
        )
        .await;
        assert!(matches!(e, GcpError::BadRequest { status: 400, .. }));
        let e = status_error(503, "<html>upstream</html>").await;
        assert_eq!(
            e,
            GcpError::Api {
                status: 503,
                code: "503".into(),
                message: "no message".into()
            }
        );
        assert!(e.is_retryable());
        let long = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(1000));
        let GcpError::Api { message, .. } = status_error(500, &long).await else {
            panic!("expected Api");
        };
        assert_eq!(message.len(), MAX_MESSAGE);
    }

    #[tokio::test]
    async fn the_token_never_appears_in_debug_or_errors() {
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            "/i",
            401,
            r#"{"error":{"message":"bad"}}"#,
        ));
        let e = c.request(HttpMethod::Get, URL, None).await.unwrap_err();
        for s in [format!("{c:?}"), format!("{e:?}"), e.to_string()] {
            assert!(!s.contains("SECRET-TOKEN"), "{s}");
        }
    }
}
