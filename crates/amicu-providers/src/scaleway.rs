//! The Scaleway [`Provider`] adapter, with EU data-residency filtering.
//!
//! Scaleway is a French sovereign cloud offering provider-managed GPU instances
//! across European regions. Amicu targets EU workloads, so this adapter is
//! **residency filtering by construction**: [`ScalewayProvider::new`] admits only
//! the EU regions from [`REGIONS`], and [`provision`](ScalewayProvider::provision)
//! rejects any region outside that admitted set with
//! [`ProviderError::UnknownRegion`]. Every Scaleway region is EU, so the EU filter
//! admits all of them — but the filter is modeled identically to a mixed-residency
//! provider, so widening past EU (e.g. to [`Residency::Us`]) correctly yields an
//! empty region set. [`ScalewayProvider::for_residencies`] widens the admitted set
//! explicitly when a caller genuinely needs other jurisdictions.
//!
//! The [`ProviderCapabilities`] descriptor carries Scaleway's EU regions and a
//! representative slice of its single-GPU catalog. Scaleway has no public spot
//! market, so offerings are priced **on-demand only** (no spot rate).
//!
//! Provisioning drives Scaleway's Instance API
//! (`https://api.scaleway.com/instance/v1`) through the [`HttpTransport`] seam:
//! `POST /zones/{zone}/servers` to create, `POST /zones/{zone}/servers/{id}/action`
//! (`{"action":"poweron"}`) to boot it, `GET /zones/{zone}/servers/{id}` for
//! status, and `DELETE /zones/{zone}/servers/{id}` to tear down. Scaleway
//! authenticates with an `X-Auth-Token` header (not `Authorization: Bearer`).
//! The adapter never touches a concrete HTTP client — production uses
//! [`ReqwestTransport`], tests inject a mock — so the provisioning path is
//! exercised with no network, credentials, or spend. HTTP status codes map onto
//! typed [`ProviderError`]s (`401`/`403` → credentials rejected, `404` → unknown
//! instance/offering, `409`/`429` → capacity unavailable, other non-`2xx` →
//! retryable backend); a transport failure maps to a retryable backend error.
//! Real-time capacity/pricing sync is out of scope here (TASK-023); prices are
//! illustrative catalog values in EUR/hr, not a live quote.

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
};
use crate::transport::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport, TransportError};

/// The adapter identity Scaleway registers under.
pub const PROVIDER_ID: &str = "scaleway";

/// The Scaleway Instance API base URL.
const BASE_URL: &str = "https://api.scaleway.com/instance/v1";

/// Scaleway's non-bearer auth header.
const AUTH_HEADER: &str = "X-Auth-Token";

/// A Scaleway region: its region code, residency jurisdiction, ISO country code,
/// and human-readable location. Every Scaleway region is EU, so the catalog holds
/// only EU entries; the residency filter still runs identically to a
/// mixed-residency provider.
struct ScwRegion {
    /// Scaleway region code, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
}

/// Scaleway's regions — all EU. [`ScalewayProvider::new`] admits every one; the
/// filter is a no-op for the EU default but excludes them all when widened past
/// EU. Uses a `fn` (not a `const`) because [`Residency::Other`] owns a `String`.
fn regions() -> [ScwRegion; 3] {
    [
        ScwRegion {
            code: "fr-par",
            residency: Residency::Eu,
            country: "FR",
            location: "Paris",
        },
        ScwRegion {
            code: "nl-ams",
            residency: Residency::Eu,
            country: "NL",
            location: "Amsterdam",
        },
        ScwRegion {
            code: "pl-waw",
            residency: Residency::Eu,
            country: "PL",
            location: "Warsaw",
        },
    ]
}

/// One entry of Scaleway's GPU catalog: an offering id, the accelerator model as
/// Scaleway names it, its VRAM in GiB, representative host resources, and an
/// illustrative on-demand rate in EUR per hour. Scaleway has no public spot
/// market, so there is no spot rate.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    vram_gib: u32,
    vcpus: u32,
    host_mem_gib: u32,
    on_demand: f64,
    availability: AvailabilityState,
}

/// A representative slice of Scaleway's single-GPU catalog. Prices are
/// illustrative catalog values in EUR/hr, not a live quote (real-time pricing is
/// TASK-023). On-demand only — Scaleway has no public spot market.
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "gpu.h100.1x",
        model: "h100",
        vram_gib: 80,
        vcpus: 24,
        host_mem_gib: 240,
        on_demand: 2.73,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "gpu.l40s.1x",
        model: "l40s",
        vram_gib: 48,
        vcpus: 8,
        host_mem_gib: 96,
        on_demand: 1.24,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.l4.1x",
        model: "l4",
        vram_gib: 24,
        vcpus: 8,
        host_mem_gib: 48,
        on_demand: 0.75,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.p100.1x",
        model: "p100",
        vram_gib: 16,
        vcpus: 10,
        host_mem_gib: 42,
        on_demand: 0.61,
        availability: AvailabilityState::Available,
    },
];

/// Request body for `POST {base}/zones/{zone}/servers`.
///
/// A minimal create body: the offering id stands in for Scaleway's
/// `commercial_type`. Richer fields (image, volumes, project) arrive with live
/// provisioning (TASK-023).
#[derive(Serialize)]
struct CreateServerRequest<'a> {
    name: &'a str,
    commercial_type: &'a str,
}

/// Request body for the boot action `POST .../servers/{id}/action`.
#[derive(Serialize)]
struct ServerActionRequest {
    action: &'static str,
}

/// The Scaleway `{"server":{...}}` envelope returned by create and status.
#[derive(Deserialize)]
struct ServerEnvelope {
    server: ServerObject,
}

/// The subset of a Scaleway server object the adapter consumes.
#[derive(Deserialize)]
struct ServerObject {
    /// Provider-assigned server id (a UUID).
    id: String,
    /// Lifecycle state, e.g. `"running"`.
    state: Option<String>,
}

/// Map a Scaleway server state string onto a [`ProvisioningState`].
fn map_server_state(state: Option<&str>) -> ProvisioningState {
    match state.unwrap_or_default() {
        "running" => ProvisioningState::Ready {
            detail: "instance running".into(),
        },
        "starting" => ProvisioningState::Provisioning {
            detail: "instance starting".into(),
        },
        "stopping" => ProvisioningState::Stopping {
            detail: "instance stopping".into(),
        },
        "stopped" | "stopped in place" => ProvisioningState::Stopped {
            detail: "instance stopped".into(),
        },
        "locked" => ProvisioningState::Failed {
            detail: "instance locked".into(),
        },
        "" => ProvisioningState::Provisioning {
            detail: "instance state unknown".into(),
        },
        other => ProvisioningState::Provisioning {
            detail: format!("instance state {other}"),
        },
    }
}

/// Fold a transport failure (the exchange never completed) into a retryable
/// backend error.
fn transport_error(err: &TransportError) -> AdapterError {
    AdapterError::from_kind(ProviderError::Backend {
        detail: format!("Scaleway transport failure: {err}"),
    })
}

/// Map a non-`2xx` status from a create-path call (create / boot action) onto a
/// typed error; `404` means the offering is unknown.
fn create_error(status: u16, request: &ProvisionRequest) -> AdapterError {
    let kind = match status {
        401 | 403 => ProviderError::CredentialsRejected {
            detail: "Scaleway rejected the API secret key".into(),
        },
        404 => ProviderError::UnknownOffering {
            offering: request.offering_id.clone(),
        },
        409 | 429 => ProviderError::CapacityUnavailable {
            offering: request.offering_id.clone(),
            region: request.region.clone(),
        },
        _ => ProviderError::Backend {
            detail: format!("Scaleway API returned HTTP {status}"),
        },
    };
    AdapterError::from_kind(kind)
}

/// Map a non-`2xx` status from a per-instance call (`status`/`teardown`) onto a
/// typed error; `404` means the instance is unknown.
fn lifecycle_error(status: u16, id: &str) -> AdapterError {
    let kind = match status {
        401 | 403 => ProviderError::CredentialsRejected {
            detail: "Scaleway rejected the API secret key".into(),
        },
        404 => ProviderError::UnknownInstance { id: id.into() },
        _ => ProviderError::Backend {
            detail: format!("Scaleway API returned HTTP {status}"),
        },
    };
    AdapterError::from_kind(kind)
}

/// The Scaleway provider adapter.
///
/// Holds the admitted (residency-filtered) region set, the derived capability
/// descriptor, and the [`HttpTransport`] used to reach Scaleway's REST API.
pub struct ScalewayProvider {
    id: AdapterId,
    capabilities: ProviderCapabilities,
    health: HealthStatus,
    transport: Arc<dyn HttpTransport>,
    base_url: String,
}

impl ScalewayProvider {
    /// A Scaleway provider admitting only EU regions.
    ///
    /// This is the residency-safe default: no non-EU region reaches the
    /// descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// A Scaleway provider admitting the regions whose residency is in `allowed`.
    /// Use this to deliberately widen beyond the EU default.
    #[must_use]
    pub fn for_residencies(allowed: &[Residency]) -> Self {
        let id = AdapterId::new(PROVIDER_ID);
        let regions: Vec<Region> = regions()
            .into_iter()
            .filter(|r| allowed.contains(&r.residency))
            .map(|r| {
                Region::new(r.code, r.residency)
                    .with_country(r.country)
                    .with_display_name(r.location)
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
    fn with_transport(
        mut self,
        transport: Arc<dyn HttpTransport>,
        base_url: impl Into<String>,
    ) -> Self {
        self.transport = transport;
        self.base_url = base_url.into();
        self
    }

    /// The Scaleway region codes admitted for `residency`.
    #[must_use]
    pub fn region_codes(residency: &Residency) -> Vec<&'static str> {
        regions()
            .into_iter()
            .filter(|r| &r.residency == residency)
            .map(|r| r.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. Every
    /// offering is sold in every admitted region (Scaleway's GPU catalog is
    /// region-agnostic at this granularity).
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let region_codes: Vec<String> = regions.iter().map(|r| r.code.clone()).collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::Api,
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
                }),
                vcpus: Some(g.vcpus),
                memory: Some(
                    format!("{}GiB", g.host_mem_gib)
                        .parse()
                        .expect("valid size"),
                ),
                regions: region_codes.clone(),
                // On-demand only: Scaleway has no public spot market.
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
        ];
        caps.network = NetworkCapabilities {
            bandwidth_mbps: Some(10_000),
            private_network: true,
        };
        caps.orchestration = OrchestrationSupport {
            managed_kubernetes: true, // Kapsule
            bare_metal: true,         // Elastic Metal
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(45),
            teardown_secs: Some(20),
            supports_hibernation: true,
        };
        caps.model_download = ModelDownloadPerformance {
            throughput_mbps: Some(5_000),
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

    /// Build a request for `{base}{path}`, attaching `token` in the Scaleway
    /// `X-Auth-Token` header when present.
    fn build_request(&self, method: HttpMethod, path: &str, token: Option<&str>) -> HttpRequest {
        let mut req = HttpRequest::new(method, format!("{}{path}", self.base_url));
        if let Some(token) = token {
            req = req.with_header(AUTH_HEADER, token);
        }
        req
    }

    /// Resolve the credential to a transient secret-key token for a single REST
    /// call.
    ///
    /// The returned [`String`] populates the `X-Auth-Token` header for the
    /// immediate call and is dropped when it returns; it is never stored on a
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
                    detail: "empty Scaleway API secret key".into(),
                },
            ));
        }
        let token = value.expose_secret_str().map(str::to_owned).map_err(|_| {
            AdapterError::from_kind(ProviderError::CredentialsRejected {
                detail: "Scaleway API secret key is not valid UTF-8".into(),
            })
        })?;
        Ok(Some(token))
    }
}

impl Default for ScalewayProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for ScalewayProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for ScalewayProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for ScalewayProvider {
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
        // Residency filter: only admitted (EU by default) regions can host an
        // instance. Any region outside the admitted set is rejected here.
        if !self.admits_region(&request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        let token = self
            .resolve_token(resolver, request.credentials.as_ref())
            .await?;

        let create = CreateServerRequest {
            name: "amicu-instance",
            commercial_type: request.offering_id.as_str(),
        };
        let body = serde_json::to_value(&create).map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "failed to encode Scaleway create request".into(),
            })
        })?;
        // TODO(TASK-023): Scaleway's Instance API path segment is a zone
        // (e.g. `fr-par-1`), not a region (`fr-par`); resolve region→zone
        // before live calls.
        let servers_path = format!("/zones/{}/servers", request.region);
        let create_request = self
            .build_request(HttpMethod::Post, &servers_path, token.as_deref())
            .with_json(body);
        let response = self
            .transport
            .execute(create_request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            return Err(create_error(response.status, request));
        }
        let envelope: ServerEnvelope = response.json().map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "malformed Scaleway create response".into(),
            })
        })?;
        let server_id = envelope.server.id;

        // A freshly created Scaleway server is stopped; boot it explicitly.
        let action_body =
            serde_json::to_value(ServerActionRequest { action: "poweron" }).map_err(|_| {
                AdapterError::from_kind(ProviderError::Backend {
                    detail: "failed to encode Scaleway action request".into(),
                })
            })?;
        let action_path = format!("/zones/{}/servers/{server_id}/action", request.region);
        let action_request = self
            .build_request(HttpMethod::Post, &action_path, token.as_deref())
            .with_json(action_body);
        let action_response = self
            .transport
            .execute(action_request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !action_response.is_success() {
            // Boot failed after create succeeded: best-effort teardown so the
            // created server does not linger (and bill). Its result is ignored;
            // the boot failure is the error surfaced. A 404 here means the
            // just-created instance, not the offering.
            let cleanup_path = format!("/zones/{}/servers/{server_id}", request.region);
            let cleanup = self.build_request(HttpMethod::Delete, &cleanup_path, token.as_deref());
            let _ = self.transport.execute(cleanup).await;
            return Err(lifecycle_error(action_response.status, &server_id));
        }
        Ok(ProvisioningHandle {
            id: server_id,
            provider: self.id.clone(),
            region: request.region.clone(),
            offering_id: request.offering_id.clone(),
            credentials: request.credentials.clone(),
            state: map_server_state(envelope.server.state.as_deref()),
        })
    }

    async fn status(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningState, AdapterError> {
        let token = self
            .resolve_token(resolver, handle.credentials.as_ref())
            .await?;
        let path = format!("/zones/{}/servers/{}", handle.region, handle.id);
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
                detail: "malformed Scaleway status response".into(),
            })
        })?;
        Ok(map_server_state(envelope.server.state.as_deref()))
    }

    async fn teardown(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<(), AdapterError> {
        let token = self
            .resolve_token(resolver, handle.credentials.as_ref())
            .await?;
        let path = format!("/zones/{}/servers/{}", handle.region, handle.id);
        let request = self.build_request(HttpMethod::Delete, &path, token.as_deref());
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            return Err(lifecycle_error(response.status, &handle.id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// A Scaleway provider whose transport is `mock` and whose base URL points at
    /// a harmless test host (the mock matches on the URL suffix).
    fn mock_provider(mock: Arc<MockTransport>) -> ScalewayProvider {
        ScalewayProvider::new().with_transport(mock, "https://mock.test/instance/v1")
    }

    /// A successful create response for server `id`, `running`.
    fn created(id: &str) -> HttpResponse {
        HttpResponse::new(
            201,
            format!(r#"{{"server":{{"id":"{id}","state":"running"}}}}"#).into_bytes(),
        )
    }

    /// A successful boot-action response.
    fn poweron_ok() -> HttpResponse {
        HttpResponse::new(200, br#"{"task":{"id":"t-1","status":"pending"}}"#.to_vec())
    }

    #[test]
    fn new_admits_only_eu_regions() {
        let caps = ScalewayProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("scaleway"));
        assert!(!caps.regions.is_empty());
        // Every admitted region is EU...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known Scaleway regions are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"fr-par"));
        assert!(codes.contains(&"nl-ams"));
        assert!(codes.contains(&"pl-waw"));
    }

    #[test]
    fn widening_past_eu_yields_no_regions() {
        // Scaleway has no non-EU regions, so widening the residency filter to a
        // non-EU jurisdiction admits nothing — the filter is a real filter, not a
        // hand-picked list.
        assert!(ScalewayProvider::region_codes(&Residency::Us).is_empty());
        let widened = ScalewayProvider::for_residencies(&[Residency::Us]);
        assert!(widened.capabilities().regions.is_empty());
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = ScalewayProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_on_demand_only_pricing() {
        let caps = ScalewayProvider::new().capabilities();
        let h100 = caps
            .compute
            .iter()
            .find(|o| o.id == "gpu.h100.1x")
            .expect("h100 offering present");
        // Scaleway prices on-demand only; no spot rate is offered.
        assert!(!h100.pricing.has_spot());
        assert!(h100.pricing.spot_rate().is_none());
        assert_eq!(h100.pricing.on_demand_rate().price.currency, "EUR");
        // H100 capacity is constrained.
        assert!(!h100.availability.is_available());
        // Every offering is sold in every admitted (EU) region.
        assert_eq!(h100.regions.len(), caps.regions.len());
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let mock = Arc::new(
            MockTransport::default()
                .on(HttpMethod::Post, "/zones/fr-par/servers", created("srv-1"))
                .on(HttpMethod::Post, "/servers/srv-1/action", poweron_ok())
                .on(
                    HttpMethod::Get,
                    "/servers/srv-1",
                    HttpResponse::new(
                        200,
                        br#"{"server":{"id":"srv-1","state":"running"}}"#.to_vec(),
                    ),
                )
                .on(
                    HttpMethod::Delete,
                    "/servers/srv-1",
                    HttpResponse::new(204, Vec::new()),
                ),
        );
        let p = mock_provider(mock.clone());
        let req = ProvisionRequest::new("gpu.l40s.1x", "fr-par");

        let handle = p.provision(&req, &no_creds()).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.id, "srv-1");
        assert_eq!(handle.provider, AdapterId::new("scaleway"));

        assert!(p.status(&handle, &no_creds()).await.unwrap().is_ready());
        p.teardown(&handle, &no_creds()).await.unwrap();

        // The lifecycle hit the expected REST endpoints in order: create, boot
        // action, status, teardown.
        let seen = mock.requests();
        assert_eq!(seen[0].method, HttpMethod::Post);
        assert!(seen[0].url.ends_with("/zones/fr-par/servers"));
        assert_eq!(seen[1].method, HttpMethod::Post);
        assert!(seen[1].url.ends_with("/action"));
        assert_eq!(seen[2].method, HttpMethod::Get);
        assert_eq!(seen[3].method, HttpMethod::Delete);
    }

    #[tokio::test]
    async fn provision_in_unknown_region_is_rejected() {
        // A region outside the admitted EU set is rejected by the residency
        // filter, before any backend call.
        let p = ScalewayProvider::new();
        let err = p
            .provision(
                &ProvisionRequest::new("gpu.l40s.1x", "us-east-1"),
                &no_creds(),
            )
            .await
            .expect_err("unknown region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let err = ScalewayProvider::new()
            .provision(&ProvisionRequest::new("nope", "fr-par"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = ScalewayProvider::new();
        let resolver = TestResolver::default().with("scaleway_secret_key", "");
        let req = ProvisionRequest::new("gpu.l40s.1x", "fr-par")
            .with_credentials(SecretRef::new("scaleway_secret_key"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn non_utf8_credential_is_rejected() {
        let p = ScalewayProvider::new();
        let resolver = TestResolver::default().with_bytes("scaleway_secret_key", vec![0xff, 0xfe]);
        let req = ProvisionRequest::new("gpu.l40s.1x", "fr-par")
            .with_credentials(SecretRef::new("scaleway_secret_key"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("non-UTF-8 credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_poweron_failure_tears_down_and_reports_unknown_instance() {
        // Create succeeds, boot action 404s: the orphaned server must be torn
        // down and the error mapped to UnknownInstance (not UnknownOffering).
        let mock = Arc::new(
            MockTransport::default()
                .on(HttpMethod::Post, "/zones/fr-par/servers", created("srv-9"))
                .on(
                    HttpMethod::Post,
                    "/servers/srv-9/action",
                    HttpResponse::new(404, b"gone".to_vec()),
                )
                .on(
                    HttpMethod::Delete,
                    "/servers/srv-9",
                    HttpResponse::new(204, Vec::new()),
                ),
        );
        let p = mock_provider(mock.clone());
        let err = p
            .provision(&ProvisionRequest::new("gpu.l40s.1x", "fr-par"), &no_creds())
            .await
            .expect_err("poweron failure surfaces");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-012");
        assert!(!err.is_retryable());
        // The orphaned server was cleaned up.
        let seen = mock.requests();
        assert!(
            seen.iter()
                .any(|r| r.method == HttpMethod::Delete && r.url.ends_with("/servers/srv-9"))
        );
    }

    #[tokio::test]
    async fn provision_maps_401_to_credentials_rejected() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/zones/fr-par/servers",
            HttpResponse::new(401, b"denied".to_vec()),
        ));
        let p = mock_provider(mock);
        let req = ProvisionRequest::new("gpu.l40s.1x", "fr-par")
            .with_credentials(SecretRef::new("scaleway_secret_key"));
        let resolver = TestResolver::default().with("scaleway_secret_key", "bad");
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("401 must reject credentials");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_404_to_unknown_offering() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/zones/fr-par/servers",
            HttpResponse::new(404, b"no such commercial_type".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision(&ProvisionRequest::new("gpu.l40s.1x", "fr-par"), &no_creds())
            .await
            .expect_err("create 404 maps to unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_429_to_capacity_unavailable() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/zones/fr-par/servers",
            HttpResponse::new(429, b"too many requests".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision(&ProvisionRequest::new("gpu.l40s.1x", "fr-par"), &no_creds())
            .await
            .expect_err("429 must map to capacity unavailable");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-001");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_500_to_retryable_backend() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/zones/fr-par/servers",
            HttpResponse::new(500, b"boom".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision(&ProvisionRequest::new("gpu.l40s.1x", "fr-par"), &no_creds())
            .await
            .expect_err("5xx maps to backend");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn provision_malformed_success_body_is_backend_error() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/zones/fr-par/servers",
            HttpResponse::new(201, b"not json".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision(&ProvisionRequest::new("gpu.l40s.1x", "fr-par"), &no_creds())
            .await
            .expect_err("malformed 2xx body maps to backend");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
    }

    #[tokio::test]
    async fn status_maps_404_to_unknown_instance() {
        let mock = Arc::new(
            MockTransport::default()
                .on(HttpMethod::Post, "/zones/fr-par/servers", created("srv-2"))
                .on(HttpMethod::Post, "/servers/srv-2/action", poweron_ok())
                .on(
                    HttpMethod::Get,
                    "/servers/srv-2",
                    HttpResponse::new(404, b"not found".to_vec()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p
            .provision(&ProvisionRequest::new("gpu.l40s.1x", "fr-par"), &no_creds())
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
    async fn teardown_maps_404_to_unknown_instance() {
        let mock = Arc::new(
            MockTransport::default()
                .on(HttpMethod::Post, "/zones/fr-par/servers", created("srv-2"))
                .on(HttpMethod::Post, "/servers/srv-2/action", poweron_ok())
                .on(
                    HttpMethod::Delete,
                    "/servers/srv-2",
                    HttpResponse::new(404, b"gone".to_vec()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p
            .provision(&ProvisionRequest::new("gpu.l40s.1x", "fr-par"), &no_creds())
            .await
            .unwrap();
        let err = p
            .teardown(&handle, &no_creds())
            .await
            .expect_err("teardown 404 maps to unknown instance");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-012");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn transport_failure_maps_to_retryable_backend() {
        let mock = Arc::new(MockTransport::default().on_error(
            HttpMethod::Post,
            "/zones/fr-par/servers",
            TransportError::Timeout,
        ));
        let err = mock_provider(mock)
            .provision(&ProvisionRequest::new("gpu.l40s.1x", "fr-par"), &no_creds())
            .await
            .expect_err("transport failure surfaces as an error");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let secret_value = "leak-me-scaleway-secret-key";
        let resolver = TestResolver::default().with("scaleway_secret_key", secret_value);
        let mock = Arc::new(
            MockTransport::default()
                .on(HttpMethod::Post, "/zones/fr-par/servers", created("srv-3"))
                .on(HttpMethod::Post, "/servers/srv-3/action", poweron_ok()),
        );
        let p = mock_provider(mock.clone());
        let req = ProvisionRequest::new("gpu.l40s.1x", "fr-par")
            .with_credentials(SecretRef::new("scaleway_secret_key"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("scaleway_secret_key"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("scaleway_secret_key"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"api_key\""));

        // The token rode in the X-Auth-Token header (never as a bearer) and never
        // landed in any serialized artifact.
        let seen = mock.requests();
        assert!(seen[0].bearer.is_none());
        assert!(
            seen[0]
                .headers
                .iter()
                .any(|(k, v)| k == "X-Auth-Token" && v == secret_value)
        );
    }

    #[tokio::test]
    async fn describe_serializes_only_eu_regions() {
        let describe_json = serde_json::to_string(&ScalewayProvider::new().describe()).unwrap();
        assert!(describe_json.contains("fr-par"));
        assert!(describe_json.contains("nl-ams"));
        assert!(describe_json.contains("pl-waw"));
        // No spot pricing appears in the on-demand-only descriptor.
        assert!(!describe_json.contains("spot"));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mock = Arc::new(
            MockTransport::default()
                .on(HttpMethod::Post, "/zones/nl-ams/servers", created("srv-4"))
                .on(HttpMethod::Post, "/servers/srv-4/action", poweron_ok()),
        );
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(mock_provider(mock)));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("scaleway")).unwrap();
        let handle = found
            .provision(&ProvisionRequest::new("gpu.l4.1x", "nl-ams"), &no_creds())
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = ScalewayProvider::new().with_health(HealthStatus::Degraded {
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
