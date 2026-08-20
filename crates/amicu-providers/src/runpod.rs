//! The RunPod [`Provider`] adapter, with EU data-residency filtering.
//!
//! RunPod offers provider-managed GPU pods across data centers worldwide. Amicu
//! targets EU workloads, so this adapter is **residency filtering by
//! construction**: [`RunPodProvider::new`] admits only the EU/EEA data centers
//! from [`datacenters`], and [`provision`](RunPodProvider::provision) rejects any
//! region outside that admitted set with [`ProviderError::UnknownRegion`] before
//! any backend call — a pod can never be placed in a non-compliant region through
//! this adapter. [`RunPodProvider::for_residencies`] widens the admitted set
//! explicitly when a caller genuinely needs other jurisdictions.
//!
//! Provisioning drives RunPod's REST API (`https://rest.runpod.io/v1`) through the
//! [`HttpTransport`] seam: `POST /pods` to create, `GET /pods/{id}` for status,
//! and `DELETE /pods/{id}` to tear down. The adapter never touches a concrete HTTP
//! client — production uses [`ReqwestTransport`], tests inject a mock — so the
//! provisioning path is exercised with no network, credentials, or spend. HTTP
//! status codes map onto typed [`ProviderError`]s (`401`/`403` → credentials
//! rejected, `404` → unknown instance/offering, `409`/`429` → capacity
//! unavailable, other non-`2xx` → retryable backend); a transport failure that
//! never reached the API maps to a retryable backend error.
//!
//! The [`ProviderCapabilities`] descriptor carries RunPod's real EU data centers
//! and a representative slice of its GPU catalog with on-demand and interruptible
//! (spot) pricing. Real-time capacity/pricing sync is out of scope here
//! (amicu-planning#23); prices are illustrative catalog values, not a live quote.

use std::sync::Arc;

use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_core::resource::{Accelerator, AcceleratorKind, Money};
use amicu_core::secret::{SecretRef, SecretResolver};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::{
    Availability, AvailabilityState, BillingGranularity, ComputeOffering, CredentialsModel,
    LifecycleBehavior, ModelDownloadPerformance, NetworkCapabilities, Pricing,
    ProviderCapabilities, ProvisioningMethod, Rate, RateUnit, Region, Residency, StorageKind,
    StorageOption,
};
use crate::provider::{
    Provider, ProviderError, ProvisionRequest, ProvisioningHandle, ProvisioningState,
};
use crate::transport::{HttpMethod, HttpRequest, HttpTransport, ReqwestTransport, TransportError};

/// The adapter identity RunPod registers under.
pub const PROVIDER_ID: &str = "runpod";

/// The RunPod REST API base URL.
const BASE_URL: &str = "https://rest.runpod.io/v1";

/// A RunPod data center: its `dataCenterId`, residency jurisdiction, ISO country
/// code, and human-readable location. Both EU/EEA and non-EU entries are listed
/// so the EU filter has something to exclude.
struct DataCenter {
    /// RunPod `dataCenterId`, used as the [`Region::code`].
    code: &'static str,
    /// Residency jurisdiction the location falls under.
    residency: Residency,
    /// ISO 3166-1 alpha-2 country code.
    country: &'static str,
    /// Human-readable location.
    location: &'static str,
}

/// RunPod's data centers. EU/EEA entries are what [`RunPodProvider::new`] admits;
/// the non-EU entries exist so the residency filter is a real exclusion, not a
/// hand-picked list. Residency uses a `fn` (not a `const`) because
/// [`Residency::Other`] owns a `String`.
fn datacenters() -> [DataCenter; 12] {
    [
        // EU / EEA.
        DataCenter {
            code: "EU-RO-1",
            residency: Residency::Eu,
            country: "RO",
            location: "Romania",
        },
        DataCenter {
            code: "EU-SE-1",
            residency: Residency::Eu,
            country: "SE",
            location: "Sweden",
        },
        DataCenter {
            code: "EU-NL-1",
            residency: Residency::Eu,
            country: "NL",
            location: "Netherlands",
        },
        DataCenter {
            code: "EU-CZ-1",
            residency: Residency::Eu,
            country: "CZ",
            location: "Czechia",
        },
        DataCenter {
            code: "EUR-IS-1",
            residency: Residency::Eu,
            country: "IS",
            location: "Iceland",
        },
        DataCenter {
            code: "EUR-NO-1",
            residency: Residency::Eu,
            country: "NO",
            location: "Norway",
        },
        // Non-EU — excluded by the residency filter.
        DataCenter {
            code: "US-CA-1",
            residency: Residency::Us,
            country: "US",
            location: "California",
        },
        DataCenter {
            code: "US-GA-1",
            residency: Residency::Us,
            country: "US",
            location: "Georgia",
        },
        DataCenter {
            code: "US-KS-2",
            residency: Residency::Us,
            country: "US",
            location: "Kansas",
        },
        DataCenter {
            code: "CA-MTL-1",
            residency: Residency::Ca,
            country: "CA",
            location: "Montreal",
        },
        DataCenter {
            code: "AP-JP-1",
            residency: Residency::Apac,
            country: "JP",
            location: "Japan",
        },
        DataCenter {
            code: "OC-AU-1",
            residency: Residency::Apac,
            country: "AU",
            location: "Australia",
        },
    ]
}

/// One entry of RunPod's GPU catalog: an offering id, the accelerator model as
/// RunPod names it, its VRAM in GiB, representative host resources, and
/// illustrative on-demand / spot rates in USD per hour.
struct GpuSpec {
    id: &'static str,
    model: &'static str,
    vram_gib: u32,
    vcpus: u32,
    host_mem_gib: u32,
    on_demand: f64,
    spot: f64,
    availability: AvailabilityState,
}

/// A representative slice of RunPod's single-GPU catalog. Prices are illustrative
/// catalog values in USD/hr, not a live quote (real-time pricing is amicu-planning#23).
const GPUS: &[GpuSpec] = &[
    GpuSpec {
        id: "gpu.h100-80gb.1x",
        model: "h100",
        vram_gib: 80,
        vcpus: 20,
        host_mem_gib: 251,
        on_demand: 2.99,
        spot: 1.99,
        availability: AvailabilityState::Limited,
    },
    GpuSpec {
        id: "gpu.a100-80gb.1x",
        model: "a100",
        vram_gib: 80,
        vcpus: 16,
        host_mem_gib: 125,
        on_demand: 1.89,
        spot: 1.19,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.l40s.1x",
        model: "l40s",
        vram_gib: 48,
        vcpus: 16,
        host_mem_gib: 94,
        on_demand: 0.99,
        spot: 0.79,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.rtx-a6000.1x",
        model: "rtx-a6000",
        vram_gib: 48,
        vcpus: 8,
        host_mem_gib: 50,
        on_demand: 0.79,
        spot: 0.49,
        availability: AvailabilityState::Available,
    },
    GpuSpec {
        id: "gpu.rtx-4090.1x",
        model: "rtx-4090",
        vram_gib: 24,
        vcpus: 8,
        host_mem_gib: 41,
        on_demand: 0.69,
        spot: 0.34,
        availability: AvailabilityState::Available,
    },
];

/// Request body for `POST {base}/pods`.
///
/// A minimal create body: the offering id stands in for RunPod's GPU type and the
/// region for its data center. Richer fields (image, volumes, env) arrive with
/// live provisioning (amicu-planning#23 / the gated smoke tests).
#[derive(Serialize)]
struct CreatePodRequest<'a> {
    #[serde(rename = "computeType")]
    compute_type: &'a str,
    #[serde(rename = "gpuCount")]
    gpu_count: u32,
    #[serde(rename = "gpuTypeIds")]
    gpu_type_ids: [&'a str; 1],
    #[serde(rename = "dataCenterIds")]
    data_center_ids: [&'a str; 1],
}

/// The subset of a RunPod pod object the adapter consumes.
#[derive(Deserialize)]
struct PodResponse {
    /// Provider-assigned pod id.
    id: String,
    /// Desired lifecycle status, e.g. `"RUNNING"`.
    #[serde(rename = "desiredStatus")]
    desired_status: Option<String>,
}

/// Map a RunPod pod status string onto a [`ProvisioningState`].
fn map_pod_state(status: Option<&str>) -> ProvisioningState {
    match status.unwrap_or_default() {
        "RUNNING" => ProvisioningState::Ready {
            detail: "pod running".into(),
        },
        "CREATED" | "PENDING" | "RESTARTING" => ProvisioningState::Provisioning {
            detail: "pod starting".into(),
        },
        "STOPPED" | "EXITED" => ProvisioningState::Stopped {
            detail: "pod stopped".into(),
        },
        "TERMINATED" | "DEAD" => ProvisioningState::Terminated {
            detail: "pod terminated".into(),
        },
        "" => ProvisioningState::Provisioning {
            detail: "pod status unknown".into(),
        },
        other => ProvisioningState::Provisioning {
            detail: format!("pod status {other}"),
        },
    }
}

/// Fold a transport failure (the exchange never completed) into a retryable
/// backend error.
fn transport_error(err: &TransportError) -> AdapterError {
    AdapterError::from_kind(ProviderError::Backend {
        detail: format!("RunPod transport failure: {err}"),
    })
}

/// Map a non-`2xx` status from a per-instance call (`status`/`teardown`) onto a
/// typed error; `404` means the pod is unknown.
fn lifecycle_error(status: u16, id: &str) -> AdapterError {
    let kind = match status {
        401 | 403 => ProviderError::CredentialsRejected {
            detail: "RunPod rejected the API token".into(),
        },
        404 => ProviderError::UnknownInstance { id: id.into() },
        _ => ProviderError::Backend {
            detail: format!("RunPod API returned HTTP {status}"),
        },
    };
    AdapterError::from_kind(kind)
}

/// The RunPod provider adapter.
///
/// Holds the admitted (residency-filtered) region set, the derived capability
/// descriptor, and the [`HttpTransport`] used to reach RunPod's REST API.
pub struct RunPodProvider {
    id: AdapterId,
    capabilities: ProviderCapabilities,
    health: HealthStatus,
    transport: Arc<dyn HttpTransport>,
    base_url: String,
}

impl RunPodProvider {
    /// A RunPod provider admitting only EU/EEA data centers.
    ///
    /// This is the residency-safe default: no non-EU region reaches the
    /// descriptor or [`provision`](Self::provision).
    #[must_use]
    pub fn new() -> Self {
        Self::for_residencies(&[Residency::Eu])
    }

    /// A RunPod provider admitting the data centers whose residency is in
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
    fn with_transport(
        mut self,
        transport: Arc<dyn HttpTransport>,
        base_url: impl Into<String>,
    ) -> Self {
        self.transport = transport;
        self.base_url = base_url.into();
        self
    }

    /// The RunPod data-center codes admitted for `residency`.
    #[must_use]
    pub fn datacenter_codes(residency: &Residency) -> Vec<&'static str> {
        datacenters()
            .into_iter()
            .filter(|dc| &dc.residency == residency)
            .map(|dc| dc.code)
            .collect()
    }

    /// Build the capability descriptor from the admitted `regions`. Every
    /// offering is sold in every admitted region (RunPod's GPU catalog is
    /// data-center agnostic at this granularity).
    fn capabilities_for(id: &AdapterId, regions: Vec<Region>) -> ProviderCapabilities {
        let region_codes: Vec<String> = regions.iter().map(|r| r.code.clone()).collect();
        let mut caps = ProviderCapabilities::new(
            id.clone(),
            ProvisioningMethod::Api,
            CredentialsModel::ApiToken,
            BillingGranularity::PerSecond,
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
                pricing: Pricing::on_demand(Rate::new(
                    Money::new(g.on_demand, "USD"),
                    RateUnit::PerHour,
                ))
                .with_spot(Rate::new(Money::new(g.spot, "USD"), RateUnit::PerHour)),
                availability: Availability::new(g.availability),
            })
            .collect();
        caps.storage = vec![
            StorageOption::new(StorageKind::LocalNvme),
            StorageOption::new(StorageKind::NetworkFs),
        ];
        caps.network = NetworkCapabilities {
            bandwidth_mbps: Some(10_000),
            private_network: false,
        };
        caps.lifecycle = LifecycleBehavior {
            startup_secs: Some(30),
            teardown_secs: Some(10),
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
    /// The returned [`String`] is used only to populate
    /// [`HttpRequest::bearer`](crate::transport::HttpRequest) for the immediate
    /// call and is dropped when that call returns; it is never stored on a
    /// [`ProvisioningHandle`], logged, or serialized. `None` credentials yield
    /// `None` (no auth header); an empty credential is rejected as
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
                    detail: "empty RunPod API token".into(),
                },
            ));
        }
        let token = value.expose_secret_str().map(str::to_owned).map_err(|_| {
            AdapterError::from_kind(ProviderError::CredentialsRejected {
                detail: "RunPod API token is not valid UTF-8".into(),
            })
        })?;
        Ok(Some(token))
    }
}

impl Default for RunPodProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for RunPodProvider {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for RunPodProvider {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl Provider for RunPodProvider {
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
        // Residency filter: only admitted (EU by default) regions can host a pod.
        // A real RunPod data center outside the admitted set is rejected here,
        // before any backend call.
        if !self.admits_region(&request.region) {
            return Err(AdapterError::from_kind(ProviderError::UnknownRegion {
                region: request.region.clone(),
            }));
        }
        let token = self
            .resolve_token(resolver, request.credentials.as_ref())
            .await?;

        let create = CreatePodRequest {
            compute_type: "GPU",
            gpu_count: 1,
            gpu_type_ids: [request.offering_id.as_str()],
            data_center_ids: [request.region.as_str()],
        };
        let body = serde_json::to_value(&create).map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "failed to encode RunPod create request".into(),
            })
        })?;
        let http_request = self
            .build_request(HttpMethod::Post, "/pods", token.as_deref())
            .with_json(body);
        let response = self
            .transport
            .execute(http_request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            let kind = match response.status {
                401 | 403 => ProviderError::CredentialsRejected {
                    detail: "RunPod rejected the API token".into(),
                },
                404 => ProviderError::UnknownOffering {
                    offering: request.offering_id.clone(),
                },
                409 | 429 => ProviderError::CapacityUnavailable {
                    offering: request.offering_id.clone(),
                    region: request.region.clone(),
                },
                status => ProviderError::Backend {
                    detail: format!("RunPod API returned HTTP {status}"),
                },
            };
            return Err(AdapterError::from_kind(kind));
        }
        let pod: PodResponse = response.json().map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "malformed RunPod create response".into(),
            })
        })?;
        Ok(ProvisioningHandle {
            id: pod.id,
            provider: self.id.clone(),
            region: request.region.clone(),
            zone: None,
            offering_id: request.offering_id.clone(),
            credentials: request.credentials.clone(),
            state: map_pod_state(pod.desired_status.as_deref()),
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
        let path = format!("/pods/{}", handle.id);
        let request = self.build_request(HttpMethod::Get, &path, token.as_deref());
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|e| transport_error(&e))?;
        if !response.is_success() {
            return Err(lifecycle_error(response.status, &handle.id));
        }
        let pod: PodResponse = response.json().map_err(|_| {
            AdapterError::from_kind(ProviderError::Backend {
                detail: "malformed RunPod status response".into(),
            })
        })?;
        Ok(map_pod_state(pod.desired_status.as_deref()))
    }

    async fn teardown(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<(), AdapterError> {
        let token = self
            .resolve_token(resolver, handle.credentials.as_ref())
            .await?;
        let path = format!("/pods/{}", handle.id);
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

    /// A RunPod provider whose transport is `mock` and whose base URL points at a
    /// harmless test host (the mock matches on the URL suffix).
    fn mock_provider(mock: Arc<MockTransport>) -> RunPodProvider {
        RunPodProvider::new().with_transport(mock, "https://mock.test/v1")
    }

    /// A successful `POST /pods` create response for pod `id`, desired `RUNNING`.
    fn created(id: &str) -> HttpResponse {
        HttpResponse::new(
            201,
            format!(r#"{{"id":"{id}","desiredStatus":"RUNNING"}}"#).into_bytes(),
        )
    }

    #[test]
    fn new_admits_only_eu_regions() {
        let caps = RunPodProvider::new().capabilities();
        assert_eq!(caps.provider, AdapterId::new("runpod"));
        assert!(!caps.regions.is_empty());
        // Every admitted region is EU/EEA...
        assert!(caps.regions.iter().all(Region::is_eu));
        // ...and the known EU data centers are present.
        let codes: Vec<&str> = caps.regions.iter().map(|r| r.code.as_str()).collect();
        assert!(codes.contains(&"EU-RO-1"));
        assert!(codes.contains(&"EUR-IS-1"));
        // No non-EU RunPod data center leaks into the descriptor.
        assert!(!codes.contains(&"US-CA-1"));
        assert!(!codes.contains(&"AP-JP-1"));
    }

    #[test]
    fn filter_excludes_non_eu_datacenters() {
        // The underlying catalog genuinely contains non-EU data centers, so the
        // EU default is an exclusion, not a hand-picked list.
        assert!(RunPodProvider::datacenter_codes(&Residency::Us).contains(&"US-CA-1"));
        let widened = RunPodProvider::for_residencies(&[Residency::Eu, Residency::Us]);
        let codes: Vec<String> = widened
            .capabilities()
            .regions
            .iter()
            .map(|r| r.code.clone())
            .collect();
        assert!(codes.iter().any(|c| c == "EU-RO-1"));
        assert!(codes.iter().any(|c| c == "US-CA-1"));
    }

    #[test]
    fn eu_regions_helper_matches_admitted_set() {
        let caps = RunPodProvider::new().capabilities();
        let eu_count = caps.eu_regions().count();
        assert_eq!(eu_count, caps.regions.len());
    }

    #[test]
    fn catalog_offers_gpu_with_on_demand_and_spot_pricing() {
        let caps = RunPodProvider::new().capabilities();
        let h100 = caps
            .compute
            .iter()
            .find(|o| o.id == "gpu.h100-80gb.1x")
            .expect("h100 offering present");
        assert!(h100.pricing.has_spot());
        assert_eq!(h100.pricing.on_demand_rate().price.currency, "USD");
        // Every offering is sold in every admitted (EU) region.
        assert_eq!(h100.regions.len(), caps.regions.len());
    }

    #[tokio::test]
    async fn provision_in_eu_region_then_teardown() {
        let mock = Arc::new(
            MockTransport::default()
                .on(HttpMethod::Post, "/pods", created("pod-xyz"))
                .on(
                    HttpMethod::Get,
                    "/pods/pod-xyz",
                    HttpResponse::new(
                        200,
                        br#"{"id":"pod-xyz","desiredStatus":"RUNNING"}"#.to_vec(),
                    ),
                )
                .on(
                    HttpMethod::Delete,
                    "/pods/pod-xyz",
                    HttpResponse::new(200, b"{}".to_vec()),
                ),
        );
        let p = mock_provider(mock.clone());
        let req = ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1");

        let handle = p.provision(&req, &no_creds()).await.unwrap();
        assert!(handle.state.is_ready());
        assert_eq!(handle.id, "pod-xyz");
        assert_eq!(handle.provider, AdapterId::new("runpod"));

        assert!(p.status(&handle, &no_creds()).await.unwrap().is_ready());
        p.teardown(&handle, &no_creds()).await.unwrap();

        // The lifecycle hit the expected REST endpoints in order.
        let seen = mock.requests();
        assert_eq!(seen[0].method, HttpMethod::Post);
        assert!(seen[0].url.ends_with("/pods"));
        assert_eq!(seen[1].method, HttpMethod::Get);
        assert_eq!(seen[2].method, HttpMethod::Delete);
    }

    #[tokio::test]
    async fn provision_in_non_eu_region_is_rejected() {
        // US-CA-1 is a real RunPod data center, but the EU adapter does not admit
        // it: the residency filter restricts pods to compliant regions, before any
        // backend call.
        let p = RunPodProvider::new();
        let err = p
            .provision(
                &ProvisionRequest::new("gpu.a100-80gb.1x", "US-CA-1"),
                &no_creds(),
            )
            .await
            .expect_err("non-EU region must be rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-011");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_unknown_offering_is_rejected() {
        let err = RunPodProvider::new()
            .provision(&ProvisionRequest::new("nope", "EU-RO-1"), &no_creds())
            .await
            .expect_err("unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
    }

    #[tokio::test]
    async fn empty_credential_is_rejected() {
        let p = RunPodProvider::new();
        let resolver = TestResolver::default().with("runpod_token", "");
        let req = ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1")
            .with_credentials(SecretRef::new("runpod_token"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("empty credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_401_to_credentials_rejected() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/pods",
            HttpResponse::new(401, b"unauthorized".to_vec()),
        ));
        let p = mock_provider(mock);
        let req = ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1")
            .with_credentials(SecretRef::new("runpod_token"));
        let resolver = TestResolver::default().with("runpod_token", "bad");
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("401 must reject credentials");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_429_to_capacity_unavailable() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/pods",
            HttpResponse::new(429, b"slow down".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision(
                &ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1"),
                &no_creds(),
            )
            .await
            .expect_err("429 must map to capacity unavailable");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-001");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn status_maps_404_to_unknown_instance() {
        let mock = Arc::new(
            MockTransport::default()
                .on(HttpMethod::Post, "/pods", created("pod-1"))
                .on(
                    HttpMethod::Get,
                    "/pods/pod-1",
                    HttpResponse::new(404, b"not found".to_vec()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p
            .provision(
                &ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1"),
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
    async fn transport_failure_maps_to_retryable_backend() {
        let mock = Arc::new(MockTransport::default().on_error(
            HttpMethod::Post,
            "/pods",
            TransportError::Timeout,
        ));
        let err = mock_provider(mock)
            .provision(
                &ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1"),
                &no_creds(),
            )
            .await
            .expect_err("transport failure surfaces as an error");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_404_to_unknown_offering() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/pods",
            HttpResponse::new(404, b"no such gpu type".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision(
                &ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1"),
                &no_creds(),
            )
            .await
            .expect_err("create 404 maps to unknown offering");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-010");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn provision_maps_500_to_retryable_backend() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/pods",
            HttpResponse::new(500, b"boom".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision(
                &ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1"),
                &no_creds(),
            )
            .await
            .expect_err("5xx maps to backend");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn provision_malformed_success_body_is_backend_error() {
        let mock = Arc::new(MockTransport::default().on(
            HttpMethod::Post,
            "/pods",
            HttpResponse::new(201, b"not json".to_vec()),
        ));
        let err = mock_provider(mock)
            .provision(
                &ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1"),
                &no_creds(),
            )
            .await
            .expect_err("malformed 2xx body maps to backend");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-013");
    }

    #[tokio::test]
    async fn teardown_maps_404_to_unknown_instance() {
        let mock = Arc::new(
            MockTransport::default()
                .on(HttpMethod::Post, "/pods", created("pod-1"))
                .on(
                    HttpMethod::Delete,
                    "/pods/pod-1",
                    HttpResponse::new(404, b"gone".to_vec()),
                ),
        );
        let p = mock_provider(mock);
        let handle = p
            .provision(
                &ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1"),
                &no_creds(),
            )
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
    async fn non_utf8_credential_is_rejected() {
        let p = RunPodProvider::new();
        let resolver = TestResolver::default().with_bytes("runpod_token", vec![0xff, 0xfe]);
        let req = ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1")
            .with_credentials(SecretRef::new("runpod_token"));
        let err = p
            .provision(&req, &resolver)
            .await
            .expect_err("non-UTF-8 credential rejected");
        assert_eq!(err.code().to_string(), "AMICU-PROVIDER-014");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn credentials_never_leak_into_serialized_artifacts() {
        let secret_value = "leak-me-runpod-token";
        let resolver = TestResolver::default().with("runpod_token", secret_value);
        let mock =
            Arc::new(MockTransport::default().on(HttpMethod::Post, "/pods", created("pod-1")));
        let p = mock_provider(mock.clone());
        let req = ProvisionRequest::new("gpu.a100-80gb.1x", "EU-RO-1")
            .with_credentials(SecretRef::new("runpod_token"));

        let req_json = serde_json::to_string(&req).unwrap();
        assert!(req_json.contains("runpod_token"));
        assert!(!req_json.contains(secret_value));

        let handle = p.provision(&req, &resolver).await.unwrap();
        let handle_json = serde_json::to_string(&handle).unwrap();
        assert!(handle_json.contains("runpod_token"));
        assert!(!handle_json.contains(secret_value));

        let describe_json = serde_json::to_string(&p.describe()).unwrap();
        assert!(!describe_json.contains(secret_value));
        // The descriptor names the credential kind only.
        assert!(describe_json.contains("\"credentials\":\"api_token\""));

        // The token was sent as a bearer on the wire but never persisted on the
        // handle or any serialized artifact.
        let seen = mock.requests();
        assert_eq!(seen[0].bearer.as_deref(), Some(secret_value));
    }

    #[tokio::test]
    async fn describe_never_serializes_non_eu_regions() {
        let describe_json = serde_json::to_string(&RunPodProvider::new().describe()).unwrap();
        assert!(describe_json.contains("EU-RO-1"));
        assert!(!describe_json.contains("US-CA-1"));
        assert!(!describe_json.contains("AP-JP-1"));
    }

    #[tokio::test]
    async fn discoverable_as_a_provider_trait_object() {
        let mock =
            Arc::new(MockTransport::default().on(HttpMethod::Post, "/pods", created("pod-1")));
        let mut registry: Registry<dyn Provider> = Registry::new();
        registry.register(Arc::new(mock_provider(mock)));
        let found: Arc<dyn Provider> = registry.get(&AdapterId::new("runpod")).unwrap();
        let handle = found
            .provision(
                &ProvisionRequest::new("gpu.rtx-4090.1x", "EU-NL-1"),
                &no_creds(),
            )
            .await
            .unwrap();
        assert!(handle.state.is_ready());
    }

    #[tokio::test]
    async fn health_override_is_reported_and_in_descriptor() {
        let p = RunPodProvider::new().with_health(HealthStatus::Degraded {
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
