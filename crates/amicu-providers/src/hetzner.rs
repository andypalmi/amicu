//! The Hetzner [`Provider`] adapter, with EU data-residency filtering.
//!
//! Hetzner is a German provider with a strong EU-sovereignty posture. Its GPU
//! hardware is **dedicated / bare-metal** (the GEX line), not a GPU cloud:
//! servers are provisioned through the hcloud API and driven over SSH, and
//! dedicated capacity bills **monthly** in EUR (no spot). Amicu targets EU
//! workloads, so this adapter is **residency filtering by construction**:
//! [`HetznerProvider::new`] admits only the EU data centers from
//! [`datacenters`], and [`provision`](HetznerProvider::provision) rejects any
//! region outside that admitted set with [`ProviderError::UnknownRegion`] - a
//! server can never be placed in a non-compliant region through this adapter.
//! [`HetznerProvider::for_residencies`] widens the admitted set explicitly when
//! a caller genuinely needs other jurisdictions.
//!
//! Hetzner's GPU hardware lives only in its German and Finnish data centers, so
//! the GPU offerings are sold in the EU data centers alone. Provisioning drives
//! the hcloud REST API (`https://api.hetzner.cloud/v1`) through the
//! [`HttpTransport`] seam: `POST /servers` to create, `GET /servers/{id}` for
//! status, and `DELETE /servers/{id}` to tear down, authenticated with
//! `Authorization: Bearer`. The adapter never touches a concrete HTTP client -
//! production uses [`ReqwestTransport`], tests inject a mock - so the
//! provisioning path is exercised with no network, credentials, or spend. HTTP
//! status codes map onto typed [`ProviderError`]s (`401`/`403` → credentials
//! rejected, `404` → unknown instance/offering, `409`/`429` → capacity
//! unavailable, other non-`2xx` → retryable backend); a transport failure maps
//! to a retryable backend error.
//!
//! The [`ProviderCapabilities`] descriptor carries Hetzner's real data centers
//! and a representative slice of the GEX dedicated-server catalog with monthly
//! on-demand pricing. Real-time capacity/pricing sync is out of scope here;
//! prices are illustrative catalog values, not a live quote.

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

/// The adapter identity Hetzner registers under.
pub const PROVIDER_ID: &str = "hetzner";

/// The hcloud REST API base URL.
const BASE_URL: &str = "https://api.hetzner.cloud/v1";

/// Placeholder OS image for the create body. Live provisioning selects the real
/// image.
const DEFAULT_IMAGE: &str = "ubuntu-22.04";

/// A Hetzner data center: its location code, residency jurisdiction, ISO
/// country code, human-readable location, and whether it hosts GPU (GEX)
/// hardware. Both EU and non-EU entries are listed so the EU filter has
/// something to exclude.
struct DataCenter {
    /// Hetzner location code, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
    /// Whether GPU (GEX dedicated) hardware is available here. Hetzner's GPU
    /// fleet lives only in its German and Finnish sites.
    gpu: bool,
}

/// Hetzner's data centers. The EU entries are what [`HetznerProvider::new`]
/// admits; the non-EU entries exist so the residency filter is a real
/// exclusion, not a hand-picked list. Residency uses a `fn` (not a `const`)
/// because [`Residency::Other`] owns a `String`.
fn datacenters() -> [DataCenter; 6] {
    [
        // EU - GPU hardware lives here (DE / FI).
        DataCenter {
            code: "nbg1",
            residency: Residency::Eu,
            country: "DE",
            location: "Nuremberg",
            gpu: true,
        },
        DataCenter {
            code: "fsn1",
            residency: Residency::Eu,
            country: "DE",
            location: "Falkenstein",
            gpu: true,
        },
        DataCenter {
            code: "hel1",
            residency: Residency::Eu,
            country: "FI",
            location: "Helsinki",
            gpu: true,
        },
        // Non-EU - excluded by the residency filter, and no GPU hardware.
        DataCenter {
            code: "ash",
            residency: Residency::Us,
            country: "US",
            location: "Ashburn, Virginia",
            gpu: false,
        },
        DataCenter {
            code: "hil",
            residency: Residency::Us,
            country: "US",
            location: "Hillsboro, Oregon",
            gpu: false,
        },
        DataCenter {
            code: "sin",
            residency: Residency::Apac,
            country: "SG",
            location: "Singapore",
            gpu: false,
        },
    ]
}

/// One entry of Hetzner's GEX dedicated-server catalog: an offering id, the GPU
/// model as Hetzner names it, its VRAM in GiB, the (generous) host resources of
/// the dedicated box, and an illustrative monthly on-demand rate in EUR.
/// Dedicated servers bill monthly with no spot market.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    vram_gib: u32,
    vcpus: u32,
    host_mem_gib: u32,
    /// Illustrative on-demand rate in EUR **per month**.
    on_demand_monthly: f64,
    availability: AvailabilityState,
}

/// A representative slice of Hetzner's GEX GPU dedicated-server line. These are
/// large bare-metal boxes, not cloud slices. Prices are illustrative catalog
/// values in EUR/month, not a live quote (real-time pricing is not yet supported).
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "gpu.rtx-4000-ada.1x",
        model: "rtx-4000-ada",
        vram_gib: 20,
        vcpus: 24,
        host_mem_gib: 64,
        on_demand_monthly: 184.0,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.rtx-6000-ada.1x",
        model: "rtx-6000-ada",
        vram_gib: 48,
        vcpus: 40,
        host_mem_gib: 128,
        on_demand_monthly: 819.0,
        availability: AvailabilityState::Available,
    },
];

/// Request body for `POST {base}/servers`.
///
/// A minimal create body: the offering id stands in for Hetzner's `server_type`
/// and the region for its `location`. Richer fields (SSH keys, volumes, user
/// data) arrive with live provisioning.
#[derive(Serialize)]
struct CreateServerRequest<'a> {
    name: &'a str,
    server_type: &'a str,
    location: &'a str,
    image: &'a str,
}

/// A `{"name": ...}` sub-object.
#[derive(Deserialize)]
struct NamedObject {
    name: Option<String>,
}

/// An hcloud datacenter sub-object (`datacenter.location.name`).
#[derive(Deserialize)]
struct DatacenterObject {
    location: Option<NamedObject>,
}

/// Page size for identity listings (the hcloud maximum).
const PER_PAGE: usize = 50;

/// The hcloud `{"servers":[...]}` list envelope.
#[derive(Deserialize)]
struct ServerList {
    servers: Vec<ServerObject>,
}

/// The hcloud `{"server":{...}}` envelope returned by create and status.
#[derive(Deserialize)]
struct ServerEnvelope {
    server: ServerObject,
}

/// The subset of an hcloud server object the adapter consumes.
#[derive(Deserialize)]
struct ServerObject {
    /// Provider-assigned server id (an integer in the hcloud API).
    id: i64,
    /// The server name (carries the identity).
    #[serde(default)]
    name: Option<String>,
    /// The server type (the offering), when listed.
    #[serde(default)]
    server_type: Option<NamedObject>,
    /// The data center, when listed.
    #[serde(default)]
    datacenter: Option<DatacenterObject>,
    /// Lifecycle status, e.g. `"running"`.
    status: Option<String>,
}

/// Map an hcloud server status string onto a [`ProvisioningState`].
fn map_server_state(status: Option<&str>) -> ProvisioningState {
    match status.unwrap_or_default() {
        "running" => ProvisioningState::Ready {
            detail: "server running".into(),
        },
        "initializing" | "starting" | "rebuilding" | "migrating" => {
            ProvisioningState::Provisioning {
                detail: "server starting".into(),
            }
        }
        "stopping" => ProvisioningState::Stopping {
            detail: "server stopping".into(),
        },
        "off" => ProvisioningState::Stopped {
            detail: "server off".into(),
        },
        "deleting" => ProvisioningState::Terminated {
            detail: "server deleting".into(),
        },
        "" => ProvisioningState::Provisioning {
            detail: "server status unknown".into(),
        },
        other => ProvisioningState::Provisioning {
            detail: format!("server status {other}"),
        },
    }
}

/// Fold a transport failure (the exchange never completed) into a retryable
/// backend error.
fn transport_error(err: &TransportError) -> AdapterError {
    AdapterError::from_kind(ProviderError::Backend {
        detail: format!("Hetzner transport failure: {err}"),
    })
}

/// Map a non-`2xx` status from a per-instance call (`status`/`teardown`) onto a
/// typed error; `404` means the server is unknown.
fn lifecycle_error(status: u16, id: &str) -> AdapterError {
    let kind = match status {
        401 | 403 => ProviderError::CredentialsRejected {
            detail: "Hetzner rejected the API token".into(),
        },
        404 => ProviderError::UnknownInstance { id: id.into() },
        _ => ProviderError::Backend {
            detail: format!("Hetzner API returned HTTP {status}"),
        },
    };
    AdapterError::from_kind(kind)
}

/// The Hetzner provider adapter.
///
/// Holds the admitted (residency-filtered) region set, the derived capability
/// descriptor, and the [`HttpTransport`] used to reach the hcloud REST API.
pub struct HetznerProvider {
    id: AdapterId,
    capabilities: ProviderCapabilities,
    health: HealthStatus,
    transport: Arc<dyn HttpTransport>,
    base_url: String,
}

impl HetznerProvider {
    /// A Hetzner provider admitting only EU data centers.
    ///
    /// This is the residency-safe default: no non-EU region reaches the
    /// descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// A Hetzner provider admitting the data centers whose residency is in
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

    /// The Hetzner data-center codes admitted for `residency`.
    #[must_use]
    pub fn datacenter_codes(residency: &Residency) -> Vec<&'static str> {
        datacenters()
            .into_iter()
            .filter(|dc| &dc.residency == residency)
            .map(|dc| dc.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. The GEX
    /// offerings are sold only in the GPU-capable (German / Finnish) data
    /// centers among the admitted set.
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let gpu_codes: Vec<String> = datacenters()
            .into_iter()
            .filter(|dc| dc.gpu)
            .map(|dc| dc.code.to_string())
            .collect();
        let gpu_region_codes: Vec<String> = regions
            .iter()
            .map(|r| r.code.clone())
            .filter(|code| gpu_codes.contains(code))
            .collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::ApiAndSsh,
            CredentialsModel::ApiToken,
            BillingGranularity::PerMonth,
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
                regions: gpu_region_codes.clone(),
                // Dedicated servers bill monthly on-demand; no spot market.
                pricing: Pricing::on_demand(Rate::new(
                    Money::new(g.on_demand_monthly, "EUR"),
                    RateUnit::PerMonth,
                )),
                availability: Availability::new(g.availability),
            })
            .collect();
        caps.storage = vec![
            StorageOption::new(StorageKind::LocalNvme),
            StorageOption::new(StorageKind::Block),
            StorageOption::new(StorageKind::Object),
        ];
        caps.network = NetworkCapabilities {
            bandwidth_mbps: Some(1_000),
            private_network: true,
        };
        caps.orchestration = OrchestrationSupport {
            managed_kubernetes: false,
            bare_metal: true,
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(1_200),
            teardown_secs: Some(120),
            supports_hibernation: false,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(1_000),
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

    /// Whether `offering_id` is sold in `region`. An offering's region set is
    /// the intersection of admitted (EU by default) residency and the
    /// GPU-capable data centers, so this rejects both non-admitted regions and
    /// admitted regions that do not host the offering's GPU line.
    fn offering_serves_region(&self, offering_id: &str, region: &str) -> bool {
        self.capabilities
            .compute
            .iter()
            .find(|o| o.id == offering_id)
            .is_some_and(|o| o.regions.iter().any(|r| r == region))
    }

    /// Check `offering_id` is offered and sold in `region`; the residency and
    /// availability filter, run before any backend call.
    fn validate(&self, offering_id: &str, region: &str) -> Result<(), AdapterError> {
        if !self.has_offering(offering_id) {
            return Err(AdapterError::from_kind(ProviderError::UnknownOffering {
                offering: offering_id.to_string(),
            }));
        }
        if !self.offering_serves_region(offering_id, region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: region.to_string(),
            }));
        }
        Ok(())
    }

    /// Build a request for `{base}{path}`, attaching `token` as a bearer when
    /// present.
    fn build_request(&self, method: HttpMethod, path: &str, token: Option<&str>) -> HttpRequest {
        let mut req = HttpRequest::new(method, format!("{}{path}", self.base_url));
        if let Some(token) = token {
            req = req.with_bearer(token);
        }
        req
    }

    /// Resolve the credential to a transient bearer token for a single REST call.
    ///
    /// The returned [`String`] populates
    /// [`HttpRequest::bearer`](crate::transport::HttpRequest) for the immediate
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
                    detail: "empty Hetzner API token".into(),
                },
            ));
        }
        let token = value.expose_secret_str().map(str::to_owned).map_err(|_| {
            AdapterError::from_kind(ProviderError::CredentialsRejected {
                detail: "Hetzner API token is not valid UTF-8".into(),
            })
        })?;
        Ok(Some(token))
    }
}

impl Default for HetznerProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for HetznerProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for HetznerProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for HetznerProvider {
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
        rest_lifecycle::guard_scope(self, &prepared, token.as_deref()).await?;

        // Adopt a server already created under this identity (retry after a
        // crash) instead of creating a duplicate.
        if let Some(found) = rest_lifecycle::adopt(self, &prepared, token.as_deref()).await? {
            let state = map_server_state(found.state.as_deref());
            return Ok(prepared.completed("server_id", found.id, state));
        }

        let create = CreateServerRequest {
            name: prepared.identity.name.as_str(),
            server_type: prepared.offering_id.as_str(),
            location: prepared.region.as_str(),
            image: DEFAULT_IMAGE,
        };
        let body = serde_json::to_value(&create).map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "failed to encode Hetzner create request".into(),
            })
        })?;
        let http_request = self
            .build_request(HttpMethod::Post, "/servers", token.as_deref())
            .with_json(body);
        let response = self
            .transport
            .execute(http_request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            let kind = match response.status {
                401 | 403 => ProviderError::CredentialsRejected {
                    detail: "Hetzner rejected the API token".into(),
                },
                404 => ProviderError::UnknownOffering {
                    offering: prepared.offering_id.clone(),
                },
                409 | 429 => ProviderError::CapacityUnavailable {
                    offering: prepared.offering_id.clone(),
                    region: prepared.region.clone(),
                },
                status => ProviderError::Backend {
                    detail: format!("Hetzner API returned HTTP {status}"),
                },
            };
            return Err(AdapterError::from_kind(kind));
        }
        let envelope: ServerEnvelope = response.json().map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "malformed Hetzner create response".into(),
            })
        })?;
        let state = map_server_state(envelope.server.status.as_deref());
        Ok(prepared.completed("server_id", envelope.server.id.to_string(), state))
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
        let path = format!("/servers/{}", handle.id);
        let request = self.build_request(HttpMethod::Get, &path, token.as_deref());
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            return Err(lifecycle_error(response.status, &handle.id));
        }
        let envelope: ServerEnvelope = response.json().map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "malformed Hetzner status response".into(),
            })
        })?;
        Ok(map_server_state(envelope.server.status.as_deref()))
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
impl ServerApi for HetznerProvider {
    fn scope_of(&self, handle: &ProvisioningHandle) -> Result<String, AdapterError> {
        // The hcloud token is itself bound to one project, so the region is the
        // only scope part the record can carry.
        Ok(format!("{PROVIDER_ID}|{}||", handle.region))
    }

    async fn verify_scope(
        &self,
        _handle: &ProvisioningHandle,
        token: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let request = self.build_request(HttpMethod::Get, "/servers?per_page=1", token);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            return Err(lifecycle_error(response.status, "servers"));
        }
        Ok(None)
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
        let mut out: Vec<Found> = Vec::new();
        for page in 1..=rest_lifecycle::MAX_PAGES {
            let path = format!("/servers?name={name}&page={page}&per_page={PER_PAGE}");
            let request = self.build_request(HttpMethod::Get, &path, token);
            let response = self
                .transport
                .execute(request)
                .await
                .map_err(|e| transport_error(&e))?;
            if !response.is_success() {
                // A collection-level 404 is a scope error, never "no servers".
                return Err(lifecycle_error(response.status, "servers"));
            }
            let list: ServerList = response.json().map_err(|_| {
                AdapterError::from_kind(ProviderError::Backend {
                    detail: "malformed Hetzner list response".into(),
                })
            })?;
            let returned = list.servers.len();
            let mut fresh = 0;
            for s in list.servers {
                let id = s.id.to_string();
                if out.iter().any(|f| f.id == id) {
                    continue;
                }
                fresh += 1;
                if s.name.as_deref() == Some(name) {
                    out.push(Found {
                        account: None,
                        id,
                        offering: s.server_type.and_then(|t| t.name),
                        region: s.datacenter.and_then(|d| d.location).and_then(|l| l.name),
                        state: s.status,
                    });
                }
            }
            if returned < PER_PAGE || fresh == 0 {
                break;
            }
        }
        Ok(out)
    }

    async fn delete(
        &self,
        _handle: &ProvisioningHandle,
        id: &str,
        token: Option<&str>,
    ) -> Result<(), AdapterError> {
        let path = format!("/servers/{id}");
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
        _handle: &ProvisioningHandle,
        id: &str,
        token: Option<&str>,
    ) -> Result<bool, AdapterError> {
        let path = format!("/servers/{id}");
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

    /// A mock that already answers the scope probe and an empty identity listing,
    /// as an account with no servers would.
    fn scoped_mock() -> MockTransport {
        MockTransport::default()
            .on(
                HttpMethod::Get,
                "?per_page=1",
                HttpResponse::new(200, br#"{"servers":[]}"#.to_vec()),
            )
            .on_contains(
                HttpMethod::Get,
                "/servers?name=",
                HttpResponse::new(200, br#"{"servers":[]}"#.to_vec()),
            )
    }

    /// A Hetzner provider whose transport is `mock` and whose base URL points at
    /// a harmless test host (the mock matches on the URL suffix).
    fn mock_provider(mock: Arc<MockTransport>) -> HetznerProvider {
        HetznerProvider::new().with_transport(mock, "https://mock.test/v1")
    }

    /// A successful `POST /servers` create response for server `id`, `running`.
    fn created(id: i64) -> HttpResponse {
        HttpResponse::new(
            201,
            format!(r#"{{"server":{{"id":{id},"status":"running"}}}}"#).into_bytes(),
        )
    }

    #[test]
    fn new_admits_only_eu_regions() {
        let caps = HetznerProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("hetzner"));
        assert_ne!(caps.regions, Vec::new());
        // Every admitted region is EU...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU data centers are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"nbg1"));
        assert!(codes.contains(&"fsn1"));
        assert!(codes.contains(&"hel1"));
        // No non-EU Hetzner data center leaks into the descriptor.
        assert!(!codes.contains(&"ash"));
        assert!(!codes.contains(&"hil"));
        assert!(!codes.contains(&"sin"));
    }

    #[test]
    fn filter_excludes_non_eu_datacenters() {
        // The underlying catalog genuinely contains US and APAC data centers, so
        // the EU default is an exclusion, not a hand-picked list.
        assert!(HetznerProvider::datacenter_codes(&Residency::Us).contains(&"ash"));
        assert!(HetznerProvider::datacenter_codes(&Residency::Apac).contains(&"sin"));
        let widened = HetznerProvider::for_residencies(&[Residency::Eu, Residency::Us]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        assert!(codes.iter().any(|c| c == "nbg1"));
        assert!(codes.iter().any(|c| c == "ash"));
        assert!(!codes.iter().any(|c| c == "sin"));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = HetznerProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_monthly_on_demand_pricing() {
        let caps = HetznerProvider::new().capabilities();
        assert_eq!(caps.billing_granularity, BillingGranularity::PerMonth);
        let gex = caps
            .compute
            .iter()
            .find(|o| o.id == "gpu.rtx-6000-ada.1x")
            .expect("rtx-6000-ada offering present");
        // Dedicated servers bill monthly in EUR with no spot market.
        assert!(!gex.pricing.has_spot());
        let rate = gex.pricing.on_demand_rate();
        assert_eq!(rate.price.currency, "EUR");
        assert_eq!(rate.unit, RateUnit::PerMonth);
        // The GEX line is sold in every admitted (EU) region.
        assert_eq!(gex.regions.len(), caps.regions.len());
    }

    #[test]
    fn gpu_offerings_are_sold_only_in_eu_datacenters() {
        // Widen to US: the US data centers are admitted as regions, but Hetzner's
        // GPU hardware lives only in the EU, so the GEX offerings are not sold
        // there.
        let widened = HetznerProvider::for_residencies(&[Residency::Eu, Residency::Us]);
        let caps = widened.capabilities();
        assert!(caps.regions.iter().any(|r| r.code == "ash"));
        for offering in &caps.compute {
            assert!(offering.regions.iter().all(|c| c != "ash" && c != "hil"));
            assert!(offering.regions.iter().any(|c| c == "nbg1"));
        }
    }

    #[test]
    fn descriptor_reports_bare_metal_provisioning() {
        let caps = HetznerProvider::new().capabilities();
        assert_eq!(caps.provisioning, ProvisioningMethod::ApiAndSsh);
        assert!(caps.orchestration.bare_metal);
        assert!(!caps.orchestration.managed_kubernetes);
        assert!(caps.network.private_network);
        assert!(!caps.lifecycle.supports_hibernation);
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let mock = Arc::new(
            scoped_mock()
                .on(HttpMethod::Post, "/servers", created(42))
                .on(
                    HttpMethod::Get,
                    "/servers/42",
                    HttpResponse::new(200, br#"{"server":{"id":42,"status":"running"}}"#.to_vec()),
                )
                .on(
                    HttpMethod::Delete,
                    "/servers/42",
                    HttpResponse::new(200, br#"{"action":{"id":1}}"#.to_vec()),
                ),
        );
        let p = mock_provider(mock.clone());
        let req = ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1");

        let handle = p.provision_request(&req, &no_creds()).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.id, "42");
        assert_eq!(handle.provider, AdapterId::new("hetzner"));

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
                .any(|r| r.method == HttpMethod::Post && r.url.ends_with("/servers"))
        );
        assert!(seen.iter().any(|r| r.method == HttpMethod::Delete));
    }

    #[tokio::test]
    async fn provision_in_non_eu_region_is_rejected() {
        // ash (US) and sin (APAC) are real Hetzner data centers, but the EU
        // adapter admits neither: the residency filter restricts servers to
        // compliant regions, before any backend call.
        let p = HetznerProvider::new();
        for region in ["ash", "hil", "sin"] {
            let err = p
                .provision_request(
                    &ProvisionRequest::new("gpu.rtx-4000-ada.1x", region),
                    &no_creds(),
                )
                .await
                .expect_err("non-EU region must be rejected");
            assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
            assert!(!err.is_retryable());
        }
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let err = HetznerProvider::new()
            .provision_request(&ProvisionRequest::new("nope", "nbg1"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn gpu_offering_in_admitted_non_gpu_region_is_rejected() {
        // Widening residency admits US data centers, but the GEX line is sold
        // only in the GPU-capable EU sites: provisioning it in an admitted-yet-
        // non-GPU region must still be rejected, not silently placed there.
        let p = HetznerProvider::for_residencies(&[Residency::Eu, Residency::Us]);
        assert!(p.capabilities().regions.iter().any(|r| r.code == "ash"));
        let err = p
            .provision_request(
                &ProvisionRequest::new("gpu.rtx-4000-ada.1x", "ash"),
                &no_creds(),
            )
            .await
            .expect_err("GPU offering not sold in this region");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = HetznerProvider::new();
        let resolver = TestResolver::default().with("hetzner_token", "");
        let req = ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1")
            .with_credentials(SecretRef::new("hetzner_token"));
        let err = p
            .provision_request(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn non_utf8_credential_is_rejected() {
        let p = HetznerProvider::new();
        let resolver = TestResolver::default().with_bytes("hetzner_token", vec![0xff, 0xfe]);
        let req = ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1")
            .with_credentials(SecretRef::new("hetzner_token"));
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
            "/servers",
            HttpResponse::new(401, b"unauthorized".to_vec()),
        ));
        let p = mock_provider(mock);
        let req = ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1")
            .with_credentials(SecretRef::new("hetzner_token"));
        let resolver = TestResolver::default().with("hetzner_token", "bad");
        let err = p
            .provision_request(&req, &resolver)
            .await
            .expect_err("401 must reject credentials");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_404_to_unknown_offering() {
        let mock = Arc::new(scoped_mock().on(
            HttpMethod::Post,
            "/servers",
            HttpResponse::new(404, b"no such server type".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision_request(
                &ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1"),
                &no_creds(),
            )
            .await
            .expect_err("create 404 maps to unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_429_to_capacity_unavailable() {
        let mock = Arc::new(scoped_mock().on(
            HttpMethod::Post,
            "/servers",
            HttpResponse::new(429, b"rate_limit_exceeded".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision_request(
                &ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1"),
                &no_creds(),
            )
            .await
            .expect_err("429 must map to capacity unavailable");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-001");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_500_to_retryable_backend() {
        let mock = Arc::new(scoped_mock().on(
            HttpMethod::Post,
            "/servers",
            HttpResponse::new(500, b"boom".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision_request(
                &ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1"),
                &no_creds(),
            )
            .await
            .expect_err("5xx maps to backend");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn provision_malformed_success_body_is_backend_error() {
        let mock = Arc::new(scoped_mock().on(
            HttpMethod::Post,
            "/servers",
            HttpResponse::new(201, b"not json".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision_request(
                &ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1"),
                &no_creds(),
            )
            .await
            .expect_err("malformed 2xx body maps to backend");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
    }

    #[tokio::test]
    async fn status_maps_404_to_unknown_instance() {
        let mock = Arc::new(
            scoped_mock()
                .on(HttpMethod::Post, "/servers", created(7))
                .on(
                    HttpMethod::Get,
                    "/servers/7",
                    HttpResponse::new(404, b"not found".to_vec()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p
            .provision_request(
                &ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1"),
                &no_creds(),
            )
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
                .on(HttpMethod::Post, "/servers", created(7))
                .on(
                    HttpMethod::Delete,
                    "/servers/7",
                    HttpResponse::new(404, b"gone".to_vec()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p
            .provision_request(
                &ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1"),
                &no_creds(),
            )
            .await
            .unwrap();
        // Idempotent: a 404 on delete means already gone.
        let outcome = p.teardown(&handle, &no_creds()).await.unwrap();
        assert_eq!(outcome, TeardownOutcome::ConfirmedGone);
    }

    #[tokio::test]
    async fn transport_failure_maps_to_retryable_backend() {
        let mock =
            Arc::new(scoped_mock().on_error(HttpMethod::Post, "/servers", TransportError::Timeout));
        let err = mock_provider(mock)
            .provision_request(
                &ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1"),
                &no_creds(),
            )
            .await
            .expect_err("transport failure surfaces as an error");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let secret_value = "leak-me-hetzner-token";
        let resolver = TestResolver::default().with("hetzner_token", secret_value);
        let mock = Arc::new(scoped_mock().on(HttpMethod::Post, "/servers", created(9)));
        let p = mock_provider(mock.clone());
        let req = ProvisionRequest::new("gpu.rtx-4000-ada.1x", "nbg1")
            .with_credentials(SecretRef::new("hetzner_token"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("hetzner_token"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision_request(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("hetzner_token"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"api_token\""));

        // The token was sent as a bearer on the wire but never persisted.
        let seen = mock.requests();
        assert_eq!(seen[0].bearer.as_deref(), Some(secret_value));
    }

    #[tokio::test]
    async fn describe_never_serializes_non_eu_regions() {
        let describe_json = serde_json::to_string(&HetznerProvider::new().describe()).unwrap();
        // Quoted forms so a code is not matched as a substring of a location
        // name (e.g. "sin" inside "Helsinki").
        assert!(describe_json.contains("\"nbg1\""));
        assert!(!describe_json.contains("\"ash\""));
        assert!(!describe_json.contains("\"hil\""));
        assert!(!describe_json.contains("\"sin\""));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mock = Arc::new(scoped_mock().on(HttpMethod::Post, "/servers", created(3)));
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(mock_provider(mock)));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("hetzner")).unwrap();
        let handle = found
            .provision_request(
                &ProvisionRequest::new("gpu.rtx-6000-ada.1x", "fsn1"),
                &no_creds(),
            )
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = HetznerProvider::new().with_health(HealthStatus::Degraded {
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
