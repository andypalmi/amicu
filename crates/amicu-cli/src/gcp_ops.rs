//! Google Cloud access for apply, destroy, refresh and orphans, behind a
//! trait so tests never touch the network.

use std::sync::Arc;

use amicu_core::secret::SecretValue;
use amicu_gcp::compute::{VmRecord, probe_transport};
use amicu_gcp::{AdcTokenSource, AuthConfig, GceTiming, GcpClient, GcpError};
use amicu_providers::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport};
use amicu_runtime::exposure::{CheckTiming, PROBE_PATH, validate_api_key_value};
use async_trait::async_trait;

use crate::error::CliError;

/// Where the caller's public address is read from when
/// `allowed_source_cidrs` is not set.
const IP_ECHO_URL: &str = "https://checkip.amazonaws.com";

#[async_trait]
pub(crate) trait GcpOps: Send + Sync {
    /// A client acting as the caller (or as `impersonate`). Reads local
    /// credential files only; the first token call is the preflight.
    fn client(&self, impersonate: Option<&str>) -> Result<GcpClient, CliError>;
    /// The transport for requests to the VM itself: pinned to the record's
    /// certificate fingerprint when it has one.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] for a malformed fingerprint.
    fn probe(&self, record: &VmRecord) -> Result<Arc<dyn HttpTransport>, GcpError>;
    /// The caller's public IPv4 address. `Err` is a message safe to print.
    async fn caller_ip(&self) -> Result<String, String>;
    /// The post-deploy check: an anonymous request is refused and `key` is
    /// accepted, over a connection pinned to `record`'s certificate. `Err` is
    /// secret-free.
    async fn verify_exposure(
        &self,
        endpoint: &str,
        record: &VmRecord,
        key: &SecretValue,
    ) -> Result<(), String>;
    /// Poll and readiness bounds.
    fn timing(&self) -> GceTiming {
        GceTiming::default()
    }
}

pub(crate) struct RealGcp;

#[async_trait]
impl GcpOps for RealGcp {
    fn client(&self, impersonate: Option<&str>) -> Result<GcpClient, CliError> {
        let tokens = AdcTokenSource::new(&AuthConfig {
            impersonate_service_account: impersonate.map(str::to_owned),
        })
        .map_err(|e| CliError::Message(format!("{e}; nothing was changed")))?;
        Ok(GcpClient::new(Arc::new(tokens)))
    }

    fn probe(&self, record: &VmRecord) -> Result<Arc<dyn HttpTransport>, GcpError> {
        probe_transport(record)
    }

    async fn caller_ip(&self) -> Result<String, String> {
        let resp = ReqwestTransport::new()
            .execute(HttpRequest::new(HttpMethod::Get, IP_ECHO_URL))
            .await
            .map_err(|e| format!("could not look up this machine's public IP ({e:?})"))?;
        let text = String::from_utf8_lossy(&resp.body).trim().to_owned();
        match text.parse::<std::net::Ipv4Addr>() {
            Ok(ip) if resp.is_success() => Ok(ip.to_string()),
            _ => Err("could not look up this machine's public IP".to_owned()),
        }
    }

    async fn verify_exposure(
        &self,
        endpoint: &str,
        record: &VmRecord,
        key: &SecretValue,
    ) -> Result<(), String> {
        let transport = probe_transport(record).map_err(|e| e.to_string())?;
        verify_pinned(&*transport, endpoint, key, CheckTiming::PUBLIC).await
    }
}

/// The exposure check of `amicu_runtime::exposure::verify_endpoint`, sent
/// through `transport` (pinned to the VM's certificate): an anonymous request
/// must be refused, then `key` must be accepted. Unreachable or not-ready
/// answers are retried until the timing runs out.
async fn verify_pinned(
    transport: &dyn HttpTransport,
    endpoint: &str,
    key: &SecretValue,
    timing: CheckTiming,
) -> Result<(), String> {
    let key = validate_api_key_value(key).map_err(|why| format!("the endpoint API key {why}"))?;
    let url = format!("{}{PROBE_PATH}", endpoint.trim_end_matches('/'));
    let status = |bearer: Option<&str>| {
        let mut req = HttpRequest::new(HttpMethod::Get, url.clone());
        if let Some(k) = bearer {
            req = req.with_bearer(k);
        }
        async move { transport.execute(req).await.ok().map(|r| r.status) }
    };
    let deadline = tokio::time::Instant::now() + timing.reject_timeout;
    loop {
        match status(None).await {
            Some(401 | 403) => break,
            Some(code) if (200..300).contains(&code) => {
                return Err(format!(
                    "{PROBE_PATH} answered an unauthenticated request ({code}); the endpoint is \
                     not protected by its api_key"
                ));
            }
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "could not confirm that {PROBE_PATH} refuses unauthenticated requests over the \
                 pinned certificate before the deadline"
            ));
        }
        tokio::time::sleep(timing.interval).await;
    }
    let deadline = tokio::time::Instant::now() + timing.accept_timeout;
    loop {
        match status(Some(key)).await {
            Some(code) if (200..300).contains(&code) => return Ok(()),
            Some(code @ (401 | 403)) => {
                return Err(format!(
                    "endpoint rejected the configured API key on {PROBE_PATH} ({code}); the key \
                     delivered to the runtime does not match"
                ));
            }
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "{PROBE_PATH} did not accept the configured API key before the deadline"
            ));
        }
        tokio::time::sleep(timing.interval).await;
    }
}

/// Checks the credentials and the project with one cheap read, before
/// anything is written.
pub(crate) async fn preflight(
    ops: &dyn GcpOps,
    project: &str,
    impersonate: Option<&str>,
) -> Result<(), CliError> {
    let client = ops.client(impersonate)?;
    client
        .request(
            HttpMethod::Get,
            &format!("{}/projects/{project}", amicu_gcp::client::COMPUTE_BASE_URL),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| {
            CliError::Message(format!(
                "Google Cloud preflight for project {project} failed; nothing was started: {e}"
            ))
        })
}

/// A Google Cloud seam with no credentials: tests never reach the network.
#[cfg(test)]
pub(crate) struct NoGcp;

#[cfg(test)]
#[async_trait]
impl GcpOps for NoGcp {
    fn client(&self, _: Option<&str>) -> Result<GcpClient, CliError> {
        Err(CliError::Message("no Google Cloud in this test".to_owned()))
    }
    fn probe(&self, _: &VmRecord) -> Result<Arc<dyn HttpTransport>, GcpError> {
        unreachable!("no Google Cloud in this test")
    }
    async fn caller_ip(&self) -> Result<String, String> {
        Err("no Google Cloud in this test".to_owned())
    }
    async fn verify_exposure(&self, _: &str, _: &VmRecord, _: &SecretValue) -> Result<(), String> {
        unreachable!("no Google Cloud in this test")
    }
}
