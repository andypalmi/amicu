//! HTTP transport seam between provider adapters and the network.
//!
//! Adapters build [`HttpRequest`]s and interpret [`HttpResponse`] status codes;
//! they never touch a concrete HTTP client. [`ReqwestTransport`] is the real
//! `reqwest`-backed implementation used in production; tests inject a mock
//! implementing [`HttpTransport`] so the provisioning path is exercised with no
//! network, credentials, or spend.
//!
//! The split is deliberate: a [`TransportError`] means the exchange never
//! completed (timeout, connection/TLS failure) and is transient; any HTTP
//! response - including `4xx`/`5xx` - is returned as an [`HttpResponse`] for the
//! adapter to map onto a typed [`ProviderError`](crate::provider::ProviderError).

use async_trait::async_trait;
use serde::de::DeserializeOwned;

/// The HTTP method of a request. Only the verbs the provider REST APIs use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Patch,
    Delete,
}

/// A transport-agnostic HTTP request.
///
/// `bearer` carries the resolved credential for `Authorization: Bearer`;
/// `headers` carries any other auth headers a provider needs (Scaleway's
/// `X-Auth-Token`, OVH's signature). Both are set transiently by the adapter for
/// a single call and are never logged or serialized (this struct derives no
/// `Serialize`/`Debug` that would expose them).
pub struct HttpRequest {
    /// The HTTP method.
    pub method: HttpMethod,
    /// The absolute request URL.
    pub url: String,
    /// Optional bearer token for the `Authorization` header.
    pub bearer: Option<String>,
    /// Extra request headers, e.g. a non-bearer auth header. Carries credential
    /// material transiently, like `bearer`; never logged or serialized.
    pub headers: Vec<(String, String)>,
    /// Optional JSON request body, already serialized.
    pub body: Option<serde_json::Value>,
}

impl HttpRequest {
    /// A request with the given method and URL and no auth or body.
    #[must_use]
    pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            bearer: None,
            headers: Vec::new(),
            body: None,
        }
    }

    /// Attach a bearer token.
    #[must_use]
    pub fn with_bearer(mut self, token: impl Into<String>) -> Self {
        self.bearer = Some(token.into());
        self
    }

    /// Attach an extra request header (e.g. a non-bearer auth header).
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Attach a JSON body.
    #[must_use]
    pub fn with_json(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }
}

/// A completed HTTP response: the status code and raw body bytes.
///
/// A response is returned for every completed exchange, `2xx` or not - the
/// adapter inspects [`status`](Self::status) to decide success or which
/// [`ProviderError`](crate::provider::ProviderError) applies.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The raw response body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A response with the given status and body.
    #[must_use]
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// Whether the status is in the `2xx` success range.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Deserialize the body as JSON.
    ///
    /// # Errors
    /// [`serde_json::Error`] when the body is not valid JSON for `T`.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

/// A transport-level failure: the HTTP exchange did not complete, so there is no
/// status to interpret. Both variants are transient (the adapter maps them to a
/// retryable backend error).
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The request timed out before a response was received.
    #[error("request timed out")]
    Timeout,
    /// The connection failed (DNS, TLS, refused) or the body could not be read.
    #[error("transport connection error: {0}")]
    Connection(String),
}

/// The seam an adapter calls to perform an HTTP exchange.
///
/// Implemented by [`ReqwestTransport`] in production and by test mocks. Kept
/// object-safe so an adapter can hold `Arc<dyn HttpTransport>`.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Execute `request` and return the response.
    ///
    /// # Errors
    /// [`TransportError`] when the exchange does not complete. A completed
    /// exchange is an `Ok(HttpResponse)` regardless of status code.
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;

    /// Like [`execute`](Self::execute), for a body that may never end (a
    /// Server-Sent Events stream): reading stops at EOF or at the first of
    /// `limits`, and what arrived so far is the body. The default delegates to
    /// [`execute`](Self::execute), which suits canned test transports.
    ///
    /// # Errors
    /// [`TransportError`] when no response arrives.
    async fn execute_bounded(
        &self,
        request: HttpRequest,
        limits: StreamLimits,
    ) -> Result<HttpResponse, TransportError> {
        let _ = limits;
        self.execute(request).await
    }
}

/// When [`HttpTransport::execute_bounded`] stops reading a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamLimits {
    /// Overall time from the request until reading stops.
    pub total: std::time::Duration,
    /// Stop once no data arrived for this long.
    pub idle: std::time::Duration,
    /// Stop once this many body bytes arrived (the body is cut there).
    pub max_bytes: usize,
}

/// The production [`HttpTransport`], backed by a `reqwest` client.
///
/// TLS is rustls (no system OpenSSL) for a hermetic build. Constructing this
/// does no network I/O; it is safe to build eagerly in provider constructors
/// that only ever read capabilities.
pub struct ReqwestTransport {
    client: reqwest::Client,
    timeout: std::time::Duration,
}

impl ReqwestTransport {
    /// A transport with the default request timeout (30s).
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(std::time::Duration::from_secs(30))
    }

    /// A transport with an explicit request timeout.
    #[must_use]
    pub fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            client: reqwest::Client::new(),
            timeout,
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestTransport {
    fn builder(&self, request: &HttpRequest) -> reqwest::RequestBuilder {
        let method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Delete => reqwest::Method::DELETE,
        };
        let mut builder = self.client.request(method, &request.url);
        if let Some(token) = &request.bearer {
            builder = builder.bearer_auth(token);
        }
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &request.body {
            builder = builder.json(body);
        }
        builder
    }
}

/// reqwest error Display carries the URL and cause, never the Authorization
/// header, so it is safe to surface.
fn send_error(e: &reqwest::Error) -> TransportError {
    if e.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Connection(e.to_string())
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let response = self
            .builder(&request)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| send_error(&e))?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|e| TransportError::Connection(e.to_string()))?
            .to_vec();
        Ok(HttpResponse { status, body })
    }

    async fn execute_bounded(
        &self,
        request: HttpRequest,
        limits: StreamLimits,
    ) -> Result<HttpResponse, TransportError> {
        let deadline = tokio::time::Instant::now() + limits.total;
        let mut response = tokio::time::timeout_at(deadline, self.builder(&request).send())
            .await
            .map_err(|_| TransportError::Timeout)?
            .map_err(|e| send_error(&e))?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        loop {
            let until = deadline.min(tokio::time::Instant::now() + limits.idle);
            match tokio::time::timeout_at(until, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    let room = limits.max_bytes.saturating_sub(body.len());
                    body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    if body.len() >= limits.max_bytes {
                        break;
                    }
                }
                Ok(Err(e)) if body.is_empty() => {
                    return Err(TransportError::Connection(e.to_string()));
                }
                Ok(Ok(None) | Err(_)) | Err(_) => break,
            }
        }
        Ok(HttpResponse { status, body })
    }
}

/// A canned response registered on a [`MockTransport`], keyed by method and
/// URL suffix.
#[cfg(test)]
type CannedResponse = (
    HttpMethod,
    String,
    bool,
    Result<HttpResponse, TransportError>,
);

/// A test [`HttpTransport`] returning canned responses keyed by
/// `(method, url-suffix)` and recording the requests it saw, so adapter tests
/// drive the provisioning path with no network, credentials, or spend.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockTransport {
    responses: std::sync::Mutex<Vec<CannedResponse>>,
    seen: std::sync::Mutex<Vec<SeenRequest>>,
}

/// A request a [`MockTransport`] observed: method, URL, the bearer token (if
/// any), and the extra headers. The auth values are recorded so tests can assert
/// auth without leaking it elsewhere.
#[cfg(test)]
pub(crate) struct SeenRequest {
    pub method: HttpMethod,
    pub url: String,
    pub bearer: Option<String>,
    pub headers: Vec<(String, String)>,
}

#[cfg(test)]
impl MockTransport {
    /// Register a canned successful response for a `(method, url-suffix)` match.
    pub(crate) fn on(self, method: HttpMethod, suffix: &str, response: HttpResponse) -> Self {
        self.responses
            .lock()
            .unwrap()
            .push((method, suffix.into(), false, Ok(response)));
        self
    }

    /// Register a canned successful response for a `(method, url-substring)`
    /// match (for URLs whose tail varies, e.g. a query with a random name).
    pub(crate) fn on_contains(
        self,
        method: HttpMethod,
        needle: &str,
        response: HttpResponse,
    ) -> Self {
        self.responses
            .lock()
            .unwrap()
            .push((method, needle.into(), true, Ok(response)));
        self
    }

    /// Register a canned transport failure for a `(method, url-suffix)` match.
    pub(crate) fn on_error(self, method: HttpMethod, suffix: &str, error: TransportError) -> Self {
        self.responses
            .lock()
            .unwrap()
            .push((method, suffix.into(), false, Err(error)));
        self
    }

    /// The requests this mock has seen, in order.
    pub(crate) fn requests(&self) -> std::sync::MutexGuard<'_, Vec<SeenRequest>> {
        self.seen.lock().unwrap()
    }
}

#[cfg(test)]
#[async_trait]
impl HttpTransport for MockTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        self.seen.lock().unwrap().push(SeenRequest {
            method: request.method,
            url: request.url.clone(),
            bearer: request.bearer.clone(),
            headers: request.headers.clone(),
        });
        let responses = self.responses.lock().unwrap();
        let hit = responses.iter().find(|(m, needle, contains, _)| {
            *m == request.method
                && if *contains {
                    request.url.contains(needle.as_str())
                } else {
                    request.url.ends_with(needle.as_str())
                }
        });
        match hit {
            Some((_, _, _, Ok(r))) => Ok(r.clone()),
            Some((_, _, _, Err(TransportError::Timeout))) => Err(TransportError::Timeout),
            Some((_, _, _, Err(TransportError::Connection(m)))) => {
                Err(TransportError::Connection(m.clone()))
            }
            None => Ok(HttpResponse::new(404, b"no mock".to_vec())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Data {
        value: String,
    }

    #[test]
    fn response_success_and_json() {
        let ok = HttpResponse::new(200, br#"{"value":"hi"}"#.to_vec());
        assert!(ok.is_success());
        let d: Data = ok.json().unwrap();
        assert_eq!(d.value, "hi");
        assert!(!HttpResponse::new(500, Vec::new()).is_success());
    }

    #[tokio::test]
    async fn mock_matches_on_method_and_suffix_and_records_bearer() {
        let mock = MockTransport::default().on(
            HttpMethod::Post,
            "/pods",
            HttpResponse::new(201, br#"{"id":"pod-1"}"#.to_vec()),
        );
        let req = HttpRequest::new(HttpMethod::Post, "https://api.example.com/v1/pods")
            .with_bearer("tok");
        let resp = mock.execute(req).await.unwrap();
        assert_eq!(resp.status, 201);
        let seen = mock.requests();
        assert_eq!(seen[0].method, HttpMethod::Post);
        assert_eq!(seen[0].bearer.as_deref(), Some("tok"));
    }

    /// Answers one request with `head` and `body`, then holds the connection
    /// open without closing it (an event stream that never ends).
    async fn serve_open_stream(head: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = sock.read(&mut buf).await;
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body.as_bytes()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        url
    }

    const STREAM_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";

    #[tokio::test]
    async fn bounded_read_returns_what_arrived_once_the_stream_goes_idle() {
        let url = serve_open_stream(STREAM_HEAD, "data: a\n\ndata: b\n\n").await;
        let limits = StreamLimits {
            total: std::time::Duration::from_secs(10),
            idle: std::time::Duration::from_millis(200),
            max_bytes: 1024,
        };
        let started = std::time::Instant::now();
        let resp = ReqwestTransport::new()
            .execute_bounded(HttpRequest::new(HttpMethod::Get, url), limits)
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"data: a\n\ndata: b\n\n");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn bounded_read_cuts_the_body_at_max_bytes() {
        let url = serve_open_stream(STREAM_HEAD, "0123456789").await;
        let limits = StreamLimits {
            total: std::time::Duration::from_secs(10),
            idle: std::time::Duration::from_secs(5),
            max_bytes: 4,
        };
        let resp = ReqwestTransport::new()
            .execute_bounded(HttpRequest::new(HttpMethod::Get, url), limits)
            .await
            .unwrap();
        assert_eq!(resp.body, b"0123");
    }

    #[tokio::test]
    async fn mock_unmatched_request_is_404() {
        let resp = MockTransport::default()
            .execute(HttpRequest::new(HttpMethod::Get, "https://x/none"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }
}
