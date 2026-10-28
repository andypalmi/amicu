//! Google Cloud access for apply, destroy, refresh and orphans, behind a
//! trait so tests never touch the network.

use std::sync::Arc;

use amicu_core::secret::SecretValue;
use amicu_gcp::cloudrun::CloudRunTiming;
use amicu_gcp::compute::{VmRecord, probe_transport};
use amicu_gcp::{
    AdcTokenSource, AuthConfig, GceTiming, GcpClient, GcpError, NetworkRef, SubnetworkRef,
};
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
    /// The transport for requests to a Cloud Run service's own URL
    /// (Google-managed TLS, so nothing is pinned).
    fn service_transport(&self) -> Arc<dyn HttpTransport> {
        Arc::new(ReqwestTransport::new())
    }
    /// Poll and readiness bounds for Cloud Run.
    fn run_timing(&self) -> CloudRunTiming {
        CloudRunTiming::default()
    }
}

/// The post-deploy check of a Cloud Run service: an anonymous request is
/// refused and `key` is accepted. `Err` is secret-free.
pub(crate) async fn verify_service(
    ops: &dyn GcpOps,
    endpoint: &str,
    key: &SecretValue,
) -> Result<(), String> {
    verify_pinned(
        &*ops.service_transport(),
        endpoint,
        key,
        CheckTiming::PUBLIC,
    )
    .await
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

/// The post-deploy check of an IAM-protected Cloud Run service: an anonymous
/// request is refused. amicu holds no identity token for the service, so the
/// key itself is not exercised. `Err` is secret-free.
pub(crate) async fn verify_service_refuses(ops: &dyn GcpOps, endpoint: &str) -> Result<(), String> {
    let transport = ops.service_transport();
    let url = format!("{}{PROBE_PATH}", endpoint.trim_end_matches('/'));
    let timing = CheckTiming::PUBLIC;
    let deadline = tokio::time::Instant::now() + timing.reject_timeout;
    loop {
        match transport
            .execute(HttpRequest::new(HttpMethod::Get, url.clone()))
            .await
            .map(|r| r.status)
        {
            Ok(401 | 403) => return Ok(()),
            Ok(code) if (200..300).contains(&code) => {
                return Err(format!(
                    "{PROBE_PATH} answered an unauthenticated request ({code}); Cloud Run's IAM \
                     check is not in force"
                ));
            }
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "could not confirm that {PROBE_PATH} refuses unauthenticated requests before \
                 the deadline"
            ));
        }
        tokio::time::sleep(timing.interval).await;
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

/// What a Compute Engine launch needs from the project, beyond credentials.
pub(crate) struct ComputeNeeds<'a> {
    pub project: &'a str,
    pub impersonate: Option<&'a str>,
    /// The VM's runtime service account, when one is configured.
    pub service_account: Option<&'a str>,
    /// Whether the secrets go through Secret Manager.
    pub secret_manager: bool,
    /// The VPC the VM and firewall rule attach to, as configured.
    pub network: &'a NetworkChoice,
}

/// The `network` and `subnetwork` of a `gcp {}` block, and the region the VM
/// is placed in (what a bare subnetwork name refers to).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct NetworkChoice {
    pub network: Option<String>,
    pub subnetwork: Option<String>,
    pub region: Option<String>,
}

/// [`preflight`], then read-only checks of what `apply` is about to rely on:
/// the Secret Manager API is enabled, the network (and subnetwork) the VM and
/// firewall rule use exist, and the caller may act as the VM's service account.
/// Nothing is enabled, created or granted; each failure says what to run.
/// A permission-denied answer is inconclusive and passes: the write that needs
/// the permission then reports it.
pub(crate) async fn preflight_compute(
    ops: &dyn GcpOps,
    needs: &ComputeNeeds<'_>,
) -> Result<(), CliError> {
    let project = needs.project;
    preflight(ops, project, needs.impersonate).await?;
    let client = ops.client(needs.impersonate)?;
    let fail = |what: String| {
        CliError::Message(format!(
            "Google Cloud preflight for project {project} failed; nothing was started: {what}"
        ))
    };
    if needs.secret_manager {
        check_secret_manager(
            &client,
            project,
            "the VM's API key, Hugging Face token and TLS key are stored there",
            ", or set `insecure_metadata_secrets = true` to skip it (not recommended)",
        )
        .await
        .map_err(fail)?;
    }
    check_network(&client, project, needs.network)
        .await
        .map_err(fail)?;
    if let Some(sa) = needs.service_account {
        check_act_as(&client, sa, "the VM").await.map_err(fail)?;
    }
    Ok(())
}

/// The network the VM and firewall rule attach to exists, a custom-mode
/// network comes with a `subnetwork`, and that subnetwork exists, belongs to
/// the network and can give the VM an external IPv4 address. Read-only; a
/// permission-denied answer is inconclusive and passes.
async fn check_network(
    client: &GcpClient,
    project: &str,
    choice: &NetworkChoice,
) -> Result<(), String> {
    let base = amicu_gcp::client::COMPUTE_BASE_URL;
    let net = choice
        .network
        .as_deref()
        .map(NetworkRef::parse)
        .transpose()
        .map_err(|e| e.to_string())?;
    let (net_project, net_name) = net.as_ref().map_or((project, "default"), |n| {
        (n.project_or(project), n.name.as_str())
    });
    let url = format!("{base}/projects/{net_project}/global/networks/{net_name}");
    let network = match client.request(HttpMethod::Get, &url, None).await {
        Err(GcpError::NotFound { .. }) if net.is_none() => {
            return Err(format!(
                "the `default` network does not exist; the VM and the firewall rule amicu \
                 creates are attached to it. Either create it with `gcloud compute networks \
                 create default --subnet-mode=auto --project {project}`, or set `network` \
                 (and `subnetwork`) in the `gcp {{}}` block to a VPC you have"
            ));
        }
        Err(GcpError::NotFound { .. }) => {
            return Err(format!(
                "the network `{net_name}` does not exist in project {net_project}; set `network` \
                 to an existing VPC, or create one with `gcloud compute networks create \
                 {net_name} --subnet-mode=custom --project {net_project}`"
            ));
        }
        Err(GcpError::PermissionDenied { .. }) => serde_json::Value::Null,
        Err(e) => return Err(e.to_string()),
        Ok(v) => v,
    };
    let custom_mode = network["autoCreateSubnetworks"] == false;
    let sub_has_region = choice
        .subnetwork
        .as_deref()
        .and_then(|s| SubnetworkRef::parse(s).ok())
        .is_some_and(|s| s.region.is_some());
    let region = choice.region.as_deref().filter(|r| !r.is_empty());
    if region.is_none() && (custom_mode || choice.subnetwork.is_some()) && !sub_has_region {
        return Err(
            "set `region` (or `gcp { zone }`) so the VM's subnetwork can be looked up".to_owned(),
        );
    }
    let region = region.unwrap_or_default();
    let Some(sub) = choice.subnetwork.as_deref() else {
        if custom_mode {
            let here = format!("/regions/{region}/subnetworks/");
            let known: Vec<&str> = network["subnetworks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str())
                .filter(|s| s.contains(&here))
                .filter_map(|s| s.rsplit('/').next())
                .collect();
            let hint = if known.is_empty() {
                format!(
                    "it has no subnetwork in `{region}`; create one with `gcloud compute \
                     networks subnets create <name> --network {net_name} --region {region} \
                     --range <cidr> --project {net_project}`"
                )
            } else {
                format!("its subnetworks in `{region}` are: {}", known.join(", "))
            };
            return Err(format!(
                "the network `{net_name}` is custom-mode, so the VM needs a subnetwork: set \
                 `subnetwork` in the `gcp {{}}` block; {hint}"
            ));
        }
        return Ok(());
    };
    check_subnetwork(client, (net_project, net_name), region, sub).await
}

/// The subnetwork exists, belongs to the network, and has IPv4.
async fn check_subnetwork(
    client: &GcpClient,
    (net_project, net_name): (&str, &str),
    region: &str,
    sub: &str,
) -> Result<(), String> {
    let base = amicu_gcp::client::COMPUTE_BASE_URL;
    let sub = SubnetworkRef::parse(sub).map_err(|e| e.to_string())?;
    let sub_region = sub.region_or(region);
    let sub_project = sub.project_or(net_project);
    let url = format!(
        "{base}/projects/{sub_project}/regions/{sub_region}/subnetworks/{}",
        sub.name
    );
    let found = match client.request(HttpMethod::Get, &url, None).await {
        Err(GcpError::NotFound { .. }) => {
            return Err(format!(
                "the subnetwork `{0}` does not exist in `{sub_region}` of project {sub_project}; \
                 set `subnetwork` to an existing one, or create it with `gcloud compute networks \
                 subnets create {0} --network {net_name} --region {sub_region} --range <cidr> \
                 --project {sub_project}`",
                sub.name
            ));
        }
        Err(GcpError::PermissionDenied { .. }) => return Ok(()),
        Err(e) => return Err(e.to_string()),
        Ok(v) => v,
    };
    let want = format!("/projects/{net_project}/global/networks/{net_name}");
    if let Some(actual) = found["network"].as_str()
        && !actual.ends_with(&want)
    {
        return Err(format!(
            "the subnetwork `{}` belongs to the network `{}`, not `{net_name}`; set `network` \
             and `subnetwork` to a matching pair",
            sub.name,
            actual.rsplit('/').next().unwrap_or(actual)
        ));
    }
    if let Some(purpose) = found["purpose"].as_str().filter(|p| *p != "PRIVATE") {
        return Err(format!(
            "the subnetwork `{}` has purpose {purpose}, which cannot host a VM; use a regular \
             (PRIVATE) subnetwork",
            sub.name
        ));
    }
    if found["stackType"] == "IPV6_ONLY" {
        return Err(format!(
            "the subnetwork `{}` is IPv6-only, so the VM cannot get the external IPv4 address \
             amicu connects to; amicu does not support VMs without an external IP yet. Use a \
             subnetwork with IPv4",
            sub.name
        ));
    }
    Ok(())
}

/// Read-only checks before a Cloud Run launch writes anything: credentials,
/// project and the Cloud Run API (one list call in the region), the Secret
/// Manager API, and that the caller may act as the service's runtime account
/// (`services.create` needs `iam.serviceAccounts.actAs` on it).
pub(crate) async fn preflight_cloud_run(
    ops: &dyn GcpOps,
    project: &str,
    region: &str,
    impersonate: Option<&str>,
    service_account: &str,
) -> Result<(), CliError> {
    let client = ops.client(impersonate)?;
    let fail = |what: String| {
        CliError::Message(format!(
            "Google Cloud preflight for Cloud Run in project {project} ({region}) failed; \
             nothing was started: {what}"
        ))
    };
    let url = format!(
        "{}/projects/{project}/locations/{region}/services?pageSize=1",
        amicu_gcp::RUN_BASE_URL
    );
    match client.request(HttpMethod::Get, &url, None).await {
        Err(e) if e.is_service_disabled() => {
            return Err(fail(format!(
                "the Cloud Run Admin API is not enabled. Enable it with `gcloud services \
                 enable run.googleapis.com --project {project}`"
            )));
        }
        Err(e) => return Err(fail(e.to_string())),
        Ok(_) => {}
    }
    check_secret_manager(
        &client,
        project,
        "the service's API key and Hugging Face token are stored there",
        "",
    )
    .await
    .map_err(fail)?;
    check_act_as(&client, service_account, "the Cloud Run service")
        .await
        .map_err(fail)
}

/// The Secret Manager API is enabled in `project`. A permission-denied answer
/// is inconclusive and passes.
async fn check_secret_manager(
    client: &GcpClient,
    project: &str,
    why: &str,
    alternative: &str,
) -> Result<(), String> {
    let url = format!(
        "{}/projects/{project}/secrets?pageSize=1",
        amicu_gcp::SECRETMANAGER_BASE_URL
    );
    match client.request(HttpMethod::Get, &url, None).await {
        Err(e) if e.is_service_disabled() => Err(format!(
            "the Secret Manager API is not enabled; {why}. Enable it with `gcloud services \
             enable secretmanager.googleapis.com --project {project}`{alternative}"
        )),
        Err(GcpError::PermissionDenied { .. }) | Ok(_) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

const ACT_AS: &str = "iam.serviceAccounts.actAs";

/// Whether the caller holds `iam.serviceAccounts.actAs` on `sa`, via
/// `testIamPermissions` (a read: it changes nothing). Any answer other than a
/// clear "no" passes.
async fn check_act_as(client: &GcpClient, sa: &str, runner: &str) -> Result<(), String> {
    let url =
        format!("https://iam.googleapis.com/v1/projects/-/serviceAccounts/{sa}:testIamPermissions");
    let body = serde_json::json!({ "permissions": [ACT_AS] });
    match client.request(HttpMethod::Post, &url, Some(body)).await {
        Ok(v) => {
            let granted = v["permissions"]
                .as_array()
                .is_some_and(|p| p.iter().any(|p| p == ACT_AS));
            if granted {
                Ok(())
            } else {
                Err(format!(
                    "the caller cannot act as `{sa}` ({ACT_AS}), which {runner} needs to run as \
                    it. Grant `roles/iam.serviceAccountUser` on that service account to the \
                    account amicu runs as"
                ))
            }
        }
        Err(GcpError::NotFound { .. }) => Err(format!(
            "the service account `{sa}` does not exist; create it first (see the GCP provider docs)"
        )),
        Err(_) => Ok(()),
    }
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
