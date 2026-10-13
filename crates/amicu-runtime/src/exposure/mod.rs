//! How a started endpoint is reachable, and the shared post-deploy check that
//! it is actually protected (amicu-planning#191).
//!
//! Every [`RuntimeHandle`](crate::RuntimeHandle) carries an
//! [`EndpointExposure`]: an adapter must say whether the endpoint it returns
//! is bound to loopback or reachable off-machine, and in the latter case which
//! gateway-level authentication (if any) sits in front of the runtime. There
//! is deliberately no default in the struct, so a new provider cannot forget.
//!
//! [`verify_endpoint`] is the provider-agnostic check `apply` runs on every
//! returned endpoint that is supposed to be key-protected:
//!
//! 1. An unauthenticated request (no `Authorization`, no gateway headers) to
//!    the OpenAI-compatible route must be refused (`401`/`403`). A `2xx` means
//!    the endpoint is open to anyone with its URL.
//! 2. When gateway headers are available (e.g. Modal's proxy token), a request
//!    carrying *only* those must also be refused: the runtime itself enforces
//!    the key, not just the gateway in front of it.
//! 3. A request with the `Bearer` key (plus the gateway headers) must succeed,
//!    proving the delivered key is the one the runtime is enforcing.
//!
//! Secret values enter only as [`SecretValue`]s, are sent as sensitive header
//! values, and never appear in an [`ExposureError`], a log, or `Debug`.

use std::time::Duration;

use amicu_core::secret::SecretValue;
use serde::{Deserialize, Serialize};

/// The OpenAI-compatible route the shared check probes. Authenticated by both
/// llama-server (`--api-key`) and vLLM (`--api-key`, `/v1/*`); llama-server's
/// `/health` is deliberately public, so it cannot be used to test the key.
pub const PROBE_PATH: &str = "/v1/models";

/// How an endpoint returned by an adapter is reachable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reach", rename_all = "snake_case")]
pub enum EndpointExposure {
    /// Bound to the loopback interface of this machine; not reachable
    /// off-host.
    Loopback,
    /// Reachable off-machine (any remote provider). Such an endpoint must be
    /// protected by the deployment's `api_key`.
    Public {
        /// Authentication a provider gateway enforces *in front of* the
        /// runtime, on top of the runtime's own API key.
        gateway: GatewayAuth,
    },
}

impl EndpointExposure {
    /// Whether the endpoint is reachable off this machine.
    #[must_use]
    pub fn is_public(&self) -> bool {
        matches!(self, Self::Public { .. })
    }
}

/// Provider-level authentication in front of a public endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayAuth {
    /// No gateway authentication: the runtime's API key is the only guard.
    None,
    /// Modal proxy auth: requests need the workspace `Modal-Key` /
    /// `Modal-Secret` Proxy Auth Token headers.
    ModalProxyToken,
}

/// A failed [`verify_endpoint`]. The text never contains a secret value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct ExposureError {
    detail: String,
}

impl ExposureError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    /// The failure description (status codes and routes only).
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Timing for the retry loops in [`verify_endpoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckTiming {
    /// How long to wait for a definite answer to an unauthenticated request.
    pub reject_timeout: Duration,
    /// How long to wait for the authenticated request to succeed (a model may
    /// still be loading and answering `503`).
    pub accept_timeout: Duration,
    /// Pause between attempts.
    pub interval: Duration,
}

impl CheckTiming {
    /// Timing for a freshly deployed remote endpoint (the provider already
    /// waited for the container to be ready).
    pub const PUBLIC: Self = Self {
        reject_timeout: Duration::from_mins(2),
        accept_timeout: Duration::from_mins(10),
        interval: Duration::from_secs(3),
    };

    /// Timing for a local endpoint: the service has just been started and may
    /// still be downloading and loading weights.
    pub const LOCAL: Self = Self {
        reject_timeout: Duration::from_mins(15),
        accept_timeout: Duration::from_mins(15),
        interval: Duration::from_secs(3),
    };
}

/// What to probe and with which credentials.
pub struct EndpointCheck<'a> {
    /// Base URL, e.g. `https://x.modal.direct` or `http://127.0.0.1:8000`.
    pub endpoint: &'a str,
    /// The route to probe; normally [`PROBE_PATH`].
    pub path: &'a str,
    /// The resolved endpoint API key to send as `Authorization: Bearer`.
    pub api_key: &'a SecretValue,
    /// Provider gateway headers (e.g. `Modal-Key`/`Modal-Secret`); empty when
    /// the gateway has none or they are not configured locally.
    pub gateway_headers: &'a [(String, SecretValue)],
    /// Retry timing.
    pub timing: CheckTiming,
}

impl std::fmt::Debug for EndpointCheck<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointCheck")
            .field("endpoint", &self.endpoint)
            .field("path", &self.path)
            .field("api_key", &"<redacted>")
            .field(
                "gateway_headers",
                &self
                    .gateway_headers
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// Why a key value cannot be delivered to a runtime. Never includes the value.
///
/// llama-server and vLLM both take a comma-separated list for `--api-key` /
/// the env mirror, and a header value cannot carry control characters or
/// spaces, so those are refused up front rather than silently changing what
/// the endpoint accepts.
///
/// # Errors
/// A description of what is wrong with the key (empty, not UTF-8, contains a
/// comma, whitespace or non-printable/non-ASCII characters).
pub fn validate_api_key_value(value: &SecretValue) -> Result<&str, String> {
    let key = value
        .expose_secret_str()
        .map_err(|_| "is not valid UTF-8".to_owned())?;
    if key.is_empty() {
        return Err("is empty".to_owned());
    }
    if key.contains(',') {
        return Err(
            "contains a comma (runtimes treat a comma as a list of several keys)".to_owned(),
        );
    }
    if !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("contains whitespace or non-printable/non-ASCII characters".to_owned());
    }
    Ok(key)
}

/// One request; `Some(status)` when a response arrived. `bearer` and
/// `gateway` are attached as sensitive headers.
async fn probe(
    client: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    gateway: &[(String, SecretValue)],
) -> Option<u16> {
    let mut request = client.get(url);
    for (name, value) in gateway {
        let mut header =
            reqwest::header::HeaderValue::from_str(value.expose_secret_str().ok()?).ok()?;
        header.set_sensitive(true);
        request = request.header(name.as_str(), header);
    }
    if let Some(key) = bearer {
        let mut header = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).ok()?;
        header.set_sensitive(true);
        request = request.header(reqwest::header::AUTHORIZATION, header);
    }
    Some(request.send().await.ok()?.status().as_u16())
}

fn client() -> Result<reqwest::Client, ExposureError> {
    reqwest::Client::builder()
        // The Bearer key must never be sent through an environment HTTP proxy.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| ExposureError::new("could not build an HTTP client for the exposure check"))
}

fn is_success(code: u16) -> bool {
    (200..300).contains(&code)
}

fn url_for(endpoint: &str, path: &str) -> String {
    format!(
        "{}/{}",
        endpoint.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn exposed(code: u16, path: &str, with_gateway: bool) -> ExposureError {
    if with_gateway {
        ExposureError::new(format!(
            "endpoint answered {path} with {code} when sent only the gateway credentials and no \
             API key; the runtime is not enforcing the API key"
        ))
    } else {
        ExposureError::new(format!(
            "endpoint answered an unauthenticated request to {path} ({code}); it is open to \
             anyone with its URL"
        ))
    }
}

/// Waits for a definite refusal of a request sent with `gateway` credentials
/// only (no API key): `401`/`403` is `Ok`, a `2xx` is an exposure, anything
/// else (connection refused, `404`, `5xx` while warming up) is retried until
/// `timeout`, then fails closed.
async fn require_refusal(
    client: &reqwest::Client,
    url: &str,
    path: &str,
    gateway: &[(String, SecretValue)],
    timeout: Duration,
    interval: Duration,
) -> Result<(), ExposureError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match probe(client, url, None, gateway).await {
            Some(code) if is_success(code) => {
                return Err(exposed(code, path, !gateway.is_empty()));
            }
            Some(401 | 403) => return Ok(()),
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ExposureError::new(format!(
                "could not verify that the endpoint rejects unauthenticated requests to {path} \
                 (no definite 401/403 answer)"
            )));
        }
        tokio::time::sleep(interval).await;
    }
}

/// Blocks until `endpoint` gives a definite answer to an UNauthenticated
/// `GET <path>`: `401`/`403` is `Ok`; a `2xx` is an exposure; anything else is
/// retried until `timeout` and then fails closed - an endpoint whose
/// protection cannot be verified is not reported deployed.
///
/// This is the single-probe half of [`verify_endpoint`], used by providers
/// that check their own gateway while deploying (Modal's proxy check).
///
/// # Errors
/// [`ExposureError`] when the endpoint is open or the answer never came.
pub async fn ensure_rejects_unauthenticated(
    endpoint: &str,
    path: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<(), ExposureError> {
    let client = client()?;
    require_refusal(
        &client,
        &url_for(endpoint, path),
        path,
        &[],
        timeout,
        interval,
    )
    .await
}

/// The shared post-deploy exposure and key check; see the module docs.
///
/// # Errors
/// [`ExposureError`] if the endpoint answers an unauthenticated request, does
/// not enforce the key behind its gateway, rejects the correct key, or cannot
/// be verified before the deadline. The caller is expected to tear the
/// endpoint down on `Err`.
pub async fn verify_endpoint(check: &EndpointCheck<'_>) -> Result<(), ExposureError> {
    let key = validate_api_key_value(check.api_key)
        .map_err(|why| ExposureError::new(format!("the endpoint API key {why}")))?;
    let client = client()?;
    let url = url_for(check.endpoint, check.path);
    let t = check.timing;

    // 1. No credentials at all.
    require_refusal(&client, &url, check.path, &[], t.reject_timeout, t.interval).await?;
    // 2. Gateway credentials but no key: the runtime must refuse on its own.
    if !check.gateway_headers.is_empty() {
        require_refusal(
            &client,
            &url,
            check.path,
            check.gateway_headers,
            t.reject_timeout,
            t.interval,
        )
        .await?;
    }
    // 3. The key works.
    let deadline = tokio::time::Instant::now() + t.accept_timeout;
    loop {
        match probe(&client, &url, Some(key), check.gateway_headers).await {
            Some(code) if is_success(code) => return Ok(()),
            Some(code @ (401 | 403)) => {
                let hint = if check.gateway_headers.is_empty() {
                    ""
                } else {
                    " (or the gateway credentials)"
                };
                return Err(ExposureError::new(format!(
                    "endpoint rejected the configured API key{hint} on {} ({code}); the key \
                     delivered to the runtime does not match",
                    check.path
                )));
            }
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ExposureError::new(format!(
                "could not verify that the API key works: no 2xx answer from {} before the \
                 deadline",
                check.path
            )));
        }
        tokio::time::sleep(t.interval).await;
    }
}

/// Turns a failed check into the message `apply` reports, tearing the
/// instance down first and saying honestly whether that worked. `stop` is the
/// adapter's own stop; `manual_hint` is the command to run by hand if it
/// fails. The instance id and hint are not secrets.
pub async fn teardown_report<F, Fut>(
    detail: &str,
    instance: &str,
    manual_hint: &str,
    stop: F,
) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    match stop().await {
        Ok(()) => format!("{detail}; the deployment ({instance}) was stopped"),
        Err(e) => {
            format!("{detail}; teardown FAILED ({e}): stop {instance} manually ({manual_hint})")
        }
    }
}

#[cfg(test)]
mod tests;
