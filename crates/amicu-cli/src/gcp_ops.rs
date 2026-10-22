//! Google Cloud access for apply, destroy, refresh and orphans, behind a
//! trait so tests never touch the network.

use std::sync::Arc;

use amicu_core::secret::SecretValue;
use amicu_gcp::{AdcTokenSource, AuthConfig, GceTiming, GcpClient};
use amicu_providers::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport};
use amicu_runtime::exposure::{CheckTiming, EndpointCheck, PROBE_PATH};
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
    /// The transport the readiness probe goes through.
    fn probe(&self) -> Arc<dyn HttpTransport>;
    /// The caller's public IPv4 address. `Err` is a message safe to print.
    async fn caller_ip(&self) -> Result<String, String>;
    /// The post-deploy check: an anonymous request is refused and `key` is
    /// accepted. `Err` is secret-free.
    async fn verify_exposure(&self, endpoint: &str, key: &SecretValue) -> Result<(), String>;
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

    fn probe(&self) -> Arc<dyn HttpTransport> {
        Arc::new(ReqwestTransport::new())
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

    async fn verify_exposure(&self, endpoint: &str, key: &SecretValue) -> Result<(), String> {
        amicu_runtime::exposure::verify_endpoint(&EndpointCheck {
            endpoint,
            path: PROBE_PATH,
            api_key: key,
            gateway_headers: &[],
            timing: CheckTiming::PUBLIC,
        })
        .await
        .map_err(|e| e.detail().to_owned())
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
    fn probe(&self) -> Arc<dyn HttpTransport> {
        unreachable!("no Google Cloud in this test")
    }
    async fn caller_ip(&self) -> Result<String, String> {
        Err("no Google Cloud in this test".to_owned())
    }
    async fn verify_exposure(&self, _: &str, _: &SecretValue) -> Result<(), String> {
        unreachable!("no Google Cloud in this test")
    }
}
