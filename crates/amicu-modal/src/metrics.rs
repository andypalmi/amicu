//! Scraping a deployed Modal server's Prometheus `/metrics` endpoint and
//! folding it into amicu's telemetry shapes.
//!
//! Parsing lives in [`amicu_telemetry::scrape`] (shared with RunPod).
//!
//! Every request goes through [`crate::proxy_auth::apply`]; errors carry only
//! the status code, a coarse transport reason and the auth hint - never a
//! header value.

use std::time::Duration;

pub use amicu_telemetry::prometheus::{Sample, parse_prometheus};
pub use amicu_telemetry::scrape::{MetricsRuntime, MetricsSnapshot, Percentiles};

/// Request timeout. Generous because a scaled-to-zero container cold-starts
/// on the first request.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(30);
/// Refuse absurdly large bodies rather than buffering them.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Why a scrape failed. `Display` never contains a header value or token.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    /// Modal's proxy answered 401/403; carries the actionable hint.
    #[error("{0}")]
    Auth(String),
    /// The runtime answered but does not serve metrics (llama-server started
    /// without `--metrics`).
    #[error(
        "the server does not expose /metrics (HTTP {0}); llama-server needs `--metrics` - \
         redeploy with a current amicu"
    )]
    NotEnabled(u16),
    /// Any other non-2xx answer.
    #[error("metrics request failed with HTTP {0}{note}", note = status_note(*.0))]
    Status(u16),
    /// The request never completed.
    #[error("could not reach the metrics endpoint: {0}")]
    Unreachable(String),
    /// The body was not usable Prometheus text.
    #[error("the response is not Prometheus text (body too large or not UTF-8)")]
    BadBody,
    /// Parsed, but no `vllm:` or `llamacpp:` series were present.
    #[error("the endpoint exposes no recognised vLLM or llama.cpp series")]
    Unrecognized,
}

fn status_note(code: u16) -> &'static str {
    match code {
        502..=504 => " (the container may be cold-starting; retry shortly)",
        _ => "",
    }
}

/// Builds a [`MetricsSnapshot`] from exposition text.
///
/// # Errors
/// [`MetricsError::Unrecognized`] when neither runtime's series are present.
pub fn snapshot_from_text(text: &str) -> Result<MetricsSnapshot, MetricsError> {
    amicu_telemetry::scrape::snapshot_from_text(text).ok_or(MetricsError::Unrecognized)
}

/// `GET <endpoint>/metrics` through the Modal proxy (proxy-auth headers
/// attached from the environment) and parse the result.
///
/// # Errors
/// See [`MetricsError`]; none of its messages contain a header or token.
pub async fn scrape(endpoint: &str) -> Result<MetricsSnapshot, MetricsError> {
    let url = format!("{}/metrics", endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(SCRAPE_TIMEOUT)
        .build()
        .map_err(|e| MetricsError::Unreachable(transport_reason(&e)))?;
    let response = crate::proxy_auth::apply(client.get(url))
        .send()
        .await
        .map_err(|e| MetricsError::Unreachable(transport_reason(&e)))?;
    let status = response.status().as_u16();
    if let Some(hint) = crate::proxy_auth::hint_for_status(status, crate::proxy_auth::env_state()) {
        return Err(MetricsError::Auth(hint));
    }
    match status {
        200..=299 => {}
        404 | 501 => return Err(MetricsError::NotEnabled(status)),
        other => return Err(MetricsError::Status(other)),
    }
    // Enforce the cap while streaming: `bytes()` would buffer a hostile or
    // runaway body in full before any length check could run.
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Err(MetricsError::BadBody);
    }
    let mut response = response;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| MetricsError::Unreachable(transport_reason(&e)))?
    {
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(MetricsError::BadBody);
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| MetricsError::BadBody)?;
    snapshot_from_text(text)
}

/// A coarse, URL-free reason for a transport failure.
fn transport_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else {
        "request failed".to_owned()
    }
}

/// Takes a throughput-bearing snapshot: scrapes twice `window` apart so a
/// runtime without a throughput gauge (vLLM) still gets tokens/s. A runtime
/// that has the gauge (llama.cpp) is returned after the first scrape.
///
/// # Errors
/// As [`scrape`].
pub async fn scrape_with_rate(
    endpoint: &str,
    window: Duration,
) -> Result<MetricsSnapshot, MetricsError> {
    let first = scrape(endpoint).await?;
    if first.generation_tokens_per_second.is_some() {
        return Ok(first);
    }
    let started = std::time::Instant::now();
    tokio::time::sleep(window).await;
    let second = scrape(endpoint).await?;
    Ok(second.rate_since(&first, started.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LLAMACPP: &str =
        "llamacpp:predicted_tokens_seconds 41.25\nllamacpp:requests_processing 1\n";

    #[test]
    fn error_messages_carry_no_secret() {
        for e in [
            MetricsError::NotEnabled(501),
            MetricsError::Status(503),
            MetricsError::Unreachable("timed out".into()),
        ] {
            let s = format!("{e} {e:?}");
            assert!(!s.contains("Modal-Secret") && !s.contains("ws-"));
        }
        assert!(MetricsError::Status(503).to_string().contains("cold-start"));
    }

    /// Serves one canned HTTP response on a loopback port; returns its URL.
    async fn serve_once(status: &str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = sock.read(&mut buf).await;
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body.as_bytes()).await.unwrap();
        });
        url
    }

    #[tokio::test]
    async fn scrape_parses_a_200_and_classifies_failures() {
        let ok = scrape(&serve_once("200 OK", LLAMACPP).await).await.unwrap();
        assert_eq!(ok.runtime, MetricsRuntime::LlamaCpp);

        let off = serve_once(
            "501 Not Implemented",
            "{\"error\":\"Start it with --metrics\"}",
        );
        assert!(matches!(
            scrape(&off.await).await,
            Err(MetricsError::NotEnabled(501))
        ));

        let cold = serve_once("503 Service Unavailable", "");
        assert!(matches!(
            scrape(&cold.await).await,
            Err(MetricsError::Status(503))
        ));

        let junk = serve_once("200 OK", "hello\n");
        assert!(matches!(
            scrape(&junk.await).await,
            Err(MetricsError::Unrecognized)
        ));
    }
}
