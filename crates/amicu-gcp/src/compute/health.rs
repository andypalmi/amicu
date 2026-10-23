//! Readiness: poll `GET /health` on the VM's external IP until it answers.
//!
//! `GET /health` is the one route the auth middleware lets through without a
//! key, so the probe sends none: the bearer key never crosses the network
//! during readiness polling, over plain HTTP or otherwise.

use std::sync::Arc;
use std::time::Duration;

use amicu_providers::{HttpMethod, HttpRequest, HttpTransport};
use tokio::time::Instant;

use super::spec::{Provisioning, VmSpec};
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

/// Time allowed for boot, the image pull and the model load once the VM runs.
const BOOT_BUDGET: Duration = Duration::from_mins(30);

impl Default for HealthTiming {
    /// Sized for a VM that starts at once (spot or standard): 30 minutes. For a
    /// flex-start VM, which may queue for capacity for up to 2 hours before it
    /// boots, use [`HealthTiming::for_spec`]; this default would give up while
    /// the request is still waiting.
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            timeout: BOOT_BUDGET,
        }
    }
}

impl HealthTiming {
    /// The default timing with the timeout extended by the flex-start wait of
    /// `spec` (zero for other provisioning models), so the wait covers queueing
    /// for capacity plus boot.
    #[must_use]
    pub fn for_spec(spec: &VmSpec) -> Self {
        let queue = match spec.provisioning {
            Provisioning::FlexStart { wait } => wait,
            _ => Duration::ZERO,
        };
        Self {
            timeout: BOOT_BUDGET + queue,
            ..Self::default()
        }
    }
}

/// Waits until `http://<external ip>:<port>/health` answers `200` (without the
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
    timing: HealthTiming,
) -> Result<String, GcpError> {
    let deadline = Instant::now() + timing.timeout;
    let mut last: String;
    loop {
        match status(client, record).await {
            Ok(VmState::Running {
                external_ip: Some(ip),
            }) => {
                let req = HttpRequest::new(HttpMethod::Get, format!("http://{ip}:{port}/health"));
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
    async fn polls_health_without_the_bearer_until_200() {
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
        let ip = wait_ready(&c, probe.clone(), &record(), 8000, timing())
            .await
            .unwrap();
        assert_eq!(ip, "203.0.113.7");
        let seen = probe.requests();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].url, "http://203.0.113.7:8000/health");
        assert!(seen.iter().all(|r| r.bearer.is_none()));
    }

    #[test]
    fn health_timeout_covers_the_flex_start_wait() {
        let t = |p| HealthTiming::for_spec(&spec("g2-standard-4", None, p)).timeout;
        assert_eq!(t(Provisioning::Spot), Duration::from_mins(30));
        assert_eq!(t(Provisioning::Standard), Duration::from_mins(30));
        assert_eq!(
            t(Provisioning::FlexStart {
                wait: Duration::from_hours(2)
            }),
            Duration::from_mins(150)
        );
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
        let e = wait_ready(&c, Arc::new(Mock::default()), &record(), 8000, timing())
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
        let e = wait_ready(&c, probe, &record(), 8000, timing())
            .await
            .unwrap_err();
        assert!(matches!(e, GcpError::NotReady { .. }));
    }
}
