//! Readiness: poll `GET /health` on the VM's external IP until it answers.
//!
//! With a pinned certificate in the [`VmRecord`] the probe is HTTPS and the
//! transport from [`probe_transport`] accepts only that certificate. Without
//! one (a VM created without TLS) the probe is plain HTTP, so the bearer key
//! crosses the internet unencrypted on every poll: do not run that way.

use std::sync::Arc;
use std::time::Duration;

use amicu_core::secret::SecretValue;
use amicu_providers::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport};
use tokio::time::Instant;

use super::tls::PinnedTransport;
use super::vm::{VmRecord, VmState, status};
use crate::client::GcpClient;
use crate::error::GcpError;

/// How long to wait for the server and how often to look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthTiming {
    pub interval: Duration,
    /// Covers boot, the container image pull and the model load.
    pub timeout: Duration,
}

impl Default for HealthTiming {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            timeout: Duration::from_mins(30),
        }
    }
}

/// The transport for requests to the VM itself: [`PinnedTransport`] for a
/// record with a certificate fingerprint, a plain one otherwise. Pass it to
/// [`wait_ready`] and use it for anything else amicu sends to the VM.
///
/// # Errors
/// [`GcpError::Invalid`] for a malformed fingerprint in the record.
pub fn probe_transport(record: &VmRecord) -> Result<Arc<dyn HttpTransport>, GcpError> {
    Ok(match &record.tls_fingerprint {
        Some(f) => Arc::new(PinnedTransport::new(f, Duration::from_secs(10))?),
        None => Arc::new(ReqwestTransport::with_timeout(Duration::from_secs(10))),
    })
}

/// Waits until `<scheme>://<external ip>:<port>/health` answers `200` (sending the
/// bearer key), and returns the IP. Each round also reads the VM's state, so a
/// preemption or deletion ends the wait at once.
///
/// The scheme is `https` when the record has a certificate fingerprint. `probe`
/// is the transport for requests to the VM (not the Google API transport):
/// [`probe_transport`] in production. A transport that does not trust the pinned
/// certificate fails every poll, so it ends as [`GcpError::NotReady`], never as
/// an unverified connection.
///
/// # Errors
/// [`GcpError::VmLost`] when the VM is gone, stopped or suspended,
/// [`GcpError::NotReady`] at the deadline (the VM is left in place and keeps
/// billing: the caller decides whether to delete it), or a non-transient API error.
pub async fn wait_ready(
    client: &GcpClient,
    probe: Arc<dyn HttpTransport>,
    record: &VmRecord,
    port: u16,
    api_key: &SecretValue,
    timing: HealthTiming,
) -> Result<String, GcpError> {
    let key = api_key.expose_secret_str().map_err(|_| GcpError::Invalid {
        kind: "api key",
        value: String::new(),
        reason: "not valid UTF-8",
    })?;
    let deadline = Instant::now() + timing.timeout;
    let mut last: String;
    loop {
        match status(client, record).await {
            Ok(VmState::Running {
                external_ip: Some(ip),
            }) => {
                let scheme = if record.tls_fingerprint.is_some() {
                    "https"
                } else {
                    "http"
                };
                let req =
                    HttpRequest::new(HttpMethod::Get, format!("{scheme}://{ip}:{port}/health"))
                        .with_bearer(key);
                match probe.execute(req).await {
                    Ok(r) if r.status == 200 => return Ok(ip),
                    Ok(r) => last = format!("/health answered HTTP {}", r.status),
                    Err(e) => last = format!("/health unreachable ({e})"),
                }
            }
            Ok(state @ (VmState::Gone | VmState::Terminated { .. } | VmState::Suspended)) => {
                return Err(GcpError::VmLost {
                    name: record.name.clone(),
                    reason: match state {
                        VmState::Terminated { preempted: true } => "preempted".to_owned(),
                        VmState::Terminated { .. } => "stopped".to_owned(),
                        VmState::Suspended => "suspended".to_owned(),
                        _ => "deleted".to_owned(),
                    },
                });
            }
            Ok(other) => last = format!("VM is {other:?}"),
            Err(e) if e.is_retryable() => last = e.to_string(),
            Err(e) => return Err(e),
        }
        if Instant::now() + timing.interval >= deadline {
            return Err(GcpError::NotReady {
                name: record.name.clone(),
                message: format!(
                    "no healthy answer within {}s (last: {last})",
                    timing.timeout.as_secs()
                ),
            });
        }
        tokio::time::sleep(timing.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use amicu_providers::HttpResponse;

    use super::*;
    use crate::client::tests::{Mock, client};
    use crate::compute::Provisioning;
    use crate::compute::spec::tests::spec;
    use crate::compute::vm::prepare;
    use crate::compute::vm::tests::{ONE, instance_json};

    fn timing() -> HealthTiming {
        HealthTiming {
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(60),
        }
    }

    fn record() -> VmRecord {
        prepare(&spec("g2-standard-4", None, Provisioning::Spot)).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn polls_health_with_the_bearer_until_200() {
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            ONE,
            200,
            &instance_json("RUNNING", true, ""),
        ));
        let probe = Arc::new(Mock::default().on(HttpMethod::Get, "/health", 503, "").on(
            HttpMethod::Get,
            "/health",
            200,
            "{}",
        ));
        let ip = wait_ready(
            &c,
            probe.clone(),
            &record(),
            8000,
            &SecretValue::new("sk-test-key"),
            timing(),
        )
        .await
        .unwrap();
        assert_eq!(ip, "203.0.113.7");
        let seen = probe.requests();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].url, "http://203.0.113.7:8000/health");
        assert_eq!(seen[0].bearer.as_deref(), Some("sk-test-key"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_pinned_vm_is_probed_over_https_and_recorded() {
        let mut spec = spec("g2-standard-4", None, Provisioning::Spot);
        let id = crate::compute::TlsIdentity::generate(&spec.name).unwrap();
        spec.startup.tls = Some(id.clone());
        let rec = prepare(&spec).unwrap();
        assert_eq!(rec.tls_fingerprint, Some(id.fingerprint().unwrap()));
        assert!(probe_transport(&rec).is_ok());
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            ONE,
            200,
            &instance_json("RUNNING", true, ""),
        ));
        let probe = Arc::new(Mock::default().on(HttpMethod::Get, "/health", 200, "{}"));
        wait_ready(
            &c,
            probe.clone(),
            &rec,
            8000,
            &SecretValue::new("k"),
            timing(),
        )
        .await
        .unwrap();
        assert_eq!(probe.requests()[0].url, "https://203.0.113.7:8000/health");
    }

    #[tokio::test(start_paused = true)]
    async fn a_preempted_vm_ends_the_wait() {
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            ONE,
            200,
            &instance_json(
                "TERMINATED",
                true,
                r#","statusMessage":"Instance was preempted.""#,
            ),
        ));
        let e = wait_ready(
            &c,
            Arc::new(Mock::default()),
            &record(),
            8000,
            &SecretValue::new("k"),
            timing(),
        )
        .await
        .unwrap_err();
        assert!(matches!(e, GcpError::VmLost { ref reason, .. } if reason == "preempted"));
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_when_the_server_never_answers() {
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            ONE,
            200,
            &instance_json("RUNNING", true, ""),
        ));
        let probe = Arc::new(Mock::default().then(
            HttpMethod::Get,
            "/health",
            Ok(HttpResponse::new(502, "")),
        ));
        let e = wait_ready(&c, probe, &record(), 8000, &SecretValue::new("k"), timing())
            .await
            .unwrap_err();
        assert!(matches!(e, GcpError::NotReady { .. }));
    }
}
