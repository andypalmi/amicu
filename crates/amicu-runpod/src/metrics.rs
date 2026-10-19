//! `GET https://<id>.api.runpod.ai/metrics` through the gateway with the
//! deployment's bearer key, parsed by [`amicu_telemetry::scrape`]. The load
//! balancer forwards each request to one worker of its choosing, so with
//! several workers a scrape describes one of them and consecutive scrapes may
//! come from different ones. A scaled-to-zero endpoint cold-starts a worker
//! to answer.

use std::time::{Duration, Instant};

use amicu_providers::{HttpMethod, HttpRequest, HttpTransport, StreamLimits};
use amicu_telemetry::scrape::{MetricsSnapshot, snapshot_from_text};

use crate::supervisor::endpoint_url;

/// Largest `/metrics` body accepted (as for Modal).
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// A cold start answers the first request, so the scrape waits generously.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a scrape failed. `Display` never contains the key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetricsError {
    /// The gateway refused the key.
    #[error(
        "RunPod's gateway rejected the key (HTTP {0}); metrics need the deployment's api_key, a \
         RunPod API key with api.runpod.ai access to this endpoint"
    )]
    Auth(u16),
    /// The runtime answered but does not serve `/metrics`.
    #[error(
        "the server does not expose /metrics (HTTP {0}); llama-server needs `--metrics` - \
         redeploy with a current amicu"
    )]
    NotEnabled(u16),
    #[error("metrics request failed with HTTP {0}{note}", note = status_note(*.0))]
    Status(u16),
    #[error("could not reach the metrics endpoint: {0}")]
    Unreachable(String),
    #[error("the endpoint exposes no recognised vLLM or llama.cpp series")]
    Unrecognized,
    #[error("the /metrics response is larger than {} MiB", MAX_BODY_BYTES / (1024 * 1024))]
    TooLarge,
}

fn status_note(code: u16) -> &'static str {
    match code {
        429 | 502..=504 => " (no worker is ready yet - it may be cold-starting; retry shortly)",
        _ => "",
    }
}

/// One scrape of endpoint `endpoint_id`.
///
/// # Errors
/// See [`MetricsError`].
pub async fn scrape(
    transport: &dyn HttpTransport,
    endpoint_id: &str,
    key: &str,
) -> Result<MetricsSnapshot, MetricsError> {
    let url = format!("{}/metrics", endpoint_url(endpoint_id));
    let limits = StreamLimits {
        total: SCRAPE_TIMEOUT,
        idle: SCRAPE_TIMEOUT,
        max_bytes: MAX_BODY_BYTES + 1,
    };
    let resp = transport
        .execute_bounded(
            HttpRequest::new(HttpMethod::Get, url).with_bearer(key),
            limits,
        )
        .await
        .map_err(|e| MetricsError::Unreachable(e.to_string()))?;
    match resp.status {
        200..=299 => {}
        401 | 403 => return Err(MetricsError::Auth(resp.status)),
        404 | 501 => return Err(MetricsError::NotEnabled(resp.status)),
        other => return Err(MetricsError::Status(other)),
    }
    if resp.body.len() > MAX_BODY_BYTES {
        return Err(MetricsError::TooLarge);
    }
    let text = String::from_utf8_lossy(&resp.body);
    snapshot_from_text(&text).ok_or(MetricsError::Unrecognized)
}

/// A snapshot with throughput: scrapes twice `window` apart when the runtime
/// has no throughput gauge (vLLM). Meaningful with a single worker only; a
/// counter that went backwards (another worker answered) yields no rate.
///
/// # Errors
/// As [`scrape`].
pub async fn scrape_with_rate(
    transport: &dyn HttpTransport,
    endpoint_id: &str,
    key: &str,
    window: Duration,
) -> Result<MetricsSnapshot, MetricsError> {
    let first = scrape(transport, endpoint_id, key).await?;
    if first.generation_tokens_per_second.is_some() {
        return Ok(first);
    }
    let started = Instant::now();
    tokio::time::sleep(window).await;
    let second = scrape(transport, endpoint_id, key).await?;
    Ok(second.rate_since(&first, started.elapsed()))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use amicu_providers::{HttpResponse, TransportError};
    use amicu_telemetry::scrape::MetricsRuntime;
    use async_trait::async_trait;

    use super::*;

    struct Canned {
        status: u16,
        bodies: Mutex<Vec<&'static str>>,
        seen: Mutex<Vec<(String, Option<String>)>>,
    }

    impl Canned {
        fn new(status: u16, bodies: &[&'static str]) -> Self {
            Self {
                status,
                bodies: Mutex::new(bodies.iter().rev().copied().collect()),
                seen: Mutex::default(),
            }
        }
    }

    #[async_trait]
    impl HttpTransport for Canned {
        async fn execute(&self, r: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.seen.lock().unwrap().push((r.url, r.bearer));
            let body = self.bodies.lock().unwrap().pop().unwrap_or("");
            Ok(HttpResponse::new(self.status, body.as_bytes().to_vec()))
        }
    }

    #[tokio::test]
    async fn scrape_goes_through_the_gateway_with_the_bearer_key() {
        let t = Canned::new(200, &["llamacpp:requests_processing 2\n"]);
        let m = scrape(&t, "ep1", "rpa_END").await.unwrap();
        assert_eq!(m.runtime, MetricsRuntime::LlamaCpp);
        assert_eq!(m.requests_running, Some(2));
        let seen = t.seen.lock().unwrap();
        assert_eq!(seen[0].0, "https://ep1.api.runpod.ai/metrics");
        assert_eq!(seen[0].1.as_deref(), Some("rpa_END"));
    }

    #[tokio::test]
    async fn failures_are_classified_without_the_key() {
        for (status, want) in [
            (401, MetricsError::Auth(401)),
            (404, MetricsError::NotEnabled(404)),
            (502, MetricsError::Status(502)),
        ] {
            let err = scrape(&Canned::new(status, &[]), "ep1", "rpa_END")
                .await
                .unwrap_err();
            assert_eq!(err, want);
            assert!(!err.to_string().contains("rpa_END"));
        }
        let junk = scrape(&Canned::new(200, &["hello 1\n"]), "ep1", "k").await;
        assert_eq!(junk, Err(MetricsError::Unrecognized));
        let huge: &'static str = Box::leak(" ".repeat(MAX_BODY_BYTES + 1).into_boxed_str());
        let big = scrape(&Canned::new(200, &[huge]), "ep1", "k").await;
        assert_eq!(big, Err(MetricsError::TooLarge));
    }

    #[tokio::test]
    async fn a_vllm_rate_comes_from_two_scrapes() {
        let t = Canned::new(
            200,
            &[
                "vllm:generation_tokens_total 100\n",
                "vllm:generation_tokens_total 150\n",
            ],
        );
        let m = scrape_with_rate(&t, "ep1", "k", Duration::from_millis(10))
            .await
            .unwrap();
        assert!(m.generation_tokens_per_second.unwrap() > 0.0);
        assert_eq!(t.seen.lock().unwrap().len(), 2);
    }
}
