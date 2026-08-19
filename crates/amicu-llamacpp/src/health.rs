//! Endpoint health probing and the tri-state health it feeds into `status()`.

use std::time::Duration;

use async_trait::async_trait;

/// The health of a running instance, surfaced through
/// [`RuntimeStatus`](amicu_runtime::RuntimeStatus)'s `detail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Process up and the endpoint answers readiness.
    Healthy,
    /// Process up but the endpoint is not (yet) ready or is slow.
    Degraded,
    /// Process down or crashed.
    Failed,
}

impl Health {
    /// The lowercase name, as it appears in a status `detail`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::Degraded => "degraded",
            Health::Failed => "failed",
        }
    }
}

impl std::fmt::Display for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Probes a runtime endpoint's readiness. Injectable so unit tests never touch
/// the network.
#[async_trait]
pub trait HealthProbe: Send + Sync {
    /// Whether `endpoint` answers a readiness probe.
    async fn is_ready(&self, endpoint: &str) -> bool;
}

/// The real probe: a blocking HTTP `GET <endpoint>/health`, run off the async
/// executor. A `2xx` is ready; a non-2xx or unreachable endpoint is not.
#[derive(Debug, Clone)]
pub struct HttpHealthProbe {
    path: String,
    timeout: Duration,
}

impl HttpHealthProbe {
    /// A probe hitting `/health` with a two-second budget.
    #[must_use]
    pub fn new() -> Self {
        Self {
            path: "/health".to_owned(),
            timeout: Duration::from_secs(2),
        }
    }
}

impl Default for HttpHealthProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HealthProbe for HttpHealthProbe {
    async fn is_ready(&self, endpoint: &str) -> bool {
        let url = format!("{}{}", endpoint.trim_end_matches('/'), self.path);
        let timeout = self.timeout;
        tokio::task::spawn_blocking(move || http_ok(&url, timeout))
            .await
            .unwrap_or(false)
    }
}

/// Blocking readiness GET: `true` only on a 2xx response.
fn http_ok(url: &str, timeout: Duration) -> bool {
    ureq::get(url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .is_ok_and(|resp| resp.status().is_success())
}
