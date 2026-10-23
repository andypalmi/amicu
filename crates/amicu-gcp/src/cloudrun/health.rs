//! Waiting for the service to answer `GET <url>/health`.

use std::sync::Arc;
use std::time::Duration;

use amicu_providers::{HttpMethod, HttpRequest, HttpTransport};
use tokio::time::Instant;

use crate::error::GcpError;

/// Polling bounds. A cold start from zero loads the model on the first
/// request, so the wait is long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthTiming {
    /// Give up after this long.
    pub timeout: Duration,
    /// First pause between polls.
    pub initial_backoff: Duration,
    /// Longest pause (the backoff doubles up to it).
    pub max_backoff: Duration,
}

impl HealthTiming {
    /// Thirty minutes, polling every 2 s backing off to 15 s.
    pub const DEFAULT: Self = Self {
        timeout: Duration::from_mins(30),
        initial_backoff: Duration::from_secs(2),
        max_backoff: Duration::from_secs(15),
    };
}

impl Default for HealthTiming {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Polls `<url>/health` with `Authorization: Bearer <api_key>` until it
/// returns `2xx`.
///
/// While Cloud Run starts an instance (cold start, model load) the request may
/// be held, time out, or get a `429`/`5xx`; all of those are retried. A `401`
/// or `403` ends the wait: with `invokerIamDisabled` that means the service is
/// IAM-protected.
///
/// # Errors
/// [`GcpError::PermissionDenied`] on `401`/`403`;
/// [`GcpError::OperationTimeout`] at the deadline.
pub async fn wait_ready(
    transport: &Arc<dyn HttpTransport>,
    url: &str,
    api_key: &str,
    timing: HealthTiming,
) -> Result<(), GcpError> {
    let health = format!("{}/health", url.trim_end_matches('/'));
    let deadline = Instant::now() + timing.timeout;
    let mut backoff = timing.initial_backoff;
    loop {
        let req = HttpRequest::new(HttpMethod::Get, &health).with_bearer(api_key);
        match transport.execute(req).await {
            Ok(r) if r.is_success() => return Ok(()),
            Ok(r) if r.status == 401 || r.status == 403 => {
                return Err(GcpError::PermissionDenied {
                    status: r.status,
                    code: "HEALTH".into(),
                    message: "the service rejected the health probe (IAM-protected, or a wrong \
                              key)"
                        .into(),
                });
            }
            Ok(_) | Err(_) => {}
        }
        if Instant::now() + backoff >= deadline {
            return Err(GcpError::OperationTimeout {
                operation: format!("health check of {health}"),
            });
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(timing.max_backoff);
    }
}
