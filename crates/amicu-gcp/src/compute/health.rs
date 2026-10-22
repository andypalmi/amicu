//! Readiness: poll `GET /health` on the VM's external IP until it answers.
//!
//! TEMPORARY: the probe uses plain HTTP, so the bearer key crosses the
//! internet unencrypted on every poll. Serving over TLS with a pinned
//! certificate is a follow-up; until then the firewall rule's source range is
//! the only protection for the first request, and the key should be treated as
//! exposed to anything on the path.

use std::sync::Arc;
use std::time::Duration;

use amicu_core::secret::SecretValue;
use amicu_providers::{HttpMethod, HttpRequest, HttpTransport};
use tokio::time::Instant;

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

/// Waits until `http://<external ip>:<port>/health` answers `200` (sending the
/// bearer key), and returns the IP. Each round also reads the VM's state, so a
/// preemption or deletion ends the wait at once.
///
/// `probe` is the transport for the plain-HTTP requests to the VM (not the
/// Google API transport).
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
                let req = HttpRequest::new(HttpMethod::Get, format!("http://{ip}:{port}/health"))
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
