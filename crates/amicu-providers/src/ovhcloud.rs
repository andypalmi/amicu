//! The OVHcloud [`Provider`] adapter, with EU data-residency filtering.
//!
//! OVHcloud is a French sovereign-cloud provider (SecNumCloud-certified) that
//! offers GPU instances through its Public Cloud API and bare-metal dedicated
//! servers across data centers worldwide. Amicu targets EU workloads, so this
//! adapter is **residency filtering by construction**: [`OvhCloudProvider::new`]
//! admits only the EU data centers from [`datacenters`], and
//! [`provision`](OvhCloudProvider::provision) rejects any region outside that
//! admitted set with [`ProviderError::UnknownRegion`] - an instance can never be
//! placed in a non-compliant region (including the UK) through this adapter.
//! [`OvhCloudProvider::for_residencies`] widens the admitted set explicitly when
//! a caller genuinely needs other jurisdictions.
//!
//! The [`ProviderCapabilities`] descriptor carries OVHcloud's real EU data
//! centers and a representative slice of its single-GPU Public Cloud catalog with
//! on-demand pricing (OVH bills hourly/monthly with no public spot market, so the
//! catalog is on-demand-only).
//!
//! Provisioning drives OVH's Public Cloud API (`https://eu.api.ovh.com/v1`,
//! `/cloud/project/{serviceName}/instance`) through the [`HttpTransport`] seam:
//! `POST .../instance` to create, `GET .../instance/{id}` for status, and
//! `DELETE .../instance/{id}` to tear down. The adapter never touches a concrete
//! HTTP client - production uses [`ReqwestTransport`], tests inject a mock - so
//! the provisioning path is exercised with no network, credentials, or spend.
//! HTTP status codes map onto typed [`ProviderError`]s (`401`/`403` → credentials
//! rejected, `404` → unknown instance/offering, `409`/`429` → capacity
//! unavailable, other non-`2xx` → retryable backend); a transport failure maps to
//! a retryable backend error. OVH's real auth is an application-key/secret plus a
//! per-request signature; this mocked phase models the resolved credential as a
//! single header (see `resolve_token`). Real-time capacity/pricing sync is out of
//! scope here; prices are illustrative catalog values in EUR/hr, not a
//! live quote.

use std::sync::Arc;

use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_core::resource::{Accelerator, AcceleratorKind, Money};
use amicu_core::secret::{SecretRef, SecretResolver};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::{
    Availability, AvailabilityState, BillingGranularity, ComputeOffering, CredentialsModel,
    LifecycleBehavior, ModelDownloadPerformance, NetworkCapabilities, OrchestrationSupport,
    Pricing, ProviderCapabilities, ProvisioningMethod, Rate, RateUnit, Region, Residency,
    StorageKind, StorageOption,
};
use crate::provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
    ResourceIdentity, TeardownOutcome,
};
use crate::rest_lifecycle::{self, Found, ServerApi};
use crate::transport::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport, TransportError};

/// The adapter identity OVHcloud registers under.
pub const PROVIDER_ID: &str = "ovhcloud";

/// The OVHcloud (EU) API base URL.
const BASE_URL: &str = "https://eu.api.ovh.com/v1";

/// OVH's per-request signature header. Live signing fills in the full
/// application-key/consumer-key/timestamp/signature set.
const AUTH_HEADER: &str = "X-Ovh-Signature";

/// Placeholder Public Cloud project id (`serviceName`) for the instance path.
/// Live provisioning resolves the caller's real project id.
const SERVICE_NAME: &str = "amicu-project";

/// An OVHcloud data center: its region code, residency jurisdiction, ISO country
/// code, and human-readable location. Both EU and non-EU entries are listed so
/// the EU filter has something to exclude.
struct DataCenter {
    /// OVHcloud region code, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
}

/// OVHcloud's data centers. EU entries are what [`OvhCloudProvider::new`] admits;
/// the UK and non-EU entries exist so the residency filter is a real exclusion,
/// not a hand-picked list. Residency uses a `fn` (not a `const`) because
/// [`Residency::Other`] owns a `String`.
fn datacenters() -> [DataCenter; 9] {
    [
        // EU.
        DataCenter {
            code: "GRA",
            residency: Residency::Eu,
            country: "FR",
            location: "Gravelines",
        },
        DataCenter {
            code: "SBG",
            residency: Residency::Eu,
            country: "FR",
            location: "Strasbourg",
        },
        DataCenter {
            code: "DE1",
            residency: Residency::Eu,
            country: "DE",
            location: "Frankfurt",
        },
        DataCenter {
            code: "WAW",
            residency: Residency::Eu,
            country: "PL",
            location: "Warsaw",
        },
        // UK - excluded by the EU filter (not EU/EEA).
        DataCenter {
            code: "UK1",
            residency: Residency::Uk,
            country: "GB",
            location: "London",
        },
        // Non-EU - excluded by the residency filter.
        DataCenter {
            code: "BHS",
            residency: Residency::Ca,
            country: "CA",
            location: "Beauharnois",
        },
        DataCenter {
            code: "US-EAST-VA",
            residency: Residency::Us,
            country: "US",
            location: "Virginia",
        },
        DataCenter {
            code: "SGP",
            residency: Residency::Apac,
            country: "SG",
            location: "Singapore",
        },
        DataCenter {
            code: "SYD",
            residency: Residency::Apac,
            country: "AU",
            location: "Sydney",
        },
    ]
}

/// One entry of OVHcloud's GPU catalog: an offering id, the accelerator model as
/// OVH names it, its VRAM in GiB, representative host resources, and an
/// illustrative on-demand rate in EUR per hour.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    vram_gib: u32,
    vcpus: u32,
    host_mem_gib: u32,
    on_demand: f64,
    availability: AvailabilityState,
}

/// A representative slice of OVHcloud's single-GPU Public Cloud catalog. Prices
/// are illustrative catalog values in EUR/hr, not a live quote (real-time pricing
/// is not yet supported). OVH offers no public spot market, so pricing is on-demand-only.
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "gpu.h100.1x",
        model: "h100-pcie-80gb",
        vram_gib: 80,
        vcpus: 30,
        host_mem_gib: 240,
        on_demand: 2.99,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "gpu.a100.1x",
        model: "a100-pcie-80gb",
        vram_gib: 80,
        vcpus: 15,
        host_mem_gib: 180,
        on_demand: 2.29,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.l40s.1x",
        model: "l40s-48gb",
        vram_gib: 48,
        vcpus: 15,
        host_mem_gib: 90,
        on_demand: 1.39,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.l4.1x",
        model: "l4-24gb",
        vram_gib: 24,
        vcpus: 8,
        host_mem_gib: 32,
        on_demand: 0.79,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.v100.1x",
        model: "v100-32gb",
        vram_gib: 32,
        vcpus: 13,
        host_mem_gib: 45,
        on_demand: 0.89,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.t4.1x",
        model: "t4-16gb",
        vram_gib: 16,
        vcpus: 8,
        host_mem_gib: 30,
        on_demand: 0.39,
        availability: AvailabilityState::Available,
    },
];

/// Request body for `POST .../instance`.
///
/// A minimal create body: the offering id stands in for OVH's `flavorId` and the
/// region for its `region`. Richer fields (image, SSH key, networks) arrive with
/// live provisioning.
#[derive(Serialize)]
struct CreateInstanceRequest<'a> {
    name: &'a str,
    #[serde(rename = "flavorId")]
    flavor_id: &'a str,
    region: &'a str,
}

/// The subset of an OVH project object the adapter consumes.
#[derive(Deserialize)]
struct ProjectResponse {
    /// The account-side project id, when reported.
    #[serde(default)]
    project_id: Option<String>,
}

/// The subset of an OVH instance object the adapter consumes.
#[derive(Deserialize)]
struct InstanceResponse {
    /// Provider-assigned instance id.
    id: String,
    /// The instance name (carries the identity).
    #[serde(default)]
    name: Option<String>,
    /// The flavor (the offering), when listed.
    #[serde(rename = "flavorId", default)]
    flavor_id: Option<String>,
    /// The region, when listed.
    #[serde(default)]
    region: Option<String>,
    /// Lifecycle status, e.g. `"ACTIVE"`.
    status: Option<String>,
}

/// Map an OVH instance status string onto a [`ProvisioningState`].
fn map_instance_state(status: Option<&str>) -> ProvisioningState {
    match status.unwrap_or_default() {
        "ACTIVE" => ProvisioningState::Ready {
            detail: "instance active".into(),
        },
        "BUILD" | "BUILDING" | "REBOOT" | "HARD_REBOOT" | "REBUILD" => {
            ProvisioningState::Provisioning {
                detail: "instance building".into(),
            }
        }
        "STOPPED" | "SHUTOFF" | "SUSPENDED" | "PAUSED" => ProvisioningState::Stopped {
            detail: "instance stopped".into(),
        },
        "DELETING" | "DELETED" | "SOFT_DELETED" => ProvisioningState::Terminated {
            detail: "instance deleting".into(),
        },
        "ERROR" => ProvisioningState::Failed {
            detail: "instance error".into(),
        },
        "" => ProvisioningState::Provisioning {
            detail: "instance status unknown".into(),
        },
        other => ProvisioningState::Provisioning {
            detail: format!("instance status {other}"),
        },
    }
}

/// The instance path under the placeholder project, with an optional `/{id}`
/// suffix.
fn instance_path(project: &str, suffix: &str) -> String {
    format!("/cloud/project/{project}/instance{suffix}")
}

/// The cloud project the record's instance lives in. Taken from the record,
/// never from adapter config, and validated before it enters a URL.
fn project_of(handle: &ProvisioningHandle) -> Result<&str, AdapterError> {
    let project = handle
        .resources
        .get("project")
        .map(String::as_str)
        .ok_or_else(|| rest_lifecycle::refuse("stop record has no OVHcloud project"))?;
    if project.is_empty()
        || project.len() > 64
        || !project
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(rest_lifecycle::refuse(
            "stop record has an invalid OVHcloud project",
        ));
    }
    Ok(project)
}

/// Fold a transport failure (the exchange never completed) into a retryable
/// backend error.
fn transport_error(err: &TransportError) -> AdapterError {
    AdapterError::from_kind(ProviderError::Backend {
        detail: format!("OVHcloud transport failure: {err}"),
    })
}

/// Map a non-`2xx` status from a per-instance call (`status`/`teardown`) onto a
/// typed error; `404` means the instance is unknown.
fn lifecycle_error(status: u16, id: &str) -> AdapterError {
    let kind = match status {
        401 | 403 => ProviderError::CredentialsRejected {
            detail: "OVHcloud rejected the credential".into(),
        },
        404 => ProviderError::UnknownInstance { id: id.into() },
        _ => ProviderError::Backend {
            detail: format!("OVHcloud API returned HTTP {status}"),
        },
    };
    AdapterError::from_kind(kind)
}

/// The OVHcloud provider adapter.
///
/// Holds the admitted (residency-filtered) region set, the derived capability
/// descriptor, and the [`HttpTransport`] used to reach OVH's REST API.
pub struct OvhCloudProvider {
    id: AdapterId,
    capabilities: ProviderCapabilities,
    health: HealthStatus,
    transport: Arc<dyn HttpTransport>,
    base_url: String,
}

impl OvhCloudProvider {
    /// An OVHcloud provider admitting only EU data centers.
    ///
    /// This is the residency-safe default: no non-EU region (including the UK)
    /// reaches the descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// An OVHcloud provider admitting the data centers whose residency is in
    /// `allowed`. Use this to deliberately widen beyond the EU default.
    #[must_use]
    pub fn for_residencies(allowed: &[Residency]) -> Self {
        let id = AdapterId::new(PROVIDER_ID);
        let regions: Vec<Region> = datacenters()
            .into_iter()
            .filter(|dc| allowed.contains(&dc.residency))
            .map(|dc| {
                Region::new(dc.code, dc.residency)
                    .with_country(dc.country)
                    .with_display_name(dc.location)
            })
            .collect();
        let capabilities = Self::capabilities_for(&id, regions);
        Self {
            id,
            capabilities,
            health: HealthStatus::Healthy,
            transport: Arc::new(ReqwestTransport::new()),
            base_url: BASE_URL.to_string(),
        }
    }

    /// Override the health status (e.g. for tests or a degraded probe result).
    #[must_use]
    pub fn with_health(mut self, health: HealthStatus) -> Self {
        self.capabilities.health = health.clone();
        self.health = health;
        self
    }

    /// Swap in a transport and base URL. Test-only: adapter tests inject a mock
    /// so the provisioning path runs with no network, credentials, or spend.
    #[cfg(test)]
    pub(crate) fn with_transport(
        mut self,
        transport: Arc<dyn HttpTransport>,
        base_url: impl Into<String>,
    ) -> Self {
        self.transport = transport;
        self.base_url = base_url.into();
        self
    }

    /// The OVHcloud data-center codes admitted for `residency`.
    #[must_use]
    pub fn datacenter_codes(residency: &Residency) -> Vec<&'static str> {
        datacenters()
            .into_iter()
            .filter(|dc| &dc.residency == residency)
            .map(|dc| dc.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. Every
    /// offering is sold in every admitted region (OVH's GPU catalog is
    /// data-center agnostic at this granularity).
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let region_codes: Vec<String> = regions.iter().map(|r| r.code.clone()).collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::ApiAndSsh,
            CredentialsModel::ApiKey,
            BillingGranularity::PerHour,
            Availability::new(AvailabilityState::Available),
            HealthStatus::Healthy,
        );
        caps.regions = regions;
        caps.compute = GPUS
            .iter()
            .map(|g| ComputeOffering {
                id: g.id.into(),
                accelerator: Some(Accelerator {
                    kind: AcceleratorKind::Gpu,
                    model: g.model.into(),
                    count: 1,
                    memory: format!("{}GiB", g.vram_gib).parse().expect("valid size"),
                    memory_bandwidth_gbps: None,
                }),
                vcpus: Some(g.vcpus),
                memory: Some(
                    format!("{}GiB", g.host_mem_gib)
                        .parse()
                        .expect("valid size"),
                ),
                regions: region_codes.clone(),
                pricing: Pricing::on_demand(Rate::new(
                    Money::new(g.on_demand, "EUR"),
                    RateUnit::PerHour,
                )),
                availability: Availability::new(g.availability),
            })
            .collect();
        caps.storage = vec![
            StorageOption::new(StorageKind::Block),
            StorageOption::new(StorageKind::Object),
            StorageOption::new(StorageKind::NetworkFs),
        ];
        caps.network = NetworkCapabilities {
            bandwidth_mbps: Some(10_000),
            private_network: true,
        };
        caps.orchestration = OrchestrationSupport {
            managed_kubernetes: true,
            bare_metal: true,
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(120),
            teardown_secs: Some(30),
            supports_hibernation: false,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(3_000),
            cached_registry: false,
        };
        caps
    }

    fn has_offering(&self, offering_id: &str) -> bool {
        self.capabilities
            .compute
            .iter()
            .any(|o| o.id == offering_id)
    }

    fn admits_region(&self, region: &str) -> bool {
        self.capabilities.regions.iter().any(|r| r.code == region)
    }

    /// Check `offering_id` is offered and `region` is admitted, before any
    /// backend call.
    fn validate(&self, offering_id: &str, region: &str) -> Result<(), AdapterError> {
        if !self.has_offering(offering_id) {
            return Err(AdapterError::from_kind(ProviderError::UnknownOffering {
                offering: offering_id.to_string(),
            }));
        }
        if !self.admits_region(region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: region.to_string(),
            }));
        }
        Ok(())
    }

    /// Build a request for `{base}{path}`, attaching `token` in the OVH auth
    /// header when present.
    fn build_request(&self, method: HttpMethod, path: &str, token: Option<&str>) -> HttpRequest {
        let mut req = HttpRequest::new(method, format!("{}{path}", self.base_url));
        if let Some(token) = token {
            // TODO: full OVH request signing with live credentials.
            req = req.with_header(AUTH_HEADER, token);
        }
        req
    }

    /// Resolve the credential to a transient signing token for a single REST
    /// call.
    ///
    /// The returned [`String`] populates the OVH auth header for the immediate
    /// call and is dropped when it returns; it is never stored on a
    /// [`ProvisioningHandle`], logged, or serialized. `None` credentials yield
    /// `None`; an empty or non-UTF-8 credential is rejected as
    /// [`ProviderError::CredentialsRejected`].
    async fn resolve_token(
        &self,
        resolver: &dyn SecretResolver,
        credentials: Option<&SecretRef>,
    ) -> Result<Option<String>, AdapterError> {
        let Some(secret_ref) = credentials else {
            return Ok(None);
        };
        let value = resolver.resolve(secret_ref).await?;
        if value.is_empty() {
            return Err(AdapterError::from_kind(
                ProviderError::CredentialsRejected {
                    detail: "empty OVHcloud API key".into(),
                },
            ));
        }
        let token = value.expose_secret_str().map(str::to_owned).map_err(|_| {
            AdapterError::from_kind(ProviderError::CredentialsRejected {
                detail: "OVHcloud API key is not valid UTF-8".into(),
            })
        })?;
        Ok(Some(token))
    }
}

impl Default for OvhCloudProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for OvhCloudProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for OvhCloudProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for OvhCloudProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn prepare(
        &self,
        request: &ProvisionRequest,
        identity: ResourceIdentity,
    ) -> Result<ProvisioningHandle, AdapterError> {
        self.validate(&request.offering_id, &request.region)?;
        let mut record = ProvisioningHandle::prepared(self.id.clone(), request, identity);
        // The project is part of the record: teardown must hit the same project
        // and must never treat an unknown-project 404 as "gone".
        record
            .resources
            .insert("project".into(), SERVICE_NAME.to_string());
        record.scope = Some(self.scope_of(&record)?);
        Ok(record)
    }

    async fn provision(
        &self,
        mut prepared: ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningHandle, AdapterError> {
        self.validate(&prepared.offering_id, &prepared.region)?;
        if prepared.identity.name.is_empty() {
            return Err(rest_lifecycle::refuse("record has no resource identity"));
        }
        let token = self
            .resolve_token(resolver, prepared.credentials.as_ref())
            .await?;
        rest_lifecycle::bind(&mut prepared, token.as_deref())?;
        if let Some(account) =
            rest_lifecycle::guard_scope(self, &prepared, token.as_deref()).await?
        {
            prepared.account_id = Some(account);
        }

        // Adopt an instance already created under this identity (retry after a
        // crash) instead of creating a duplicate.
        if let Some(found) = rest_lifecycle::adopt(self, &prepared, token.as_deref()).await? {
            let state = map_instance_state(found.state.as_deref());
            return Ok(prepared.completed("instance_id", found.id, state));
        }

        let create = CreateInstanceRequest {
            name: prepared.identity.name.as_str(),
            flavor_id: prepared.offering_id.as_str(),
            region: prepared.region.as_str(),
        };
        let body = serde_json::to_value(&create).map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "failed to encode OVHcloud create request".into(),
            })
        })?;
        let path = instance_path(project_of(&prepared)?, "");
        let http_request = self
            .build_request(HttpMethod::Post, &path, token.as_deref())
            .with_json(body);
        let response = self
            .transport
            .execute(http_request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            let kind = match response.status {
                401 | 403 => ProviderError::CredentialsRejected {
                    detail: "OVHcloud rejected the credential".into(),
                },
                404 => ProviderError::UnknownOffering {
                    offering: prepared.offering_id.clone(),
                },
                409 | 429 => ProviderError::CapacityUnavailable {
                    offering: prepared.offering_id.clone(),
                    region: prepared.region.clone(),
                },
                status => ProviderError::Backend {
                    detail: format!("OVHcloud API returned HTTP {status}"),
                },
            };
            return Err(AdapterError::from_kind(kind));
        }
        let instance: InstanceResponse = response.json().map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "malformed OVHcloud create response".into(),
            })
        })?;
        let state = map_instance_state(instance.status.as_deref());
        Ok(prepared.completed("instance_id", instance.id, state))
    }

    async fn status(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningState, AdapterError> {
        rest_lifecycle::check_record(handle)?;
        let token = self
            .resolve_token(resolver, handle.credentials.as_ref())
            .await?;
        let path = instance_path(project_of(handle)?, &format!("/{}", handle.id));
        let request = self.build_request(HttpMethod::Get, &path, token.as_deref());
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            return Err(lifecycle_error(response.status, &handle.id));
        }
        let instance: InstanceResponse = response.json().map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "malformed OVHcloud status response".into(),
            })
        })?;
        Ok(map_instance_state(instance.status.as_deref()))
    }

    async fn teardown(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<TeardownOutcome, AdapterError> {
        let token = self
            .resolve_token(resolver, handle.credentials.as_ref())
            .await?;
        rest_lifecycle::teardown(self, handle, token.as_deref()).await
    }
}

#[async_trait]
impl ServerApi for OvhCloudProvider {
    fn scope_of(&self, handle: &ProvisioningHandle) -> Result<String, AdapterError> {
        Ok(format!(
            "{PROVIDER_ID}|{}||{}",
            handle.region,
            project_of(handle)?
        ))
    }

    async fn verify_scope(
        &self,
        handle: &ProvisioningHandle,
        token: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        // The project itself must exist for this credential: an unknown-project
        // 404 means wrong scope, not "instance gone".
        let path = format!("/cloud/project/{}", project_of(handle)?);
        let request = self.build_request(HttpMethod::Get, &path, token);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            return Err(lifecycle_error(response.status, "project"));
        }
        let project: ProjectResponse = response.json().map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "malformed OVHcloud project response".into(),
            })
        })?;
        Ok(project.project_id)
    }

    async fn list_by_identity(
        &self,
        handle: &ProvisioningHandle,
        token: Option<&str>,
    ) -> Result<Vec<Found>, AdapterError> {
        let name = handle.identity.name.as_str();
        if name.is_empty() {
            return Ok(Vec::new());
        }
        // The instance endpoint returns the full set in one response.
        let path = instance_path(project_of(handle)?, &format!("?region={}", handle.region));
        let request = self.build_request(HttpMethod::Get, &path, token);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            // A collection-level 404 is a scope error, never "no instances".
            return Err(lifecycle_error(response.status, "instances"));
        }
        let list: Vec<InstanceResponse> = response.json().map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "malformed OVHcloud list response".into(),
            })
        })?;
        Ok(list
            .into_iter()
            .filter(|i| i.name.as_deref() == Some(name))
            .map(|i| Found {
                account: None,
                id: i.id,
                offering: i.flavor_id,
                region: i.region,
                state: i.status,
            })
            .collect())
    }

    async fn delete(
        &self,
        handle: &ProvisioningHandle,
        id: &str,
        token: Option<&str>,
    ) -> Result<(), AdapterError> {
        let path = instance_path(project_of(handle)?, &format!("/{id}"));
        let request = self.build_request(HttpMethod::Delete, &path, token);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|e| transport_error(&e))?;
        if response.is_success() || response.status == 404 {
            return Ok(());
        }
        Err(lifecycle_error(response.status, id))
    }

    async fn exists(
        &self,
        handle: &ProvisioningHandle,
        id: &str,
        token: Option<&str>,
    ) -> Result<bool, AdapterError> {
        let path = instance_path(project_of(handle)?, &format!("/{id}"));
        let request = self.build_request(HttpMethod::Get, &path, token);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|e| transport_error(&e))?;
        if response.status == 404 {
            return Ok(false);
        }
        if response.is_success() {
            return Ok(true);
        }
        Err(lifecycle_error(response.status, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProvisionCompat;
    use crate::transport::{HttpResponse, MockTransport};
    use amicu_core::adapter::Registry;
    use amicu_core::diagnostic::{Category as DiagCategory, DiagnosticCode};
    use amicu_core::secret::SecretValue;
    use std::collections::HashMap;

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

    /// A mock that already answers the project probe and an empty identity
    /// listing, as a project with no instances would.
    fn scoped_mock() -> MockTransport {
        MockTransport::default()
            .on(
                HttpMethod::Get,
                "/cloud/project/amicu-project",
                HttpResponse::new(200, br#"{"project_id":"proj-1"}"#.to_vec()),
            )
            .on_contains(
                HttpMethod::Get,
                "/instance?region=",
                HttpResponse::new(200, b"[]".to_vec()),
            )
    }

    /// An OVHcloud provider whose transport is `mock` and whose base URL points
    /// at a harmless test host (the mock matches on the URL suffix).
    fn mock_provider(mock: Arc<MockTransport>) -> OvhCloudProvider {
        OvhCloudProvider::new().with_transport(mock, "https://mock.test/v1")
    }

    /// A successful create response for instance `id`, `ACTIVE`.
    fn created(id: &str) -> HttpResponse {
        HttpResponse::new(
            201,
            format!(r#"{{"id":"{id}","status":"ACTIVE"}}"#).into_bytes(),
        )
    }

    #[test]
    fn new_admits_only_eu_regions() {
        let caps = OvhCloudProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("ovhcloud"));
        assert_ne!(caps.regions, Vec::new());
        // Every admitted region is EU/EEA...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU data centers are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"GRA"));
        assert!(codes.contains(&"DE1"));
        // No UK or non-EU OVH data center leaks into the descriptor.
        assert!(!codes.contains(&"UK1"));
        assert!(!codes.contains(&"BHS"));
        assert!(!codes.contains(&"US-EAST-VA"));
        assert!(!codes.contains(&"SGP"));
    }

    #[test]
    fn filter_excludes_uk_and_non_eu_datacenters() {
        // The underlying catalog genuinely contains UK and non-EU data centers,
        // so the EU default is an exclusion, not a hand-picked list.
        assert!(OvhCloudProvider::datacenter_codes(&Residency::Uk).contains(&"UK1"));
        assert!(OvhCloudProvider::datacenter_codes(&Residency::Us).contains(&"US-EAST-VA"));
        let widened =
            OvhCloudProvider::for_residencies(&[Residency::Eu, Residency::Uk, Residency::Us]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        assert!(codes.iter().any(|c| c == "GRA"));
        assert!(codes.iter().any(|c| c == "UK1"));
        assert!(codes.iter().any(|c| c == "US-EAST-VA"));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = OvhCloudProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_on_demand_only_pricing() {
        let caps = OvhCloudProvider::new().capabilities();
        let h100 = caps
            .compute
            .iter()
            .find(|o| o.id == "gpu.h100.1x")
            .expect("h100 offering present");
        // OVH has no public spot market: pricing is on-demand-only.
        assert!(!h100.pricing.has_spot());
        assert_eq!(h100.pricing.on_demand_rate().price.currency, "EUR");
        // H100 capacity is constrained.
        assert!(!h100.availability.is_available());
        // Every offering is sold in every admitted (EU) region.
        assert_eq!(h100.regions.len(), caps.regions.len());
    }

    #[test]
    fn descriptor_models_ovh_operational_facts() {
        let caps = OvhCloudProvider::new().capabilities();
        assert_eq!(caps.provisioning, ProvisioningMethod::ApiAndSsh);
        assert_eq!(caps.credentials, CredentialsModel::ApiKey);
        assert_eq!(caps.billing_granularity, BillingGranularity::PerHour);
        // vRack private network.
        assert!(caps.network.private_network);
        assert!(caps.orchestration.managed_kubernetes);
        assert!(caps.orchestration.bare_metal);
        assert!(!caps.lifecycle.supports_hibernation);
        assert!(!caps.model_download.cached_registry);
        let kinds: Vec<StorageKind> = caps.storage.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&StorageKind::Block));
        assert!(kinds.contains(&StorageKind::Object));
        assert!(kinds.contains(&StorageKind::NetworkFs));
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let mock = Arc::new(
            scoped_mock()
                .on(HttpMethod::Post, "/instance", created("ovh-1"))
                .on(
                    HttpMethod::Get,
                    "/instance/ovh-1",
                    HttpResponse::new(200, br#"{"id":"ovh-1","status":"ACTIVE"}"#.to_vec()),
                )
                .on(
                    HttpMethod::Delete,
                    "/instance/ovh-1",
                    HttpResponse::new(204, Vec::new()),
                ),
        );
        let p = mock_provider(mock.clone());
        let req = ProvisionRequest::new("gpu.a100.1x", "GRA");

        let handle = p.provision_request(&req, &no_creds()).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.id, "ovh-1");
        assert_eq!(handle.provider, AdapterId::new("ovhcloud"));

        assert!(p.status(&handle, &no_creds()).await.unwrap().is_ready());
        // The canned GET keeps reporting the resource after the delete, so absence is
        // unconfirmed (the conformance harness covers the confirmed path).
        assert!(
            !p.teardown(&handle, &no_creds())
                .await
                .unwrap()
                .is_confirmed_gone()
        );

        // The lifecycle hit the expected REST endpoints in order.
        let seen = mock.requests();
        // The identity lookup (adoption check) comes first, then the create.
        assert_eq!(seen[0].method, HttpMethod::Get);
        assert!(
            seen.iter()
                .any(|r| r.method == HttpMethod::Post && r.url.ends_with("/instance"))
        );
        assert!(seen.iter().any(|r| r.method == HttpMethod::Delete));
    }

    #[tokio::test]
    async fn provision_in_uk_region_is_rejected() {
        // UK1 (London) is a real OVH data center, but the EU adapter does not
        // admit it: the UK is outside the EU/EEA residency boundary. Rejected
        // before any backend call.
        let p = OvhCloudProvider::new();
        let err = p
            .provision_request(&ProvisionRequest::new("gpu.a100.1x", "UK1"), &no_creds())
            .await
            .expect_err("UK region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_in_non_eu_region_is_rejected() {
        // BHS (Canada) is a real OVH data center outside the admitted EU set.
        let p = OvhCloudProvider::new();
        let err = p
            .provision_request(&ProvisionRequest::new("gpu.a100.1x", "BHS"), &no_creds())
            .await
            .expect_err("non-EU region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let err = OvhCloudProvider::new()
            .provision_request(&ProvisionRequest::new("nope", "GRA"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = OvhCloudProvider::new();
        let resolver = TestResolver::default().with("ovh_api_key", "");
        let req = ProvisionRequest::new("gpu.a100.1x", "GRA")
            .with_credentials(SecretRef::new("ovh_api_key"));
        let err = p
            .provision_request(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn non_utf8_credential_is_rejected() {
        let p = OvhCloudProvider::new();
        let resolver = TestResolver::default().with_bytes("ovh_api_key", vec![0xff, 0xfe]);
        let req = ProvisionRequest::new("gpu.a100.1x", "GRA")
            .with_credentials(SecretRef::new("ovh_api_key"));
        let err = p
            .provision_request(&req, &resolver)
            .await
            .expect_err("non-UTF-8 credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_401_to_credentials_rejected() {
        let mock = Arc::new(scoped_mock().on(
            HttpMethod::Post,
            "/instance",
            HttpResponse::new(403, b"forbidden".to_vec()),
        ));
        let p = mock_provider(mock);
        let req = ProvisionRequest::new("gpu.a100.1x", "GRA")
            .with_credentials(SecretRef::new("ovh_api_key"));
        let resolver = TestResolver::default().with("ovh_api_key", "bad");
        let err = p
            .provision_request(&req, &resolver)
            .await
            .expect_err("403 must reject credentials");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_404_to_unknown_offering() {
        let mock = Arc::new(scoped_mock().on(
            HttpMethod::Post,
            "/instance",
            HttpResponse::new(404, b"no such flavor".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision_request(&ProvisionRequest::new("gpu.a100.1x", "GRA"), &no_creds())
            .await
            .expect_err("create 404 maps to unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_429_to_capacity_unavailable() {
        let mock = Arc::new(scoped_mock().on(
            HttpMethod::Post,
            "/instance",
            HttpResponse::new(429, b"quota".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision_request(&ProvisionRequest::new("gpu.a100.1x", "GRA"), &no_creds())
            .await
            .expect_err("429 must map to capacity unavailable");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-001");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_500_to_retryable_backend() {
        let mock = Arc::new(scoped_mock().on(
            HttpMethod::Post,
            "/instance",
            HttpResponse::new(500, b"boom".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision_request(&ProvisionRequest::new("gpu.a100.1x", "GRA"), &no_creds())
            .await
            .expect_err("5xx maps to backend");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn provision_malformed_success_body_is_backend_error() {
        let mock = Arc::new(scoped_mock().on(
            HttpMethod::Post,
            "/instance",
            HttpResponse::new(201, b"not json".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision_request(&ProvisionRequest::new("gpu.a100.1x", "GRA"), &no_creds())
            .await
            .expect_err("malformed 2xx body maps to backend");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
    }

    #[tokio::test]
    async fn status_maps_404_to_unknown_instance() {
        let mock = Arc::new(
            scoped_mock()
                .on(HttpMethod::Post, "/instance", created("ovh-2"))
                .on(
                    HttpMethod::Get,
                    "/instance/ovh-2",
                    HttpResponse::new(404, b"not found".to_vec()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p
            .provision_request(&ProvisionRequest::new("gpu.a100.1x", "GRA"), &no_creds())
            .await
            .unwrap();
        let err = p
            .status(&handle, &no_creds())
            .await
            .expect_err("404 must map to unknown instance");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-012");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn teardown_404_is_confirmed_gone() {
        let mock = Arc::new(
            scoped_mock()
                .on(HttpMethod::Post, "/instance", created("ovh-2"))
                .on(
                    HttpMethod::Delete,
                    "/instance/ovh-2",
                    HttpResponse::new(404, b"gone".to_vec()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p
            .provision_request(&ProvisionRequest::new("gpu.a100.1x", "GRA"), &no_creds())
            .await
            .unwrap();
        // Idempotent: a 404 on delete means already gone.
        let outcome = p.teardown(&handle, &no_creds()).await.unwrap();
        assert_eq!(outcome, TeardownOutcome::ConfirmedGone);
    }

    #[tokio::test]
    async fn transport_failure_maps_to_retryable_backend() {
        let mock = Arc::new(scoped_mock().on_error(
            HttpMethod::Post,
            "/instance",
            TransportError::Timeout,
        ));
        let err = mock_provider(mock)
            .provision_request(&ProvisionRequest::new("gpu.a100.1x", "GRA"), &no_creds())
            .await
            .expect_err("transport failure surfaces as an error");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let secret_value = "leak-me-ovh-api-key";
        let resolver = TestResolver::default().with("ovh_api_key", secret_value);
        let mock = Arc::new(scoped_mock().on(HttpMethod::Post, "/instance", created("ovh-3")));
        let p = mock_provider(mock.clone());
        let req = ProvisionRequest::new("gpu.a100.1x", "GRA")
            .with_credentials(SecretRef::new("ovh_api_key"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("ovh_api_key"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision_request(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("ovh_api_key"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"api_key\""));

        // The token rode in the OVH signature header (never as a bearer) and never
        // landed in any serialized artifact.
        let seen = mock.requests();
        assert!(seen[0].bearer.is_none());
        assert!(
            seen[0]
                .headers
                .iter()
                .any(|(k, v)| k == "X-Ovh-Signature" && v == secret_value)
        );
    }

    #[tokio::test]
    async fn describe_never_serializes_uk_or_non_eu_regions() {
        let describe_json = serde_json::to_string(&OvhCloudProvider::new().describe()).unwrap();
        assert!(describe_json.contains("GRA"));
        assert!(!describe_json.contains("UK1"));
        assert!(!describe_json.contains("BHS"));
        assert!(!describe_json.contains("US-EAST-VA"));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mock = Arc::new(scoped_mock().on(HttpMethod::Post, "/instance", created("ovh-4")));
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(mock_provider(mock)));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("ovhcloud")).unwrap();
        let handle = found
            .provision_request(&ProvisionRequest::new("gpu.t4.1x", "DE1"), &no_creds())
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = OvhCloudProvider::new().with_health(HealthStatus::Degraded {
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
