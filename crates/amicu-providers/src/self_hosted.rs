//! The self-hosted / bare-metal [`Provider`] adapter over operator-declared
//! hardware.
//!
//! Unlike the cloud adapters, self-hosted has **no cloud API and no fixed
//! catalog**. It represents existing hardware - bare-metal boxes and existing
//! clusters - that an operator declares directly: each [`DeclaredHost`] carries
//! the machine's resources and, crucially, the [`Region`] the operator states
//! their hardware physically sits in. **Data residency is therefore derived
//! straight from the declared location metadata**, not from a cloud catalog: a
//! host declared in `Residency::Eu` yields an EU region, one declared in
//! `Residency::Us` a US region, and [`ProviderCapabilities::eu_regions`] reports
//! only the EU ones.
//!
//! Because the operator vouches for the jurisdiction, there is **no residency
//! filter**: self-hosted hardware can sit in any jurisdiction the operator
//! declares. The descriptor reflects exactly the declared hosts - every host
//! becomes a [`ComputeOffering`] sold in that host's declared region.
//!
//! Hosts are provisioned over SSH ([`ProvisioningMethod::Ssh`],
//! [`CredentialsModel::SshKey`]) through the [`SshTransport`] seam:
//! [`provision`](SelfHostedProvider::provision) validates that the request's
//! offering and region correspond to a declared host, resolves the SSH key
//! transiently, then runs the host prep/registration command over SSH;
//! [`status`](SelfHostedProvider::status) queries host liveness and
//! [`teardown`](SelfHostedProvider::teardown) runs deregistration/cleanup. The
//! adapter never touches a concrete SSH client - production uses
//! [`RusshTransport`], tests inject a mock - so the provisioning path is
//! exercised with no network, credentials, or spend. A failed SSH exchange
//! ([`SshTransportError`]) is distinct from a command that ran and returned a
//! non-zero exit status: auth failure maps to
//! [`CredentialsRejected`](ProviderError::CredentialsRejected), connection/timeout
//! to a retryable [`Backend`](ProviderError::Backend), and a non-zero exit to a
//! [`Backend`](ProviderError::Backend) (or
//! [`UnknownInstance`](ProviderError::UnknownInstance) when the host reports the
//! instance is not found). Declared hourly costs are amortized/own-hardware
//! figures, not a provider quote (and default to zero).

use std::sync::Arc;

use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_core::resource::{Accelerator, Money};
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use amicu_core::units::ByteSize;
use async_trait::async_trait;

use crate::capability::{
    Availability, AvailabilityState, BillingGranularity, ComputeOffering, CredentialsModel,
    LifecycleBehavior, ModelDownloadPerformance, NetworkCapabilities, OrchestrationSupport,
    Pricing, ProviderCapabilities, ProvisioningMethod, Rate, RateUnit, Region, StorageKind,
    StorageOption,
};
use crate::provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};
use crate::ssh::{CommandOutput, RusshTransport, SshTarget, SshTransport, SshTransportError};

/// The adapter identity the self-hosted provider registers under.
pub const PROVIDER_ID: &str = "self-hosted";

/// The remote agent invoked over SSH to manage a declared host.
const HOST_AGENT: &str = "amicu-host";

/// Exit status the host agent uses to signal that an instance is not known
/// (mapped to [`ProviderError::UnknownInstance`], the SSH analogue of HTTP 404).
const EXIT_NOT_FOUND: i32 = 44;

/// Ambiguity in the operator-declared hosts backing a [`SelfHostedProvider`].
///
/// Surfaced by the fallible constructors
/// ([`try_from_hosts`](SelfHostedProvider::try_from_hosts) /
/// [`try_with_host`](SelfHostedProvider::try_with_host)) so operator-supplied
/// ambiguity is rejected rather than silently mis-served.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelfHostedConfigError {
    /// Two declared hosts share the same id. Host ids are unique identities: a
    /// collision produces colliding offering ids and mis-serves provisioning.
    #[error("duplicate declared-host id '{id}'")]
    DuplicateHostId { id: String },
    /// One region code is declared with two different residencies. A region code
    /// is a unique location identity; conflicting residencies cannot both hold.
    #[error("region code '{code}' declared with conflicting residency")]
    ConflictingRegionResidency { code: String },
}

/// An operator-declared existing machine.
///
/// This is where residency comes from: the operator states the [`Region`] their
/// hardware physically sits in (its code, its
/// [`Residency`](crate::capability::Residency), and an optional country /
/// display name), and that declared metadata flows straight into the descriptor.
/// The remaining fields describe the box: an optional [`Accelerator`], host
/// vCPUs / memory, whether it fronts an existing cluster, and an optional
/// declared hourly cost (amortized own-hardware; defaults to 0).
#[derive(Debug, Clone)]
pub struct DeclaredHost {
    /// Operator-assigned host id; also the [`ComputeOffering`] id it becomes.
    id: String,
    /// The region the operator declares the hardware sits in - the source of
    /// this host's residency.
    region: Region,
    /// Accelerator (GPU model / count / VRAM), if the host has one.
    accelerator: Option<Accelerator>,
    /// Host vCPU count, if declared.
    vcpus: Option<u32>,
    /// Host system memory, if declared.
    memory: Option<ByteSize>,
    /// Declared hourly cost in EUR (amortized / own-hardware). Zero by default.
    hourly_cost: f64,
    /// Whether this host represents an existing cluster (adds managed-Kubernetes
    /// orchestration and a shared network filesystem to the descriptor).
    cluster: bool,
    /// SSH host/address to connect to. Defaults to the host `id` when unset.
    address: Option<String>,
    /// SSH login user.
    user: String,
    /// SSH TCP port.
    ssh_port: u16,
}

impl DeclaredHost {
    /// A declared host with `id`, sitting in the operator-declared `region`, and
    /// no accelerator, resources, cost, or cluster role yet.
    #[must_use]
    pub fn new(id: impl Into<String>, region: Region) -> Self {
        Self {
            id: id.into(),
            region,
            accelerator: None,
            vcpus: None,
            memory: None,
            hourly_cost: 0.0,
            cluster: false,
            address: None,
            user: "amicu".into(),
            ssh_port: 22,
        }
    }

    /// Declare the host's SSH address and login user. The address defaults to the
    /// host id and the user to `amicu` when this is not called.
    #[must_use]
    pub fn with_ssh(mut self, address: impl Into<String>, user: impl Into<String>) -> Self {
        self.address = Some(address.into());
        self.user = user.into();
        self
    }

    /// Declare a non-default SSH port.
    #[must_use]
    pub fn with_ssh_port(mut self, port: u16) -> Self {
        self.ssh_port = port;
        self
    }

    /// The SSH target for this host: its address (or id) and login user/port.
    fn ssh_target(&self) -> SshTarget {
        let address = self.address.clone().unwrap_or_else(|| self.id.clone());
        SshTarget::new(address, self.user.clone()).with_port(self.ssh_port)
    }

    /// Declare the host's accelerator.
    #[must_use]
    pub fn with_accelerator(mut self, accelerator: Accelerator) -> Self {
        self.accelerator = Some(accelerator);
        self
    }

    /// Declare the host's vCPU count and system memory.
    #[must_use]
    pub fn with_resources(mut self, vcpus: u32, memory: ByteSize) -> Self {
        self.vcpus = Some(vcpus);
        self.memory = Some(memory);
        self
    }

    /// Declare the host's amortized hourly cost in EUR. Non-finite or negative
    /// values are ignored (a `NaN` would poison cost ranking).
    #[must_use]
    pub fn with_hourly_cost(mut self, hourly_cost: f64) -> Self {
        if hourly_cost.is_finite() && hourly_cost >= 0.0 {
            self.hourly_cost = hourly_cost;
        }
        self
    }

    /// Mark the host as fronting an existing cluster.
    #[must_use]
    pub fn as_cluster(mut self) -> Self {
        self.cluster = true;
        self
    }
}

/// The self-hosted / bare-metal provider adapter.
///
/// Built from operator-declared hosts; its descriptor reflects exactly those
/// hosts and derives residency from their declared regions. Provisioning runs
/// real host-management commands over the [`SshTransport`] seam.
pub struct SelfHostedProvider {
    id: AdapterId,
    hosts: Vec<DeclaredHost>,
    capabilities: ProviderCapabilities,
    health: HealthStatus,
    transport: Arc<dyn SshTransport>,
}

impl SelfHostedProvider {
    /// A self-hosted provider with no declared hosts. Add hosts with
    /// [`with_host`](Self::with_host); provisioning against an empty provider
    /// yields [`ProviderError::UnknownOffering`].
    #[must_use]
    pub fn new() -> Self {
        Self::from_hosts(Vec::new())
    }

    /// A self-hosted provider built from `hosts`.
    ///
    /// Infallible and best-effort: host ids and region codes are treated as
    /// unique identities on a documented first-wins precondition. Use
    /// [`try_from_hosts`](Self::try_from_hosts) to enforce that precondition and
    /// reject ambiguous input.
    #[must_use]
    pub fn from_hosts(hosts: Vec<DeclaredHost>) -> Self {
        let id = AdapterId::new(PROVIDER_ID);
        let health = HealthStatus::Healthy;
        let capabilities = Self::capabilities_for(&id, &hosts, &health);
        Self {
            id,
            hosts,
            capabilities,
            health,
            transport: Arc::new(RusshTransport::new()),
        }
    }

    /// A self-hosted provider built from validated `hosts`.
    ///
    /// The enforcing counterpart to [`from_hosts`](Self::from_hosts): duplicate
    /// declared-host ids, or a region code declared with conflicting residency,
    /// are rejected as [`SelfHostedConfigError`] rather than silently
    /// mis-served.
    pub fn try_from_hosts(hosts: Vec<DeclaredHost>) -> Result<Self, SelfHostedConfigError> {
        Self::validate_hosts(&hosts)?;
        Ok(Self::from_hosts(hosts))
    }

    /// Add a declared host, rebuilding the descriptor. Host ids and region codes
    /// are treated as unique identities: re-declaring an id, or a region code
    /// with differing residency, keeps the first declaration. Use
    /// [`try_with_host`](Self::try_with_host) to reject such ambiguity instead.
    #[must_use]
    pub fn with_host(mut self, host: DeclaredHost) -> Self {
        self.hosts.push(host);
        self.capabilities = Self::capabilities_for(&self.id, &self.hosts, &self.health);
        self
    }

    /// Add a declared host, validating the resulting host set.
    ///
    /// The enforcing counterpart to [`with_host`](Self::with_host): a host whose
    /// id or region-code residency conflicts with an already-declared host is
    /// rejected as [`SelfHostedConfigError`], leaving the caller to handle it.
    pub fn try_with_host(mut self, host: DeclaredHost) -> Result<Self, SelfHostedConfigError> {
        self.hosts.push(host);
        Self::validate_hosts(&self.hosts)?;
        self.capabilities = Self::capabilities_for(&self.id, &self.hosts, &self.health);
        Ok(self)
    }

    /// Validate the declared hosts backing this provider, reporting the first
    /// duplicate id or conflicting region residency. The infallible builders
    /// accept ambiguous input on the documented first-wins precondition; call
    /// this (or use [`try_from_hosts`](Self::try_from_hosts)) to enforce it.
    pub fn validate(&self) -> Result<(), SelfHostedConfigError> {
        Self::validate_hosts(&self.hosts)
    }

    /// Reject duplicate host ids and region codes declared with conflicting
    /// residency. Returns the first conflict found.
    fn validate_hosts(hosts: &[DeclaredHost]) -> Result<(), SelfHostedConfigError> {
        let mut seen_ids: Vec<&str> = Vec::new();
        let mut seen_regions: Vec<&Region> = Vec::new();
        for host in hosts {
            if seen_ids.contains(&host.id.as_str()) {
                return Err(SelfHostedConfigError::DuplicateHostId {
                    id: host.id.clone(),
                });
            }
            seen_ids.push(&host.id);
            match seen_regions.iter().find(|r| r.code == host.region.code) {
                Some(prev) if prev.residency != host.region.residency => {
                    return Err(SelfHostedConfigError::ConflictingRegionResidency {
                        code: host.region.code.clone(),
                    });
                }
                Some(_) => {}
                None => seen_regions.push(&host.region),
            }
        }
        Ok(())
    }

    /// Override the health status (e.g. for tests or a degraded probe result).
    #[must_use]
    pub fn with_health(mut self, health: HealthStatus) -> Self {
        self.capabilities.health = health.clone();
        self.health = health;
        self
    }

    /// Swap in an SSH transport. Test-only: adapter tests inject a mock so the
    /// provisioning path runs with no network, credentials, or spend.
    #[cfg(test)]
    fn with_transport(mut self, transport: Arc<dyn SshTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Build the descriptor from the declared `hosts`. Each host becomes a
    /// [`ComputeOffering`] sold in its declared region, and each declared region
    /// (deduplicated by code) carries the residency the operator stated.
    fn capabilities_for(
        id: &AdapterId,
        hosts: &[DeclaredHost],
        health: &HealthStatus,
    ) -> ProviderCapabilities {
        let mut regions: Vec<Region> = Vec::new();
        for host in hosts {
            if !regions.iter().any(|r| r.code == host.region.code) {
                regions.push(host.region.clone());
            }
        }
        let has_cluster = hosts.iter().any(|h| h.cluster);

        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::Ssh,
            CredentialsModel::SshKey,
            BillingGranularity::PerHour,
            Availability::new(AvailabilityState::Available),
            health.clone(),
        );
        caps.regions = regions;
        caps.compute = hosts
            .iter()
            .map(|h| ComputeOffering {
                id: h.id.clone(),
                accelerator: h.accelerator.clone(),
                vcpus: h.vcpus,
                memory: h.memory,
                regions: vec![h.region.code.clone()],
                // Own hardware has no provider price: on-demand only, at the
                // declared (amortized) hourly cost in EUR.
                pricing: Pricing::on_demand(Rate::new(
                    Money::new(h.hourly_cost, "EUR"),
                    RateUnit::PerHour,
                )),
                availability: Availability::new(AvailabilityState::Available),
            })
            .collect();
        caps.storage = if has_cluster {
            vec![
                StorageOption::new(StorageKind::LocalNvme),
                StorageOption::new(StorageKind::NetworkFs),
            ]
        } else {
            vec![StorageOption::new(StorageKind::LocalNvme)]
        };
        caps.network = NetworkCapabilities {
            bandwidth_mbps: None,
            private_network: true,
        };
        caps.orchestration = OrchestrationSupport {
            managed_kubernetes: has_cluster,
            bare_metal: true,
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(60),
            teardown_secs: Some(30),
            supports_hibernation: false,
        };
        caps.model_download = ModelDownloadPerformance::default();
        caps
    }

    /// The declared hosts backing this provider.
    #[must_use]
    pub fn declared_host_count(&self) -> usize {
        self.hosts.len()
    }

    fn has_offering(&self, offering_id: &str) -> bool {
        self.capabilities
            .compute
            .iter()
            .any(|o| o.id == offering_id)
    }

    /// The declared host backing `offering_id`, if any. Used to build the SSH
    /// target for provisioning and lifecycle commands.
    fn host_for(&self, offering_id: &str) -> Option<&DeclaredHost> {
        self.hosts.iter().find(|h| h.id == offering_id)
    }

    /// Whether the offering is sold in `region`. Each self-hosted offering is
    /// sold only in its declared host's region, so this rejects a request whose
    /// region does not correspond to the declared host. Considers every offering
    /// with the id so a duplicate id (accepted by the infallible builder) is not
    /// mis-rejected when provisioned in a later host's region.
    fn offering_serves_region(&self, offering_id: &str, region: &str) -> bool {
        self.capabilities
            .compute
            .iter()
            .filter(|o| o.id == offering_id)
            .any(|o| o.regions.iter().any(|r| r == region))
    }

    /// Resolve the SSH key to a transient [`SecretValue`] for a single set of
    /// lifecycle commands.
    ///
    /// The returned value authenticates the SSH connection and is dropped
    /// (zeroized) when the caller returns; it is never stored on a
    /// [`ProvisioningHandle`], logged, or serialized. Self-hosted provisioning
    /// requires an SSH key: a missing, empty, or non-UTF-8 credential is rejected
    /// as [`ProviderError::CredentialsRejected`] before any SSH call.
    async fn resolve_key(
        &self,
        resolver: &dyn SecretResolver,
        credentials: Option<&SecretRef>,
    ) -> Result<SecretValue, AdapterError> {
        let Some(secret_ref) = credentials else {
            return Err(AdapterError::from_kind(
                ProviderError::CredentialsRejected {
                    detail: "self-hosted provisioning requires an SSH key".into(),
                },
            ));
        };
        let value = resolver.resolve(secret_ref).await?;
        if value.is_empty() {
            return Err(AdapterError::from_kind(
                ProviderError::CredentialsRejected {
                    detail: "empty self-hosted SSH key".into(),
                },
            ));
        }
        // Reject a non-UTF-8 key up front so the failure is a permanent
        // credential error, not a transport error.
        value.expose_secret_str().map_err(|_| {
            AdapterError::from_kind(ProviderError::CredentialsRejected {
                detail: "self-hosted SSH key is not valid UTF-8".into(),
            })
        })?;
        Ok(value)
    }
}

impl Default for SelfHostedProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for SelfHostedProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for SelfHostedProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

/// POSIX single-quote a value interpolated into a remote shell command, so an
/// argument cannot break out and inject. Wrap in `'...'`, rendering each embedded
/// `'` as `'\''`.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Fold an SSH transport failure (the exchange never completed) onto a typed
/// error: an auth failure is a permanent credential rejection, a
/// connection/timeout a retryable backend fault.
fn transport_error(err: &SshTransportError) -> AdapterError {
    let kind = match err {
        SshTransportError::Auth(detail) => ProviderError::CredentialsRejected {
            detail: format!("self-hosted SSH authentication failed: {detail}"),
        },
        SshTransportError::Connection(detail) => ProviderError::Backend {
            detail: format!("self-hosted SSH connection failure: {detail}"),
        },
        SshTransportError::Timeout => ProviderError::Backend {
            detail: "self-hosted SSH exchange timed out".into(),
        },
    };
    AdapterError::from_kind(kind)
}

/// Map a lifecycle command that ran but exited non-zero onto a typed error: the
/// [`EXIT_NOT_FOUND`] sentinel means the instance is unknown, any other non-zero
/// exit is a retryable backend fault carrying a secret-free `stderr` summary.
fn command_error(output: &CommandOutput, id: &str) -> AdapterError {
    let kind = if output.exit_status == EXIT_NOT_FOUND {
        ProviderError::UnknownInstance { id: id.into() }
    } else {
        ProviderError::Backend {
            detail: format!(
                "self-hosted host agent exited {} ({})",
                output.exit_status,
                output.stderr_summary()
            ),
        }
    };
    AdapterError::from_kind(kind)
}

/// Map the host agent's liveness token (on `stdout`) onto a [`ProvisioningState`].
fn map_status(stdout: &[u8]) -> ProvisioningState {
    match String::from_utf8_lossy(stdout).trim() {
        "ready" => ProvisioningState::Ready {
            detail: "host ready".into(),
        },
        "provisioning" | "pending" => ProvisioningState::Provisioning {
            detail: "host preparing".into(),
        },
        "stopped" => ProvisioningState::Stopped {
            detail: "host stopped".into(),
        },
        "terminated" => ProvisioningState::Terminated {
            detail: "host deregistered".into(),
        },
        "" => ProvisioningState::Provisioning {
            detail: "host status unknown".into(),
        },
        other => ProvisioningState::Provisioning {
            detail: format!("host status {other}"),
        },
    }
}

#[async_trait]
impl Provider for SelfHostedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    async fn provision(
        &self,
        request: &ProvisionRequest,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningHandle, AdapterError> {
        if !self.has_offering(&request.offering_id) {
            return Err(AdapterError::from_kind(ProviderError::UnknownOffering {
                offering: request.offering_id.clone(),
            }));
        }
        // The offering and region must correspond to a declared host, before any
        // SSH call: an offering registered against a different declared region is
        // rejected.
        if !self.offering_serves_region(&request.offering_id, &request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        let key = self
            .resolve_key(resolver, request.credentials.as_ref())
            .await?;
        let target = self
            .host_for(&request.offering_id)
            .ok_or_else(|| {
                AdapterError::from_kind(ProviderError::UnknownOffering {
                    offering: request.offering_id.clone(),
                })
            })?
            .ssh_target();

        // Prepare/register the declared host over SSH.
        let command = format!(
            "{HOST_AGENT} provision --offering {} --region {}",
            sh_quote(&request.offering_id),
            sh_quote(&request.region)
        );
        let output = self
            .transport
            .run(&target, &command, &key)
            .await
            .map_err(|e| transport_error(&e))?;
        if !output.is_success() {
            return Err(AdapterError::from_kind(ProviderError::Backend {
                detail: format!(
                    "self-hosted host prep exited {} ({})",
                    output.exit_status,
                    output.stderr_summary()
                ),
            }));
        }
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if id.is_empty() {
            return Err(AdapterError::from_kind(ProviderError::Backend {
                detail: "self-hosted host agent did not report an instance id".into(),
            }));
        }
        Ok(ProvisioningHandle {
            id,
            provider: self.id.clone(),
            region: request.region.clone(),
            zone: None,
            offering_id: request.offering_id.clone(),
            credentials: request.credentials.clone(),
            state: ProvisioningState::Ready {
                detail: "host ready".into(),
            },
        })
    }

    async fn status(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningState, AdapterError> {
        let key = self
            .resolve_key(resolver, handle.credentials.as_ref())
            .await?;
        let target = self
            .host_for(&handle.offering_id)
            .ok_or_else(|| {
                AdapterError::from_kind(ProviderError::UnknownInstance {
                    id: handle.id.clone(),
                })
            })?
            .ssh_target();
        let command = format!("{HOST_AGENT} status {}", sh_quote(&handle.id));
        let output = self
            .transport
            .run(&target, &command, &key)
            .await
            .map_err(|e| transport_error(&e))?;
        if !output.is_success() {
            return Err(command_error(&output, &handle.id));
        }
        Ok(map_status(&output.stdout))
    }

    async fn teardown(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<(), AdapterError> {
        let key = self
            .resolve_key(resolver, handle.credentials.as_ref())
            .await?;
        let target = self
            .host_for(&handle.offering_id)
            .ok_or_else(|| {
                AdapterError::from_kind(ProviderError::UnknownInstance {
                    id: handle.id.clone(),
                })
            })?
            .ssh_target();
        let command = format!("{HOST_AGENT} teardown {}", sh_quote(&handle.id));
        let output = self
            .transport
            .run(&target, &command, &key)
            .await
            .map_err(|e| transport_error(&e))?;
        if !output.is_success() {
            return Err(command_error(&output, &handle.id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Residency;
    use crate::ssh::MockSshTransport;
    use amicu_core::adapter::Registry;
    use amicu_core::diagnostic::{Category as DiagCategory, DiagnosticCode};
    use amicu_core::resource::AcceleratorKind;
    use amicu_core::secret::SecretValue;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[derive(Default)]
    struct TestResolver {
        secrets: HashMap<String, SecretValue>,
    }

    impl TestResolver {
        fn with(mut self, name: &str, value: &str) -> Self {
            self.secrets
                .insert(name.into(), SecretValue::new(value.to_string()));
            self
        }

        fn with_bytes(mut self, name: &str, value: Vec<u8>) -> Self {
            self.secrets.insert(name.into(), SecretValue::new(value));
            self
        }
    }

    #[async_trait]
    impl SecretResolver for TestResolver {
        async fn resolve(&self, secret: &SecretRef) -> Result<SecretValue, AdapterError> {
            self.secrets.get(&secret.name).cloned().ok_or_else(|| {
                AdapterError::permanent(
                    DiagnosticCode::new(DiagCategory::Policy, 10),
                    format!("secret '{}' unavailable", secret.name),
                )
            })
        }
    }

    fn no_creds() -> TestResolver {
        TestResolver::default()
    }

    /// A resolver holding a valid (opaque, non-empty UTF-8) SSH key named
    /// `ssh_key`.
    fn key_resolver() -> TestResolver {
        TestResolver::default().with("ssh_key", "PRIVATE-KEY-MATERIAL")
    }

    /// A provision request for the DE host, binding the `ssh_key` credential.
    fn de_request() -> ProvisionRequest {
        ProvisionRequest::new("rack-de-01", "dc-frankfurt")
            .with_credentials(SecretRef::new("ssh_key"))
    }

    /// A DE provider whose SSH transport is the given mock.
    fn mock_provider(mock: Arc<MockSshTransport>) -> SelfHostedProvider {
        SelfHostedProvider::new()
            .with_host(de_host())
            .with_transport(mock)
    }

    /// A mock that drives the full happy-path lifecycle: provision reports an
    /// instance id, status reports `ready`, teardown succeeds.
    fn lifecycle_mock() -> Arc<MockSshTransport> {
        Arc::new(
            MockSshTransport::default()
                .on(
                    "provision",
                    CommandOutput::new(0, b"sh-inst-1".to_vec(), Vec::new()),
                )
                .on(
                    "status",
                    CommandOutput::new(0, b"ready".to_vec(), Vec::new()),
                )
                .on("teardown", CommandOutput::new(0, Vec::new(), Vec::new())),
        )
    }

    /// A declared GPU host in Germany (EU residency).
    fn de_host() -> DeclaredHost {
        DeclaredHost::new(
            "rack-de-01",
            Region::new("dc-frankfurt", Residency::Eu)
                .with_country("DE")
                .with_display_name("Frankfurt"),
        )
        .with_accelerator(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "a100".into(),
            count: 1,
            memory: "80GiB".parse().unwrap(),
        })
        .with_resources(32, "256GiB".parse().unwrap())
        .with_hourly_cost(1.5)
    }

    /// A declared host in the US (US residency).
    fn us_host() -> DeclaredHost {
        DeclaredHost::new(
            "rack-us-01",
            Region::new("dc-virginia", Residency::Us)
                .with_country("US")
                .with_display_name("Virginia"),
        )
        .with_resources(16, "128GiB".parse().unwrap())
    }

    fn two_region_provider() -> SelfHostedProvider {
        SelfHostedProvider::new()
            .with_host(de_host())
            .with_host(us_host())
    }

    #[test]
    fn try_from_hosts_rejects_duplicate_host_id() {
        // Same id declared in two different regions.
        let dup = DeclaredHost::new("rack-de-01", Region::new("dc-other", Residency::Eu));
        let err = SelfHostedProvider::try_from_hosts(vec![de_host(), dup])
            .err()
            .expect("duplicate id rejected");
        assert_eq!(
            err,
            SelfHostedConfigError::DuplicateHostId {
                id: "rack-de-01".into()
            }
        );
    }

    #[test]
    fn try_from_hosts_rejects_conflicting_region_residency() {
        // One region code declared with two different residencies.
        let a = DeclaredHost::new("h1", Region::new("dc-x", Residency::Eu));
        let b = DeclaredHost::new("h2", Region::new("dc-x", Residency::Us));
        let err = SelfHostedProvider::try_from_hosts(vec![a, b])
            .err()
            .expect("conflicting residency rejected");
        assert_eq!(
            err,
            SelfHostedConfigError::ConflictingRegionResidency {
                code: "dc-x".into()
            }
        );
    }

    #[test]
    fn try_from_hosts_accepts_valid_hosts() {
        let p = SelfHostedProvider::try_from_hosts(vec![de_host(), us_host()])
            .expect("valid hosts accepted");
        assert_eq!(p.declared_host_count(), 2);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn same_region_code_and_residency_across_hosts_is_allowed() {
        // Two hosts sitting in the same region (same code and residency) is a
        // valid declaration, not a conflict.
        let a = DeclaredHost::new("h1", Region::new("dc-x", Residency::Eu));
        let b = DeclaredHost::new("h2", Region::new("dc-x", Residency::Eu));
        let p = SelfHostedProvider::try_from_hosts(vec![a, b]).expect("same region ok");
        assert_eq!(p.capabilities().regions.len(), 1);
    }

    #[test]
    fn try_with_host_rejects_conflicting_addition() {
        let err = SelfHostedProvider::new()
            .with_host(de_host())
            .try_with_host(DeclaredHost::new(
                "rack-de-01",
                Region::new("dc-other", Residency::Us),
            ))
            .err()
            .expect("duplicate id on add rejected");
        assert_eq!(
            err,
            SelfHostedConfigError::DuplicateHostId {
                id: "rack-de-01".into()
            }
        );
    }

    #[tokio::test]
    async fn duplicate_id_via_infallible_builder_still_serves_second_region() {
        // The infallible builder accepts a duplicate id on its first-wins
        // precondition; the hardened region check no longer mis-rejects the
        // second host's region as UnknownRegion.
        let dup = DeclaredHost::new("rack-de-01", Region::new("dc-virginia", Residency::Us))
            .with_resources(8, "64GiB".parse().unwrap());
        let p = SelfHostedProvider::new()
            .with_host(de_host())
            .with_host(dup)
            .with_transport(lifecycle_mock());
        // The infallible provider is inspectably invalid.
        assert!(p.validate().is_err());
        let req = ProvisionRequest::new("rack-de-01", "dc-virginia")
            .with_credentials(SecretRef::new("ssh_key"));
        let handle = p
            .provision(&req, &key_resolver())
            .await
            .expect("second host's region now serves");
        assert!(handle.state.is_ready());
    }

    #[test]
    fn residency_is_derived_from_declared_location() {
        let caps = two_region_provider().capabilities();
        assert_eq!(caps.provider, AdapterId::new("self-hosted"));

        // Each declared region carries the residency the operator stated -
        // residency is derived from the declaration, not a cloud catalog.
        let de = caps
            .regions
            .iter()
            .find(|r| r.code == "dc-frankfurt")
            .expect("declared DE region present");
        assert_eq!(de.residency, Residency::Eu);
        let us = caps
            .regions
            .iter()
            .find(|r| r.code == "dc-virginia")
            .expect("declared US region present");
        assert_eq!(us.residency, Residency::Us);

        // eu_regions() derives over the declared EU hosts only.
        let eu: Vec<String> = caps.eu_regions().map(|r| r.code.clone()).collect();
        assert_eq!(eu, ["dc-frankfurt"]);
    }

    #[test]
    fn declared_host_becomes_offering_with_declared_cost() {
        let caps = SelfHostedProvider::new()
            .with_host(de_host())
            .capabilities();
        assert_eq!(caps.billing_granularity, BillingGranularity::PerHour);
        let offering = caps
            .compute
            .iter()
            .find(|o| o.id == "rack-de-01")
            .expect("declared host offering present");
        assert_eq!(offering.regions, ["dc-frankfurt"]);
        assert_eq!(offering.vcpus, Some(32));
        assert!(!offering.pricing.has_spot());
        let rate = offering.pricing.on_demand_rate();
        assert_eq!(rate.price, Money::new(1.5, "EUR"));
        assert_eq!(rate.unit, RateUnit::PerHour);
        assert_eq!(
            offering.accelerator.as_ref().map(|a| a.model.as_str()),
            Some("a100")
        );
    }

    #[test]
    fn default_hourly_cost_is_zero() {
        let caps = SelfHostedProvider::new()
            .with_host(us_host())
            .capabilities();
        let offering = &caps.compute[0];
        assert_eq!(
            offering.pricing.on_demand_rate().price,
            Money::new(0.0, "EUR")
        );
    }

    #[test]
    fn with_host_after_with_health_preserves_descriptor_health() {
        // with_host rebuilds the descriptor; it must not clobber a health
        // override applied earlier back to Healthy.
        let p = SelfHostedProvider::new()
            .with_health(HealthStatus::Degraded {
                detail: "probe failed".into(),
            })
            .with_host(de_host());
        assert!(matches!(
            p.capabilities().health,
            HealthStatus::Degraded { .. }
        ));
    }

    #[test]
    fn non_finite_or_negative_hourly_cost_is_ignored() {
        for bad in [f64::NAN, f64::INFINITY, -5.0] {
            let caps = SelfHostedProvider::new()
                .with_host(us_host().with_hourly_cost(bad))
                .capabilities();
            assert_eq!(
                caps.compute[0].pricing.on_demand_rate().price,
                Money::new(0.0, "EUR")
            );
        }
    }

    #[test]
    fn descriptor_reports_ssh_and_bare_metal() {
        let caps = SelfHostedProvider::new()
            .with_host(de_host())
            .capabilities();
        assert_eq!(caps.provisioning, ProvisioningMethod::Ssh);
        assert_eq!(caps.credentials, CredentialsModel::SshKey);
        assert!(caps.orchestration.bare_metal);
        assert!(!caps.orchestration.managed_kubernetes);
        assert!(
            caps.storage
                .iter()
                .any(|s| s.kind == StorageKind::LocalNvme)
        );
        assert!(
            !caps
                .storage
                .iter()
                .any(|s| s.kind == StorageKind::NetworkFs)
        );
    }

    #[test]
    fn cluster_host_adds_kubernetes_and_network_fs() {
        let caps = SelfHostedProvider::new()
            .with_host(de_host().as_cluster())
            .capabilities();
        assert!(caps.orchestration.managed_kubernetes);
        assert!(
            caps.storage
                .iter()
                .any(|s| s.kind == StorageKind::NetworkFs)
        );
    }

    #[tokio::test]
    async fn provision_status_teardown_runs_expected_ssh_commands_in_order() {
        let mock = lifecycle_mock();
        let p = mock_provider(mock.clone());
        let resolver = key_resolver();

        let handle = p.provision(&de_request(), &resolver).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.provider, AdapterId::new("self-hosted"));
        // The instance id comes from the provision command's stdout.
        assert_eq!(handle.id, "sh-inst-1");

        assert!(p.status(&handle, &resolver).await.unwrap().is_ready());
        p.teardown(&handle, &resolver).await.unwrap();

        // The lifecycle ran the host-agent commands in order against the host.
        let seen = mock.commands();
        assert_eq!(seen.len(), 3);
        assert!(seen[0].command.contains("provision"));
        // Interpolated values are POSIX single-quoted.
        assert!(seen[0].command.contains("--offering 'rack-de-01'"));
        assert!(seen[0].command.contains("--region 'dc-frankfurt'"));
        assert_eq!(seen[0].user, "amicu");
        assert_eq!(seen[0].host, "rack-de-01");
        assert!(seen[1].command.contains("status 'sh-inst-1'"));
        assert!(seen[2].command.contains("teardown 'sh-inst-1'"));
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let p = SelfHostedProvider::new().with_host(de_host());
        let err = p
            .provision(&ProvisionRequest::new("nope", "dc-frankfurt"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_provider_provision_yields_unknown_offering() {
        // No declared hosts: provisioning must fail cleanly, not panic.
        let p = SelfHostedProvider::new();
        assert_eq!(p.declared_host_count(), 0);
        let err = p
            .provision(
                &ProvisionRequest::new("rack-de-01", "dc-frankfurt"),
                &no_creds(),
            )
            .await
            .expect_err("empty provider has no offerings");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn provision_in_uncorresponding_region_is_rejected() {
        // rack-de-01 is declared in dc-frankfurt; requesting it in the US host's
        // region does not correspond to a declared host.
        let p = two_region_provider();
        let err = p
            .provision(
                &ProvisionRequest::new("rack-de-01", "dc-virginia"),
                &no_creds(),
            )
            .await
            .expect_err("region does not correspond to the declared host");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = SelfHostedProvider::new().with_host(de_host());
        let resolver = TestResolver::default().with("ssh_key", "");
        let req = ProvisionRequest::new("rack-de-01", "dc-frankfurt")
            .with_credentials(SecretRef::new("ssh_key"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn missing_credential_is_rejected() {
        // Self-hosted provisioning requires an SSH key.
        let p = mock_provider(lifecycle_mock());
        let err = p
            .provision(
                &ProvisionRequest::new("rack-de-01", "dc-frankfurt"),
                &no_creds(),
            )
            .await
            .expect_err("missing SSH key rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn non_utf8_credential_is_rejected() {
        let p = mock_provider(lifecycle_mock());
        let resolver = TestResolver::default().with_bytes("ssh_key", vec![0xff, 0xfe]);
        let err = p
            .provision(&de_request(), &resolver)
            .await
            .expect_err("non-UTF-8 key rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn auth_failure_maps_to_credentials_rejected() {
        let mock = Arc::new(
            MockSshTransport::default()
                .on_error("provision", SshTransportError::Auth("bad key".into())),
        );
        let err = mock_provider(mock)
            .provision(&de_request(), &key_resolver())
            .await
            .expect_err("auth failure surfaces");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn connection_failure_maps_to_retryable_backend() {
        let mock = Arc::new(
            MockSshTransport::default()
                .on_error("provision", SshTransportError::Connection("refused".into())),
        );
        let err = mock_provider(mock)
            .provision(&de_request(), &key_resolver())
            .await
            .expect_err("connection failure surfaces");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn timeout_maps_to_retryable_backend() {
        let mock =
            Arc::new(MockSshTransport::default().on_error("provision", SshTransportError::Timeout));
        let err = mock_provider(mock)
            .provision(&de_request(), &key_resolver())
            .await
            .expect_err("timeout surfaces");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn nonzero_exit_maps_to_backend() {
        let mock = Arc::new(MockSshTransport::default().on(
            "provision",
            CommandOutput::new(1, Vec::new(), b"prep failed".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision(&de_request(), &key_resolver())
            .await
            .expect_err("non-zero exit surfaces");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn status_not_found_exit_maps_to_unknown_instance() {
        let mock = Arc::new(
            MockSshTransport::default()
                .on(
                    "provision",
                    CommandOutput::new(0, b"sh-inst-1".to_vec(), Vec::new()),
                )
                .on(
                    "status",
                    CommandOutput::new(EXIT_NOT_FOUND, Vec::new(), Vec::new()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p.provision(&de_request(), &key_resolver()).await.unwrap();
        let err = p
            .status(&handle, &key_resolver())
            .await
            .expect_err("not-found exit surfaces");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-012");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_empty_instance_id_is_backend_error() {
        // A create command that succeeds but reports no instance id is a backend
        // fault, not a silent success.
        let mock = Arc::new(
            MockSshTransport::default()
                .on("provision", CommandOutput::new(0, Vec::new(), Vec::new())),
        );
        let err = mock_provider(mock)
            .provision(&de_request(), &key_resolver())
            .await
            .expect_err("empty instance id surfaces");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert_eq!(
            err.to_diagnostic().message,
            "provider backend error: self-hosted host agent did not report an instance id"
        );
    }

    #[tokio::test]
    async fn teardown_nonzero_exit_maps_to_backend() {
        let mock = Arc::new(
            MockSshTransport::default()
                .on(
                    "provision",
                    CommandOutput::new(0, b"sh-inst-1".to_vec(), Vec::new()),
                )
                .on(
                    "teardown",
                    CommandOutput::new(7, Vec::new(), b"cleanup failed".to_vec()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p.provision(&de_request(), &key_resolver()).await.unwrap();
        let err = p
            .teardown(&handle, &key_resolver())
            .await
            .expect_err("teardown non-zero exit surfaces");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[test]
    fn map_status_covers_every_token() {
        assert!(map_status(b"ready").is_ready());
        assert!(matches!(
            map_status(b"provisioning"),
            ProvisioningState::Provisioning { .. }
        ));
        assert!(matches!(
            map_status(b"pending"),
            ProvisioningState::Provisioning { .. }
        ));
        assert!(map_status(b"stopped").is_stopped());
        assert!(map_status(b"terminated").is_terminal());
        // Empty and unknown tokens both fall back to Provisioning.
        assert!(matches!(
            map_status(b""),
            ProvisioningState::Provisioning { .. }
        ));
        match map_status(b"weird") {
            ProvisioningState::Provisioning { detail } => assert!(detail.contains("weird")),
            other => panic!("unexpected state: {other:?}"),
        }
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
    }

    #[tokio::test]
    async fn custom_ssh_address_user_and_port_reach_the_target() {
        let host = de_host().with_ssh("10.0.0.5", "ops").with_ssh_port(2200);
        let mock = lifecycle_mock();
        let p = SelfHostedProvider::new()
            .with_host(host)
            .with_transport(mock.clone());
        p.provision(&de_request(), &key_resolver()).await.unwrap();
        let seen = mock.commands();
        assert_eq!(seen[0].host, "10.0.0.5");
        assert_eq!(seen[0].user, "ops");
        assert_eq!(seen[0].port, 2200);
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let secret_value = "leak-me-self-hosted-ssh-key";
        let mock = lifecycle_mock();
        let p = mock_provider(mock.clone());
        let resolver = TestResolver::default().with("ssh_key", secret_value);
        let req = de_request();

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("ssh_key"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("ssh_key"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"ssh_key\""));

        // The key authenticated the SSH connection but was never recorded on the
        // handle or in the commands the transport ran.
        let seen = mock.commands();
        assert!(seen.iter().all(|c| !c.command.contains(secret_value)));
    }

    #[tokio::test]
    async fn describe_reflects_declared_hosts() {
        let describe_json = serde_json::to_string(&two_region_provider().describe()).unwrap();
        assert!(describe_json.contains("\"dc-frankfurt\""));
        assert!(describe_json.contains("\"dc-virginia\""));
        assert!(describe_json.contains("\"rack-de-01\""));
        assert!(describe_json.contains("\"rack-us-01\""));
        // Residency is derived from the declared metadata.
        assert!(describe_json.contains("\"eu\""));
        assert!(describe_json.contains("\"us\""));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(mock_provider(lifecycle_mock())));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("self-hosted")).unwrap();
        let handle = found
            .provision(&de_request(), &key_resolver())
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p =
            SelfHostedProvider::new()
                .with_host(de_host())
                .with_health(HealthStatus::Degraded {
                    detail: "maintenance".into(),
                });
        assert!(matches!(
            p.health().await.unwrap(),
            HealthStatus::Degraded { .. }
        ));
        assert!(matches!(
            p.capabilities().health,
            HealthStatus::Degraded { .. }
        ));
    }
}
