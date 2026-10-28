//! Readiness: poll `GET /health` on the VM's external IP until it answers.
//!
//! `GET /health` is the one route the auth middleware lets through without a
//! key, so the probe sends none: the bearer key never crosses the network
//! during readiness polling. With a pinned certificate in the [`VmRecord`] the
//! probe is HTTPS and the transport from [`probe_transport`] accepts only that
//! certificate; without one it is plain HTTP.

use std::sync::Arc;
use std::time::Duration;

use amicu_providers::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport};
use tokio::time::Instant;

use super::spec::{Provisioning, VmSpec};
use super::startup::GUEST_NAMESPACE;
use super::tls::PinnedTransport;
use super::vm::{VmRecord, VmState, guest_attributes, observe, was_preempted};
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
    /// Sized for a VM that starts at once (spot or standard): 30 minutes.
    ///
    /// Callers that have a [`VmSpec`] must use [`HealthTiming::for_spec`]
    /// instead. A flex-start VM may queue for capacity for up to 2 hours before
    /// it boots, and this default would give up while the request is still
    /// waiting.
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

/// Guest attributes are read at most this often: Compute Engine allows 10
/// queries a minute per VM, and may drop attributes when they come faster.
const REPORT_GAP: Duration = Duration::from_secs(15);
/// The [`GcpError::VmLost`] reason for a Spot VM Compute Engine reclaimed.
pub const PREEMPTED: &str = "preempted: Compute Engine reclaimed the Spot capacity; run \
                             `amicu apply` again, or set `provisioning = \"standard\"` to avoid \
                             preemption";
/// Lines of the startup script's log kept in [`GcpError::StartupFailed`].
const DETAIL_LINES: usize = 25;

/// What the startup script reported (see [`super::startup`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupReport {
    Ready,
    /// The step that failed and the tail of the script's log.
    Failed {
        step: String,
        detail: String,
    },
}

/// The startup script's report, or `None` when there is none yet or it cannot
/// be read (guest attributes are a fast path; the health deadline still
/// applies without them, for example when the caller lacks
/// `compute.instances.getGuestAttributes`).
pub async fn startup_report(client: &GcpClient, record: &VmRecord) -> Option<StartupReport> {
    let attrs = guest_attributes(client, record, GUEST_NAMESPACE)
        .await
        .ok()?;
    let get = |k: &str| {
        attrs
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let status = get("status")?.trim();
    if status == "ready" {
        return Some(StartupReport::Ready);
    }
    let step = status.strip_prefix("failed:")?;
    Some(StartupReport::Failed {
        step: step.chars().filter(|c| !c.is_control()).take(100).collect(),
        detail: clean(get("detail").unwrap_or_default(), DETAIL_LINES),
    })
}

/// The last `lines` lines of `text` from the VM, without control characters
/// other than newlines, each indented.
fn clean(text: &str, lines: usize) -> String {
    let kept: Vec<String> = text
        .lines()
        .map(|l| {
            l.chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect::<String>()
        })
        .map(|l| l.trim_end().to_owned())
        .filter(|l| !l.is_empty())
        .collect();
    kept[kept.len().saturating_sub(lines)..]
        .iter()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Waits until `<scheme>://<external ip>:<port>/health` answers `200` (without the
/// bearer key), and returns the IP. Each round also reads the VM's state, so a
/// preemption or deletion ends the wait at once, and while the probe fails, the
/// startup script's report, so a failed boot ends it at once too.
///
/// The scheme is `https` when the record has a certificate fingerprint. `probe`
/// is the transport for requests to the VM (not the Google API transport):
/// [`probe_transport`] in production. A transport that does not trust the pinned
/// certificate fails every poll, so it ends as [`GcpError::NotReady`], never as
/// an unverified connection.
///
/// # Errors
/// [`GcpError::VmLost`] when the VM is gone, stopped or suspended,
/// [`GcpError::StartupFailed`] when its startup script reports a failure,
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
    let mut next_report = Instant::now();
    let mut vm_ready = false;
    let mut vm_id = None;
    let mut last: String;
    loop {
        let seen = observe(client, record).await.map(|(state, id)| {
            vm_id = id.or(vm_id.take());
            state
        });
        match seen {
            Ok(VmState::Running {
                external_ip: Some(ip),
            }) => {
                let scheme = if record.tls_fingerprint.is_some() {
                    "https"
                } else {
                    "http"
                };
                let req =
                    HttpRequest::new(HttpMethod::Get, format!("{scheme}://{ip}:{port}/health"));
                match probe.execute(req).await {
                    Ok(r) if r.status == 200 => return Ok(ip),
                    Ok(r) => last = format!("/health answered HTTP {}", r.status),
                    Err(e) => last = format!("/health unreachable ({e})"),
                }
                if Instant::now() >= next_report {
                    next_report = Instant::now() + REPORT_GAP;
                    match startup_report(client, record).await {
                        Some(StartupReport::Failed { step, detail }) => {
                            return Err(GcpError::StartupFailed {
                                name: record.name.clone(),
                                step,
                                detail,
                            });
                        }
                        Some(StartupReport::Ready) => vm_ready = true,
                        None => {}
                    }
                }
            }
            Ok(state @ (VmState::Gone | VmState::Terminated { .. } | VmState::Suspended)) => {
                let preempted = match state {
                    VmState::Terminated { preempted } => preempted,
                    VmState::Gone => was_preempted(client, record, vm_id.as_deref()).await,
                    _ => false,
                };
                return Err(GcpError::VmLost {
                    name: record.name.clone(),
                    reason: match state {
                        _ if preempted => PREEMPTED.to_owned(),
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
                    "no healthy answer within {}s (last: {last}){}",
                    timing.timeout.as_secs(),
                    if vm_ready {
                        "; the VM reports vLLM ready, so the firewall rule or the network path \
                         drops the probe (check allowed_source_cidrs)"
                    } else {
                        ""
                    }
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
        wait_ready(&c, probe.clone(), &rec, 8000, timing())
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
        let e = wait_ready(&c, Arc::new(Mock::default()), &record(), 8000, timing())
            .await
            .unwrap_err();
        assert!(matches!(e, GcpError::VmLost { ref reason, .. } if reason == PREEMPTED));
    }

    const PREEMPTIONS: &str =
        "/zones/us-central1-a/operations?filter=operationType%3D%22compute.instances.preempted%22";

    fn preemptions(target_id: &str) -> String {
        serde_json::json!({"items": [{
            "operationType": "compute.instances.preempted",
            "targetId": target_id,
            "targetLink": "https://compute.googleapis.com/compute/v1/projects/my-proj/zones/us-central1-a/instances/amicu-demo-chat-0123456789ab-n1",
        }]})
        .to_string()
    }

    #[tokio::test(start_paused = true)]
    async fn a_spot_vm_deleted_by_preemption_is_reported_as_preempted() {
        let gone_after_running = |ops: &str| {
            Mock::default()
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("RUNNING", true, r#","id":"4242""#),
                )
                .on(HttpMethod::Get, ONE, 404, "")
                .on(HttpMethod::Get, PREEMPTIONS, 200, ops)
        };
        let (c, _) = client(gone_after_running(&preemptions("4242")));
        let e = wait_ready(&c, down(), &record(), 8000, timing())
            .await
            .unwrap_err();
        let msg = e.to_string();
        assert!(matches!(e, GcpError::VmLost { ref reason, .. } if reason == PREEMPTED));
        assert!(msg.contains("provisioning = \"standard\""), "{msg}");
        assert!(msg.contains("amicu apply"), "{msg}");

        // An earlier VM of the same name, preempted before, does not count.
        let (c, _) = client(gone_after_running(&preemptions("1")));
        let e = wait_ready(&c, down(), &record(), 8000, timing())
            .await
            .unwrap_err();
        assert!(matches!(e, GcpError::VmLost { ref reason, .. } if reason == "deleted"));

        let (c, _) = client(Mock::default().on(HttpMethod::Get, ONE, 404, "").on(
            HttpMethod::Get,
            PREEMPTIONS,
            403,
            "{}",
        ));
        let e = wait_ready(&c, down(), &record(), 8000, timing())
            .await
            .unwrap_err();
        assert!(matches!(e, GcpError::VmLost { ref reason, .. } if reason == "deleted"));
    }

    const GUEST: &str =
        "/instances/amicu-demo-chat-0123456789ab-n1/getGuestAttributes?queryPath=amicu%2F";

    fn guest_json(status: &str, detail: &str) -> String {
        serde_json::json!({
            "queryPath": "amicu/",
            "queryValue": {"items": [
                {"namespace": "amicu", "key": "status", "value": status},
                {"namespace": "amicu", "key": "detail", "value": detail},
                {"namespace": "other", "key": "status", "value": "ready"},
            ]},
        })
        .to_string()
    }

    fn running() -> Mock {
        Mock::default().on(
            HttpMethod::Get,
            ONE,
            200,
            &instance_json("RUNNING", true, ""),
        )
    }

    fn down() -> Arc<Mock> {
        Arc::new(Mock::default().on(HttpMethod::Get, "/health", 503, ""))
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_startup_script_ends_the_wait_at_once_with_its_step_and_log() {
        let log =
            "apt noise\n".repeat(40) + "amicu: unexpected NVIDIA repository key none\x1b[0m\n";
        let (c, api) = client(running().on(HttpMethod::Get, GUEST, 404, "").on(
            HttpMethod::Get,
            GUEST,
            200,
            &guest_json(
                "failed:install Docker and the NVIDIA container toolkit",
                &log,
            ),
        ));
        let started = Instant::now();
        let e = wait_ready(&c, down(), &record(), 8000, timing())
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
        let GcpError::StartupFailed { step, detail, .. } = &e else {
            panic!("{e:?}");
        };
        assert_eq!(step, "install Docker and the NVIDIA container toolkit");
        assert!(
            detail.ends_with("  amicu: unexpected NVIDIA repository key none[0m"),
            "{detail}"
        );
        assert_eq!(detail.lines().count(), DETAIL_LINES);
        let msg = e.to_string();
        assert!(
            msg.starts_with(
                "VM amicu-demo-chat-0123456789ab-n1: the startup script failed at step \
                 `install Docker and the NVIDIA container toolkit`; the end of its log:\n  apt noise"
            ),
            "{msg}"
        );
        assert!(!msg.contains("gcloud"), "{msg}");
        let guest_reads = api
            .requests()
            .iter()
            .filter(|r| r.url.ends_with(GUEST))
            .count();
        assert_eq!(guest_reads, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn guest_attributes_are_read_at_most_every_15_seconds_and_errors_are_ignored() {
        let (c, api) = client(running().on(
            HttpMethod::Get,
            GUEST,
            403,
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no"}}"#,
        ));
        let e = wait_ready(&c, down(), &record(), 8000, timing())
            .await
            .unwrap_err();
        assert!(matches!(e, GcpError::NotReady { .. }), "{e:?}");
        let guest_reads = api
            .requests()
            .iter()
            .filter(|r| r.url.ends_with(GUEST))
            .count();
        // 60 s polled every 5 s: reads at 0, 15, 30 and 45 s.
        assert_eq!(guest_reads, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_that_reports_ready_but_cannot_be_reached_names_the_firewall() {
        let (c, _) = client(running().on(HttpMethod::Get, GUEST, 200, &guest_json("ready", "")));
        let e = wait_ready(&c, down(), &record(), 8000, timing())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("the VM reports vLLM ready"), "{e}");
        assert!(e.contains("allowed_source_cidrs"), "{e}");
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
