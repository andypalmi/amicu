//! The telemetry agent for cloud deployments (Modal, RunPod).
//!
//! A cloud deployment has no process on this machine to sit beside, so its
//! agent is a launchd/systemd job of its own (labelled like a host
//! deployment's sidecar) that `apply` starts once the deployment serves. It
//! is the same `amicu telemetry-agent` as for a host deployment; only where
//! the `/metrics` text comes from differs:
//!
//! * Modal: `<endpoint>/metrics` with the workspace's Proxy Auth Token
//!   (`Modal-Key` / `Modal-Secret`, as every other request to the endpoint);
//!   the accelerator is read by the hardware probe in the running container.
//! * RunPod: `<id>.api.runpod.ai/metrics` (Serverless) or the pod's proxy
//!   URL, with the deployment's `api_key` as the bearer.
//!
//! The job holds provider credentials, so its definition is as private as a
//! host instance's (which holds the endpoint API key): they live in the job's
//! environment, never argv, and the file is deleted when the agent stops.
//!
//! **A scrape is a request, and a request keeps a scaled-to-zero deployment
//! up (and billed).** The agent therefore never wakes one: before each scrape
//! it asks the provider's control plane whether a replica is already running
//! (Modal: the app's tasks; RunPod Serverless: its workers) and does nothing
//! if not. Once a scrape shows no new requests or tokens since the previous
//! one, it stops scraping for `--idle-pause-secs` (the deployment's scale-down
//! window plus a margin), so the deployment can scale down on schedule. The
//! price is that traffic starting inside that pause is missed until the next
//! scrape, and that windows with no scrapes are not reported.

use std::time::Duration;

use amicu_core::resource::ScalingSettings;
use amicu_modal::proxy_auth::{ProxyToken, TokenConfig};
use amicu_modal::{Credentials, ModalClient};
use amicu_runpod::{ApiKey, RunPodClient};
use amicu_telemetry::hardware::HardwareSample;
use amicu_telemetry::rollup::Scrape;
use async_trait::async_trait;

use super::telemetry_agent::Descriptor;

/// Where the agent reads `/metrics` from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub(crate) enum SourceKind {
    /// The deployment's own loopback port.
    #[default]
    Local,
    Modal,
    RunpodEndpoint,
    RunpodPod,
}

impl SourceKind {
    /// The `--source` value.
    pub(crate) fn arg(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Modal => "modal",
            Self::RunpodEndpoint => "runpod-endpoint",
            Self::RunpodPod => "runpod-pod",
        }
    }

    /// The provider the deployment runs on, as `apply` names it.
    pub(crate) fn provider(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Modal => "modal",
            Self::RunpodEndpoint | Self::RunpodPod => "runpod",
        }
    }

    /// Host-name suffixes the metrics URL may have. The job sends provider
    /// credentials to it, so nothing else is accepted.
    fn host_suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Local => &[],
            Self::Modal => &[".modal.direct", ".modal.run"],
            Self::RunpodEndpoint => &[".api.runpod.ai"],
            Self::RunpodPod => &[".proxy.runpod.net"],
        }
    }
}

/// Env var (in the agent's job) holding the deployment's `api_key`, the
/// bearer RunPod accepts on `/metrics`.
pub(crate) const GATEWAY_KEY_ENV: &str = "AMICU_TELEMETRY_GATEWAY_KEY";
/// Env var holding the RunPod management key, which lists an endpoint's workers.
pub(crate) const RUNPOD_KEY_ENV: &str = crate::runpod_ops::API_KEY_ENV_VAR;
const MODAL_ENV: [&str; 4] = [
    "MODAL_TOKEN_ID",
    "MODAL_TOKEN_SECRET",
    amicu_modal::proxy_auth::TOKEN_ID_ENV_VAR,
    amicu_modal::proxy_auth::TOKEN_SECRET_ENV_VAR,
];

/// Whether `raw` is a metrics URL `kind` may scrape: `https`, a provider host,
/// path `/metrics`, and no userinfo, query or fragment.
pub(crate) fn is_cloud_metrics_url(kind: SourceKind, raw: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    let host_ok = url
        .host_str()
        .is_some_and(|h| kind.host_suffixes().iter().any(|s| h.ends_with(s)));
    url.scheme() == "https"
        && host_ok
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/metrics"
        && url.query().is_none()
        && url.fragment().is_none()
}

// ---- apply side -------------------------------------------------------------

/// What `apply` tells the cloud agent about the deployment it just started.
pub(crate) struct CloudAgent {
    pub kind: SourceKind,
    /// The host label the job is named after (see `sidecar_label`); also the
    /// key of the deployment's anonymous id.
    pub main_label: String,
    /// The Modal app id or RunPod endpoint id the control plane is asked about.
    pub resource_id: String,
    /// The deployment's endpoint URL, without `/metrics`.
    pub endpoint: String,
    pub runtime: &'static str,
    pub descriptor: Descriptor,
    pub region: Option<String>,
    pub context_length: u32,
    pub batch_size: Option<u32>,
    /// How long to stop scraping after an idle scrape; zero for a deployment
    /// that is always up (a pod, or Modal kept warm with `min_replicas`).
    pub idle_pause_secs: u64,
}

/// How long the agent stops scraping after an idle scrape: the deployment's
/// scale-down window plus a margin, so it has scaled down by the time the
/// agent looks again. Zero when the deployment never scales down (a pod, or
/// replicas kept warm), since then scraping costs nothing.
pub(crate) fn idle_pause_secs(kind: SourceKind, scaling: Option<&ScalingSettings>) -> u64 {
    const MARGIN: u64 = 120;
    let scaling = scaling.cloned().unwrap_or_default();
    let (window, min_warm) = match kind {
        SourceKind::Local | SourceKind::RunpodPod => return 0,
        SourceKind::Modal => (
            scaling
                .scaledown_window_secs
                .unwrap_or(amicu_modal::supervisor::SCALEDOWN_WINDOW_SECS),
            scaling
                .min_replicas
                .unwrap_or(amicu_modal::supervisor::MIN_CONTAINERS),
        ),
        SourceKind::RunpodEndpoint => {
            let w = amicu_runpod::supervisor::workers_config(Some(&scaling));
            (w.idle_timeout.unwrap_or(0), w.min)
        }
    };
    if min_warm > 0 {
        0
    } else {
        u64::from(window) + MARGIN
    }
}

/// The job's environment: provider credentials from `getenv` (the shell
/// `apply` runs in) and the resolved gateway key.
///
/// # Errors
/// Names (never values) the variables that are missing.
pub(crate) fn credentials(
    kind: SourceKind,
    getenv: &dyn Fn(&str) -> Option<String>,
    gateway_key: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let get = |name: &str| getenv(name).filter(|v| !v.trim().is_empty());
    let need: Vec<&str> = match kind {
        SourceKind::Local | SourceKind::RunpodPod => Vec::new(),
        SourceKind::Modal => MODAL_ENV.to_vec(),
        SourceKind::RunpodEndpoint => vec![RUNPOD_KEY_ENV],
    };
    let missing: Vec<&str> = need.iter().copied().filter(|n| get(n).is_none()).collect();
    if !missing.is_empty() {
        return Err(format!("{} not set", missing.join(" and ")));
    }
    let mut env: Vec<(String, String)> = need
        .iter()
        .filter_map(|n| get(n).map(|v| ((*n).to_owned(), v)))
        .collect();
    if matches!(kind, SourceKind::RunpodEndpoint | SourceKind::RunpodPod) {
        let key = gateway_key.ok_or_else(|| "the deployment has no api_key".to_owned())?;
        env.push((GATEWAY_KEY_ENV.to_owned(), key.to_owned()));
    }
    Ok(env)
}

// ---- agent side -------------------------------------------------------------

/// How a scrape authenticates.
pub(crate) enum Auth {
    ModalProxy(ProxyToken),
    Bearer(String),
}

impl Auth {
    /// From the job's environment; `None` when what `kind` needs is missing.
    pub(crate) fn from_env(kind: SourceKind) -> Option<Self> {
        match kind {
            SourceKind::Local => None,
            SourceKind::Modal => match TokenConfig::from_env() {
                TokenConfig::Complete(token) => Some(Self::ModalProxy(token)),
                _ => None,
            },
            SourceKind::RunpodEndpoint | SourceKind::RunpodPod => std::env::var(GATEWAY_KEY_ENV)
                .ok()
                .filter(|k| !k.trim().is_empty())
                .map(Self::Bearer),
        }
    }

    fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::ModalProxy(token) => token
                .headers()
                .into_iter()
                .fold(request, |req, (name, value)| req.header(name, value)),
            Self::Bearer(key) => request.bearer_auth(key),
        }
    }
}

/// Whether a replica of the deployment is already running. Implementations
/// only read the control plane; they never send a request that could start one.
#[async_trait]
pub(crate) trait Gate: Send + Sync {
    /// `None`: nothing is running (or it could not be told). `Some(sample)`:
    /// something is, with what the hardware probe saw in it, if it ran.
    async fn running(&self) -> Option<Option<HardwareSample>>;
}

/// Always up (a pod, or a deployment kept warm): nothing to ask.
pub(crate) struct AlwaysUp;

#[async_trait]
impl Gate for AlwaysUp {
    async fn running(&self) -> Option<Option<HardwareSample>> {
        Some(None)
    }
}

/// A Modal app's running containers. The hardware probe runs in the first that
/// answers, until one has: it execs in the container, so it is not repeated
/// once the agent has what it reports.
pub(crate) struct ModalGate {
    app_id: String,
    credentials: Credentials,
    probed: std::sync::atomic::AtomicBool,
    warned: std::sync::atomic::AtomicBool,
}

impl ModalGate {
    pub(crate) fn from_env(app_id: &str) -> Option<Self> {
        Some(Self {
            app_id: app_id.to_owned(),
            credentials: Credentials::from_env().ok()?,
            probed: std::sync::atomic::AtomicBool::new(false),
            warned: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl Gate for ModalGate {
    async fn running(&self) -> Option<Option<HardwareSample>> {
        use std::io::Write;
        use std::sync::atomic::Ordering;
        let tasks = async {
            let mut client = ModalClient::connect(self.credentials.clone()).await?;
            let tasks = client.app_task_ids(&self.app_id).await?;
            Ok::<_, amicu_modal::ModalError>((client, tasks))
        }
        .await;
        let (mut client, tasks) = match tasks {
            Ok(v) => v,
            Err(e) => {
                // A rotated or revoked token ends up here every poll: say so
                // once, in the agent's log, instead of failing silently.
                if !self.warned.swap(true, Ordering::Relaxed) {
                    let _ = writeln!(
                        std::io::stderr(),
                        "telemetry-agent: cannot ask Modal whether the deployment is running \
                         ({e}); no data is collected until it can (was the Modal token rotated?)"
                    );
                }
                return None;
            }
        };
        self.warned.store(false, Ordering::Relaxed);
        if tasks.is_empty() {
            return None;
        }
        if self.probed.load(Ordering::Relaxed) {
            return Some(None);
        }
        for task in &tasks {
            if let Ok(sample) = client.probe_container(task).await {
                self.probed.store(true, Ordering::Relaxed);
                return Some(Some(sample));
            }
        }
        Some(None)
    }
}

/// A RunPod Serverless endpoint's workers.
pub(crate) struct RunPodGate {
    client: RunPodClient,
    endpoint_id: String,
}

impl RunPodGate {
    pub(crate) fn from_env(endpoint_id: &str) -> Option<Self> {
        let key = std::env::var(RUNPOD_KEY_ENV)
            .ok()
            .filter(|k| !k.trim().is_empty())?;
        Some(Self {
            client: RunPodClient::new(ApiKey::new(key)),
            endpoint_id: endpoint_id.to_owned(),
        })
    }
}

#[async_trait]
impl Gate for RunPodGate {
    async fn running(&self) -> Option<Option<HardwareSample>> {
        let s = self
            .client
            .list_workers(&self.endpoint_id)
            .await
            .ok()?
            .summary;
        (s.running.saturating_add(s.idle) > 0).then_some(None)
    }
}

/// One poll of a source: the scrape and hardware sample, if it got any.
#[derive(Default)]
pub(crate) struct Polled {
    pub scrape: Option<Scrape>,
    pub hardware: Option<HardwareSample>,
}

/// A deployment reached over the network.
pub(crate) struct Remote {
    url: String,
    auth: Auth,
    gate: Box<dyn Gate>,
    client: reqwest::Client,
    idle_pause_secs: u64,
    paused_until: u64,
    /// Requests, prompt and output tokens counted by the last scrape.
    last: Option<[Option<f64>; 3]>,
}

impl Remote {
    pub(crate) fn new(
        url: String,
        auth: Auth,
        gate: Box<dyn Gate>,
        idle_pause_secs: u64,
    ) -> Result<Self, reqwest::Error> {
        // No redirects, so credentials only ever go to the validated host.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            url,
            auth,
            gate,
            client,
            idle_pause_secs,
            paused_until: 0,
            last: None,
        })
    }

    /// Scrape once if a replica is running and the idle pause is over.
    pub(crate) async fn poll(&mut self, now: u64) -> Polled {
        if now < self.paused_until {
            return Polled::default();
        }
        let Some(hardware) = self.gate.running().await else {
            // Scaled to zero: the counters restart with the next container.
            self.last = None;
            return Polled::default();
        };
        let request = self.auth.apply(self.client.get(&self.url));
        let scrape = super::telemetry_agent::scrape(request)
            .await
            .map(|(_, s)| s);
        if scrape.is_none() {
            // A request that fails still reaches the container and resets its
            // scale-down timer: back off like for an idle one.
            self.paused_until = now + self.idle_pause_secs;
        }
        if let Some(s) = &scrape {
            let seen = [
                s.requests_total,
                s.prompt_tokens_total,
                s.output_tokens_total,
            ];
            if self.last == Some(seen) {
                self.paused_until = now + self.idle_pause_secs;
            }
            self.last = Some(seen);
        }
        Polled { scrape, hardware }
    }
}

/// Fakes shared with the agent's tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A gate the test flips, counting how often it was asked.
    pub(crate) struct TestGate {
        pub(crate) up: Arc<AtomicBool>,
        pub(crate) asked: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Gate for TestGate {
        async fn running(&self) -> Option<Option<HardwareSample>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.up.load(Ordering::SeqCst).then_some(None)
        }
    }

    pub(crate) fn vllm(requests: u32, tokens: u32) -> String {
        format!(
            "vllm:request_success_total{{finished_reason=\"stop\"}} {requests}\n\
             vllm:generation_tokens_total {tokens}\n\
             vllm:prompt_tokens_total {tokens}\n"
        )
    }

    /// A loopback server answering every request with the next of `bodies`
    /// (the last repeats), logging each request's text.
    pub(crate) async fn serve(bodies: Vec<String>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/metrics", listener.local_addr().unwrap());
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let index = {
                    let mut log = log.lock().unwrap();
                    log.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                    log.len() - 1
                };
                let body = &bodies[index.min(bodies.len() - 1)];
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (url, seen)
    }

    /// A loopback server answering every request with a 500.
    pub(crate) async fn serve_failing() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/metrics", listener.local_addr().unwrap());
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let reply = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\
                             connection: close\r\n\r\n";
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (url, seen)
    }

    pub(crate) fn remote(
        url: &str,
        up: &Arc<AtomicBool>,
        asked: &Arc<AtomicUsize>,
        pause: u64,
    ) -> Remote {
        let gate = TestGate {
            up: up.clone(),
            asked: asked.clone(),
        };
        Remote::new(
            url.to_owned(),
            Auth::Bearer("k-1".into()),
            Box::new(gate),
            pause,
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{remote, serve, serve_failing, vllm};
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn only_provider_hosts_are_scraped_with_credentials() {
        use SourceKind::{Modal, RunpodEndpoint, RunpodPod};
        for (kind, url) in [
            (Modal, "https://ws--app-server.us-east.modal.direct/metrics"),
            (RunpodEndpoint, "https://abc123.api.runpod.ai/metrics"),
            (RunpodPod, "https://p1-8000.proxy.runpod.net/metrics"),
        ] {
            assert!(is_cloud_metrics_url(kind, url), "{url}");
        }
        for (kind, url) in [
            (Modal, "http://ws--app.modal.direct/metrics"),
            (Modal, "https://evil.example/metrics"),
            (Modal, "https://modal.direct.evil.example/metrics"),
            (Modal, "https://evilmodal.direct/metrics"),
            (Modal, "https://u:p@ws--app.modal.direct/metrics"),
            (Modal, "https://ws--app.modal.direct/v1/models"),
            (Modal, "https://ws--app.modal.direct/metrics?x=1"),
            (RunpodEndpoint, "https://abc.proxy.runpod.net/metrics"),
            (RunpodPod, "https://abc.api.runpod.ai/metrics"),
            (SourceKind::Local, "https://x.modal.direct/metrics"),
        ] {
            assert!(!is_cloud_metrics_url(kind, url), "{url}");
        }
    }

    #[test]
    fn the_job_gets_exactly_the_credentials_its_provider_needs() {
        let all = |n: &str| Some(format!("value-of-{n}"));
        let modal = credentials(SourceKind::Modal, &all, None).unwrap();
        let names: Vec<_> = modal.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, MODAL_ENV);
        let endpoint = credentials(SourceKind::RunpodEndpoint, &all, Some("k")).unwrap();
        assert_eq!(
            endpoint,
            [
                (
                    RUNPOD_KEY_ENV.to_owned(),
                    format!("value-of-{RUNPOD_KEY_ENV}")
                ),
                (GATEWAY_KEY_ENV.to_owned(), "k".to_owned()),
            ]
        );
        // A pod needs no management key, only the deployment's own.
        let pod = credentials(SourceKind::RunpodPod, &|_| None, Some("k")).unwrap();
        assert_eq!(pod, [(GATEWAY_KEY_ENV.to_owned(), "k".to_owned())]);
        // Missing pieces are named, and no value appears in the message.
        let partial = |n: &str| (n == "MODAL_TOKEN_ID").then(|| "ak-secret".to_owned());
        let err = credentials(SourceKind::Modal, &partial, None).unwrap_err();
        assert!(err.contains("MODAL_TOKEN_SECRET") && err.contains("MODAL_PROXY_TOKEN_ID"));
        assert!(!err.contains("ak-secret") && !err.contains("MODAL_TOKEN_ID and"));
        assert!(credentials(SourceKind::RunpodPod, &|_| None, None).is_err());
        assert!(credentials(SourceKind::RunpodEndpoint, &|_| Some(" ".into()), Some("k")).is_err());
    }

    #[tokio::test]
    async fn a_scaled_to_zero_deployment_is_never_scraped() {
        let (url, seen) = serve(vec![vllm(1, 10)]).await;
        let (up, asked) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut remote = remote(&url, &up, &asked, 1200);
        for t in [0, 60, 120] {
            let polled = remote.poll(t).await;
            assert!(polled.scrape.is_none());
        }
        assert_eq!(asked.load(Ordering::SeqCst), 3, "asked the control plane");
        assert!(seen.lock().unwrap().is_empty(), "never woke it");
    }

    #[tokio::test]
    async fn scrapes_carry_the_credential_and_pause_once_the_counters_stand_still() {
        let (url, seen) = serve(vec![vllm(1, 10), vllm(3, 50), vllm(3, 50), vllm(4, 60)]).await;
        let (up, asked) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut remote = remote(&url, &up, &asked, 1200);
        // Busy: every minute is scraped.
        assert!(remote.poll(0).await.scrape.is_some());
        assert!(remote.poll(60).await.scrape.is_some());
        // Nothing new since the last scrape: this one is the last for a while.
        assert!(remote.poll(120).await.scrape.is_some());
        assert_eq!(seen.lock().unwrap().len(), 3);
        for t in [180, 600, 1319] {
            assert!(remote.poll(t).await.scrape.is_none(), "paused at {t}");
        }
        assert_eq!(seen.lock().unwrap().len(), 3, "the pause sends no request");
        // The pause is over: the deployment is scraped again and has traffic.
        assert!(remote.poll(1320).await.scrape.is_some());
        assert_eq!(seen.lock().unwrap().len(), 4);
        let first = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(first.starts_with("get /metrics http/1.1"), "{first}");
        assert!(first.contains("authorization: bearer k-1"), "{first}");
    }

    #[tokio::test]
    async fn a_failing_scrape_backs_off_instead_of_hitting_the_container_every_minute() {
        let (url, seen) = serve_failing().await;
        let (up, asked) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut remote = remote(&url, &up, &asked, 1200);
        assert!(remote.poll(0).await.scrape.is_none());
        assert_eq!(seen.lock().unwrap().len(), 1);
        for t in [60, 600, 1199] {
            assert!(remote.poll(t).await.scrape.is_none(), "paused at {t}");
        }
        assert_eq!(seen.lock().unwrap().len(), 1, "the pause sends no request");
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "nor asks the control plane"
        );
        // Tried again once the pause is over.
        assert!(remote.poll(1200).await.scrape.is_none());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_deployment_that_scaled_down_starts_fresh_when_it_comes_back() {
        let (url, _) = serve(vec![vllm(5, 50), vllm(1, 5)]).await;
        let (up, asked) = (
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicUsize::new(0)),
        );
        let mut remote = remote(&url, &up, &asked, 1200);
        assert!(remote.poll(0).await.scrape.is_some());
        up.store(false, Ordering::SeqCst);
        assert!(remote.poll(60).await.scrape.is_none());
        up.store(true, Ordering::SeqCst);
        // Counters restarted: not read as "unchanged", so no pause is armed.
        assert!(remote.poll(120).await.scrape.is_some());
        assert_eq!(remote.paused_until, 0);
    }
}
