//! Typed client over the RunPod v2 REST API
//! (<https://api.runpod.io/v2/openapi.json>), on the [`HttpTransport`] seam.
//! The API key and secret values are never `Debug`-printed or put in an error.

mod pods;

pub use pods::{
    Cloud, CreatePod, Pod, PodAction, PodEnv, PodGpu, PodStatus, UpdatePod, pod_proxy_url,
};

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use amicu_providers::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, ReqwestTransport, StreamLimits,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::error::RunPodError;

pub const BASE_URL: &str = "https://api.runpod.io/v2";

/// Upper bound on list pages followed, so a misbehaving cursor cannot loop
/// forever (each page holds up to 1000 items).
const MAX_PAGES: usize = 100;

/// Longest API-supplied `detail` kept in an error.
const MAX_DETAIL: usize = 300;

/// A RunPod API key. `Debug` never prints it; there is no `Display`.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlashBoot {
    /// Disabled (the API default).
    #[default]
    Off,
    Flashboot,
    /// Enabled with priority capacity.
    PriorityFlashboot,
}

impl FlashBoot {
    fn wire(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Flashboot => "FLASHBOOT",
            Self::PriorityFlashboot => "PRIORITY_FLASHBOOT",
        }
    }
}

/// GPU request for an endpoint: serverless pool ids (see
/// [`crate::hardware`]), GPUs per worker, and the GPU types excluded from those
/// pools. A pool can hold several cards, so [`crate::hardware::gpu_placement`]
/// fills `excluded_types` to leave only the requested card eligible.
///
/// On update, supplying `pools` replaces the pool selection **and clears
/// `excluded_types`** unless restated (RunPod's documented behavior).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuRequest {
    pub pools: Vec<String>,
    pub count: u32,
    /// GPU type ids subtracted from the pools.
    pub excluded_types: Vec<String>,
    /// Lowest host CUDA version (`major.minor`) a worker may be placed on.
    pub min_cuda_version: Option<String>,
}

impl GpuRequest {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("pools".into(), json!(self.pools));
        m.insert("count".into(), json!(self.count));
        if !self.excluded_types.is_empty() {
            m.insert("excludedTypes".into(), json!(self.excluded_types));
        }
        if let Some(v) = &self.min_cuda_version {
            m.insert("minCudaVersion".into(), json!(v));
        }
        Value::Object(m)
    }
}

impl From<crate::hardware::GpuPlacement> for GpuRequest {
    fn from(p: crate::hardware::GpuPlacement) -> Self {
        Self {
            pools: p.pools,
            count: p.count,
            excluded_types: p.excluded_types,
            min_cuda_version: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkersConfig {
    /// Minimum workers (`0` scales to zero).
    pub min: u32,
    pub max: u32,
    /// Seconds before an idle worker scales down (1..=3600).
    pub idle_timeout: Option<u32>,
}

impl WorkersConfig {
    fn to_json(self) -> Value {
        let mut m = Map::new();
        m.insert("min".into(), json!(self.min));
        m.insert("max".into(), json!(self.max));
        if let Some(t) = self.idle_timeout {
            m.insert("idleTimeout".into(), json!(t));
        }
        Value::Object(m)
    }
}

/// Body of `POST /serverless`. Always a `LOAD_BALANCER` endpoint scaled on
/// concurrent requests (the only scaler the API allows for that type).
///
/// `Debug` prints environment variable names only, never their values.
#[derive(Clone)]
pub struct CreateEndpoint {
    /// Endpoint name (the identity amicu looks it up by).
    pub name: String,
    pub image: String,
    /// Container ENTRYPOINT in exec form; omitted when empty.
    pub entrypoint: Vec<String>,
    /// Container CMD in exec form; omitted when empty.
    pub cmd: Vec<String>,
    /// Environment variables. Values may reference account secrets as
    /// `{{ RUNPOD_SECRET_<name> }}`.
    pub env: BTreeMap<String, String>,
    /// Exposed ports as `port/protocol`, e.g. `8000/http`.
    pub ports: Vec<String>,
    pub disk_gb: Option<u32>,
    pub gpu: GpuRequest,
    pub workers: WorkersConfig,
    /// In-flight requests per worker that triggers scale-up (`>= 1`).
    pub request_count: u32,
    /// Placement data centers; empty lets RunPod choose.
    pub data_center_ids: Vec<String>,
    pub flashboot: FlashBoot,
    /// Network volumes to mount (at `/runpod-volume`).
    pub network_volume_ids: Vec<String>,
    pub timeout_ms: Option<u32>,
    pub registry_id: Option<String>,
}

impl CreateEndpoint {
    /// A minimal request: one worker bound, no env, ports, volumes, or
    /// placement, FlashBoot off. Fill in the public fields as needed.
    #[must_use]
    pub fn new(name: impl Into<String>, image: impl Into<String>, gpu: GpuRequest) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            entrypoint: Vec::new(),
            cmd: Vec::new(),
            env: BTreeMap::new(),
            ports: Vec::new(),
            disk_gb: None,
            gpu,
            workers: WorkersConfig {
                min: 0,
                max: 1,
                idle_timeout: None,
            },
            request_count: 1,
            data_center_ids: Vec::new(),
            flashboot: FlashBoot::Off,
            network_volume_ids: Vec::new(),
            timeout_ms: None,
            registry_id: None,
        }
    }

    #[must_use]
    pub fn to_body(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("type".into(), json!("LOAD_BALANCER"));
        m.insert("image".into(), json!(self.image));
        insert_container(
            &mut m,
            non_empty(&self.entrypoint),
            non_empty(&self.cmd),
            (!self.env.is_empty()).then_some(&self.env),
            non_empty(&self.ports),
            self.disk_gb,
            self.registry_id.as_deref(),
        );
        m.insert("gpu".into(), self.gpu.to_json());
        m.insert("workers".into(), self.workers.to_json());
        m.insert(
            "scaling".into(),
            json!({"type": "REQUEST_COUNT", "requestCount": self.request_count}),
        );
        if !self.data_center_ids.is_empty() {
            m.insert("dataCenterIds".into(), json!(self.data_center_ids));
        }
        m.insert("flashboot".into(), json!(self.flashboot.wire()));
        if !self.network_volume_ids.is_empty() {
            m.insert("mounts".into(), mounts_json(&self.network_volume_ids));
        }
        if let Some(t) = self.timeout_ms {
            m.insert("timeout".into(), json!(t));
        }
        Value::Object(m)
    }
}

impl fmt::Debug for CreateEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateEndpoint")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("gpu", &self.gpu)
            .field("workers", &self.workers)
            .field("data_center_ids", &self.data_center_ids)
            .finish_non_exhaustive()
    }
}

/// Body of `PATCH /serverless/{id}`: only `Some` fields are sent and changed.
/// `Debug` prints environment variable names only.
#[derive(Clone, Default)]
pub struct UpdateEndpoint {
    pub name: Option<String>,
    pub image: Option<String>,
    /// Replacement ENTRYPOINT (`Some(vec![])` clears it).
    pub entrypoint: Option<Vec<String>>,
    /// Replacement CMD (`Some(vec![])` clears it).
    pub cmd: Option<Vec<String>>,
    pub env: Option<BTreeMap<String, String>>,
    pub ports: Option<Vec<String>>,
    pub disk_gb: Option<u32>,
    pub gpu: Option<GpuRequest>,
    pub workers: Option<WorkersConfig>,
    pub request_count: Option<u32>,
    /// Replacement data centers (`Some(vec![])` lets RunPod choose).
    pub data_center_ids: Option<Vec<String>>,
    pub flashboot: Option<FlashBoot>,
    /// Replacement network volumes (`Some(vec![])` detaches).
    pub network_volume_ids: Option<Vec<String>>,
    pub timeout_ms: Option<u32>,
    pub registry_id: Option<String>,
}

impl UpdateEndpoint {
    #[must_use]
    pub fn to_body(&self) -> Value {
        let mut m = Map::new();
        if let Some(v) = &self.name {
            m.insert("name".into(), json!(v));
        }
        if let Some(v) = &self.image {
            m.insert("image".into(), json!(v));
        }
        insert_container(
            &mut m,
            self.entrypoint.as_deref(),
            self.cmd.as_deref(),
            self.env.as_ref(),
            self.ports.as_deref(),
            self.disk_gb,
            self.registry_id.as_deref(),
        );
        if let Some(g) = &self.gpu {
            m.insert("gpu".into(), g.to_json());
        }
        if let Some(w) = self.workers {
            m.insert("workers".into(), w.to_json());
        }
        if let Some(n) = self.request_count {
            m.insert(
                "scaling".into(),
                json!({"type": "REQUEST_COUNT", "requestCount": n}),
            );
        }
        if let Some(v) = &self.data_center_ids {
            m.insert("dataCenterIds".into(), json!(v));
        }
        if let Some(v) = self.flashboot {
            m.insert("flashboot".into(), json!(v.wire()));
        }
        if let Some(v) = &self.network_volume_ids {
            m.insert("mounts".into(), mounts_json(v));
        }
        if let Some(t) = self.timeout_ms {
            m.insert("timeout".into(), json!(t));
        }
        Value::Object(m)
    }
}

impl fmt::Debug for UpdateEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpdateEndpoint")
            .field("name", &self.name)
            .field("image", &self.image)
            .field(
                "env_keys",
                &self.env.as_ref().map(|e| e.keys().collect::<Vec<_>>()),
            )
            .finish_non_exhaustive()
    }
}

/// `None` for an empty list: create never sends empty lists (it keeps the body
/// minimal), whereas update sends `Some(vec![])` to clear a field.
fn non_empty(v: &[String]) -> Option<&[String]> {
    (!v.is_empty()).then_some(v)
}

/// Shared container settings, inserted only when `Some`.
fn insert_container(
    m: &mut Map<String, Value>,
    entrypoint: Option<&[String]>,
    cmd: Option<&[String]>,
    env: Option<&BTreeMap<String, String>>,
    ports: Option<&[String]>,
    disk_gb: Option<u32>,
    registry_id: Option<&str>,
) {
    if let Some(v) = entrypoint {
        m.insert("entrypoint".into(), json!(v));
    }
    if let Some(v) = cmd {
        m.insert("cmd".into(), json!(v));
    }
    if let Some(v) = env {
        m.insert("env".into(), json!(v));
    }
    if let Some(v) = ports {
        m.insert("ports".into(), json!(v));
    }
    if let Some(d) = disk_gb {
        m.insert("disk".into(), json!(d));
    }
    if let Some(r) = registry_id {
        m.insert("registry".into(), json!(r));
    }
}

fn mounts_json(volume_ids: &[String]) -> Value {
    let network: Vec<Value> = volume_ids.iter().map(|v| json!({"volumeId": v})).collect();
    json!({ "network": network })
}

/// Body of `POST /account/secrets`. `Debug` redacts the value.
pub struct CreateSecret {
    /// Unique, immutable name; referenced as `{{ RUNPOD_SECRET_<name> }}`.
    pub name: String,
    /// The secret value (write-only on RunPod's side).
    pub value: String,
    pub description: Option<String>,
}

impl CreateSecret {
    fn to_body(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("value".into(), json!(self.value));
        if let Some(d) = &self.description {
            m.insert("description".into(), json!(d));
        }
        Value::Object(m)
    }
}

impl fmt::Debug for CreateSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateSecret")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Whether a delete removed something or it was already gone (`404`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// RunPod deleted it (`2xx`).
    Deleted,
    /// RunPod reported `404`: nothing to delete. Treated as success by
    /// idempotent teardown, but kept distinct so callers can report it.
    AlreadyGone,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EndpointGpu {
    #[serde(default)]
    pub pools: Vec<String>,
    #[serde(default)]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EndpointWorkers {
    #[serde(default)]
    pub min: u32,
    #[serde(default)]
    pub max: u32,
    /// Idle scale-down delay in seconds.
    #[serde(default)]
    pub idle_timeout: Option<u32>,
}

/// Request URLs of a returned endpoint (load-balancing endpoints carry
/// `base` and `health`).
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RequestUrls {
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub health: Option<String>,
}

/// The plain endpoint env variable amicu sets on every endpoint it creates:
/// an owner marker (a digest of the workspace / project / deployment), checked
/// before an endpoint found only by name is adopted.
pub const OWNER_ENV_VAR: &str = "AMICU_OWNER";

/// A serverless endpoint as RunPod returns it. Environment values are
/// deliberately not modelled, so they cannot be logged from here; only
/// amicu's own [`OWNER_ENV_VAR`] marker (not a secret) is read.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    /// `QUEUE` or `LOAD_BALANCER`.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    /// GPU settings (absent for CPU endpoints).
    #[serde(default)]
    pub gpu: Option<EndpointGpu>,
    #[serde(default)]
    pub workers: Option<EndpointWorkers>,
    #[serde(default)]
    pub data_center_ids: Vec<String>,
    #[serde(default)]
    pub request_urls: Option<RequestUrls>,
    /// FlashBoot mode as RunPod spells it.
    #[serde(default)]
    pub flashboot: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// The [`OWNER_ENV_VAR`] value in the endpoint env, when RunPod returns
    /// the env and amicu set the marker.
    #[serde(default, rename = "env", deserialize_with = "owner_from_env")]
    pub owner: Option<String>,
}

/// Reads only [`OWNER_ENV_VAR`] out of an endpoint's `env` map; every other
/// value is dropped unseen.
fn owner_from_env<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let env: Option<BTreeMap<String, Value>> = Option::deserialize(d)?;
    Ok(env
        .and_then(|mut m| m.remove(OWNER_ENV_VAR))
        .and_then(|v| v.as_str().map(str::to_owned)))
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Worker {
    pub id: String,
    /// `RUNNING`, `IDLE`, `INITIALIZING`, `THROTTLED` or `UNHEALTHY`.
    pub status: String,
    /// Whether it runs an older endpoint configuration.
    #[serde(default)]
    pub is_stale: bool,
    #[serde(default)]
    pub gpu_count: u32,
    #[serde(default)]
    pub gpu_type_id: Option<String>,
    #[serde(default)]
    pub data_center_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct WorkerSummary {
    #[serde(default)]
    pub running: u32,
    /// Ready, polling for jobs.
    #[serde(default)]
    pub idle: u32,
    #[serde(default)]
    pub initializing: u32,
    /// Waiting on capacity.
    #[serde(default)]
    pub throttled: u32,
    /// Failing health checks.
    #[serde(default)]
    pub unhealthy: u32,
    #[serde(default)]
    pub total: u32,
}

/// Response of `GET /serverless/{id}/workers`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkersList {
    pub workers: Vec<Worker>,
    pub summary: WorkerSummary,
    /// The endpoint's current configuration version.
    #[serde(default)]
    pub endpoint_version: Option<u32>,
}

/// The account's organization: its identity.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Organization {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Secret {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Deserialize)]
struct Pagination {
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
    #[serde(rename = "hasNextPage", default)]
    has_next_page: bool,
}

#[derive(Deserialize)]
struct EndpointsPage {
    endpoints: Vec<Endpoint>,
    pagination: Pagination,
}

#[derive(Deserialize)]
struct SecretsList {
    secrets: Vec<Secret>,
}

#[derive(Deserialize)]
struct GpuCatalog {
    gpus: Vec<GpuType>,
}

/// One entry of `GET /catalog/gpus`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GpuType {
    /// GPU type id, as used in `excludedTypes` (`"NVIDIA L40S"`).
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Serverless pool id (`"ADA_48_PRO"`); absent for types with no pool.
    #[serde(default)]
    pub pool: Option<String>,
    #[serde(default)]
    pub memory: Option<u32>,
}

/// Name of the amicu-owned secret whose id identifies a personal account
/// (see [`RunPodClient::account_identity`]).
pub const ACCOUNT_MARKER_SECRET: &str = "amicu-account-marker";

/// Which RunPod account a set of credentials acts in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountIdentity {
    /// An organization account, by organization id.
    Organization { id: String },
    /// A personal account (organizations not enabled), by the id of its
    /// [`ACCOUNT_MARKER_SECRET`].
    Personal { marker_secret_id: String },
}

impl AccountIdentity {
    /// A stable string to record in state and compare later:
    /// `org:<id>` or `personal:<marker secret id>`.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        match self {
            Self::Organization { id } => format!("org:{id}"),
            Self::Personal { marker_secret_id } => format!("personal:{marker_secret_id}"),
        }
    }
}

pub struct RunPodClient {
    transport: Arc<dyn HttpTransport>,
    base_url: String,
    key: ApiKey,
}

impl fmt::Debug for RunPodClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunPodClient")
            .field("base_url", &self.base_url)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl RunPodClient {
    /// A client for the production API using the `reqwest` transport.
    #[must_use]
    pub fn new(key: ApiKey) -> Self {
        Self::with_transport(key, Arc::new(ReqwestTransport::new()), BASE_URL)
    }

    /// A client over an explicit transport and base URL (tests inject a mock).
    #[must_use]
    pub fn with_transport(
        key: ApiKey,
        transport: Arc<dyn HttpTransport>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            key,
        }
    }

    /// `POST /serverless`: create a load-balancing endpoint.
    ///
    /// A transport failure here is [`RunPodError::AmbiguousCreate`]: the
    /// request may have reached RunPod and created the endpoint, and names are
    /// not unique, so a blind retry could create a duplicate. Callers must
    /// [`find_endpoints_by_name`](Self::find_endpoints_by_name) before retrying.
    /// Error details are withheld when the request carries env values.
    pub async fn create_endpoint(&self, req: &CreateEndpoint) -> Result<Endpoint, RunPodError> {
        let resp = self
            .send_inner(
                HttpMethod::Post,
                "/serverless",
                Some(req.to_body()),
                "serverless endpoint",
                !req.env.is_empty(),
            )
            .await
            .map_err(|e| match e {
                RunPodError::Transport(m) => RunPodError::AmbiguousCreate(m),
                other => other,
            })?;
        decode(&resp)
    }

    pub async fn get_endpoint(&self, id: &str) -> Result<Endpoint, RunPodError> {
        let path = format!("/serverless/{}", segment(id)?);
        let resp = self
            .send(HttpMethod::Get, &path, None, &format!("endpoint `{id}`"))
            .await?;
        decode(&resp)
    }

    /// `GET /serverless`, following pagination to the end (newest first).
    pub async fn list_endpoints(&self) -> Result<Vec<Endpoint>, RunPodError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let path = match &cursor {
                Some(c) => format!("/serverless?cursor={}", encode_query(c)),
                None => "/serverless".to_owned(),
            };
            let resp = self
                .send(HttpMethod::Get, &path, None, "serverless endpoints")
                .await?;
            let page: EndpointsPage = decode(&resp)?;
            all.extend(page.endpoints);
            match page.pagination.next_cursor {
                Some(next) if page.pagination.has_next_page => cursor = Some(next),
                _ => return Ok(all),
            }
        }
        Err(RunPodError::Decode(format!(
            "endpoint list still paginating after {MAX_PAGES} pages"
        )))
    }

    /// `GET /serverless?limit=1`: proves the key is accepted. Not
    /// `/organization`, which answers `403` for every personal account.
    pub async fn verify_key(&self) -> Result<(), RunPodError> {
        self.send(
            HttpMethod::Get,
            "/serverless?limit=1",
            None,
            "serverless endpoints",
        )
        .await
        .map(|_| ())
    }

    /// Every endpoint whose name is exactly `name` (RunPod does not enforce
    /// unique names), newest first.
    pub async fn find_endpoints_by_name(&self, name: &str) -> Result<Vec<Endpoint>, RunPodError> {
        Ok(self
            .list_endpoints()
            .await?
            .into_iter()
            .filter(|e| e.name == name)
            .collect())
    }

    /// The one endpoint named exactly `name`, or `None`. More than one match is
    /// [`RunPodError::DuplicateName`] rather than a silent pick.
    pub async fn find_endpoint_by_name(&self, name: &str) -> Result<Option<Endpoint>, RunPodError> {
        let mut found = self.find_endpoints_by_name(name).await?;
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            count => Err(RunPodError::DuplicateName {
                name: name.to_owned(),
                count,
            }),
        }
    }

    /// `PATCH /serverless/{id}`: apply a partial update. Error details are
    /// withheld when the request carries env values.
    pub async fn update_endpoint(
        &self,
        id: &str,
        update: &UpdateEndpoint,
    ) -> Result<Endpoint, RunPodError> {
        let path = format!("/serverless/{}", segment(id)?);
        let resp = self
            .send_inner(
                HttpMethod::Patch,
                &path,
                Some(update.to_body()),
                &format!("endpoint `{id}`"),
                update.env.as_ref().is_some_and(|e| !e.is_empty()),
            )
            .await?;
        decode(&resp)
    }

    /// `DELETE /serverless/{id}`. `404` is [`DeleteOutcome::AlreadyGone`], not
    /// an error.
    pub async fn delete_endpoint(&self, id: &str) -> Result<DeleteOutcome, RunPodError> {
        let path = format!("/serverless/{}", segment(id)?);
        self.delete(&path, &format!("endpoint `{id}`")).await
    }

    pub async fn list_workers(&self, id: &str) -> Result<WorkersList, RunPodError> {
        let path = format!("/serverless/{}/workers", segment(id)?);
        let resp = self
            .send(HttpMethod::Get, &path, None, &format!("endpoint `{id}`"))
            .await?;
        decode(&resp)
    }

    /// A bounded `GET` of an event stream (see
    /// [`HttpTransport::execute_bounded`]), checked like any other call.
    pub(crate) async fn get_stream(
        &self,
        path: &str,
        what: &str,
        limits: StreamLimits,
    ) -> Result<HttpResponse, RunPodError> {
        let req = HttpRequest::new(HttpMethod::Get, format!("{}{path}", self.base_url))
            .with_bearer(self.key.0.clone())
            .with_header("Accept", "text/event-stream");
        let resp = self
            .transport
            .execute_bounded(req, limits)
            .await
            .map_err(|e| RunPodError::Transport(e.to_string()))?;
        check(&resp, what, false)?;
        Ok(resp)
    }

    /// `GET /organization`: the account identity.
    ///
    /// A personal account answers `403` "organizations are not enabled for
    /// this user"; that is [`RunPodError::OrganizationsDisabled`], not a
    /// rejected key (see [`account_identity`](Self::account_identity)).
    pub async fn get_organization(&self) -> Result<Organization, RunPodError> {
        let resp = self.execute(HttpMethod::Get, "/organization", None).await?;
        if resp.status == 403
            && problem_detail(&resp.body)
                .to_ascii_lowercase()
                .contains("organizations are not enabled")
        {
            return Err(RunPodError::OrganizationsDisabled);
        }
        check(&resp, "organization", false)?;
        decode(&resp)
    }

    /// The account these credentials act in, for "confirmed gone only in the
    /// same workspace" checks.
    ///
    /// An organization account is identified by its organization id. The v2
    /// API has no user or account endpoint for a personal account (organization
    /// `403`), so its identity is the id of an amicu-owned marker secret,
    /// [`ACCOUNT_MARKER_SECRET`]: secrets are account-scoped and ids are
    /// stable, so the same account always re-derives the same id, and another
    /// account never sees it. With `create_marker` the marker is created when
    /// missing (deploy time); without it a missing marker is `Ok(None)`
    /// (identity unknown: never treat that as "same account").
    pub async fn account_identity(
        &self,
        create_marker: bool,
    ) -> Result<Option<AccountIdentity>, RunPodError> {
        match self.get_organization().await {
            Ok(org) => return Ok(Some(AccountIdentity::Organization { id: org.id })),
            Err(RunPodError::OrganizationsDisabled) => {}
            Err(e) => return Err(e),
        }
        if let Some(marker) = self.find_secret(ACCOUNT_MARKER_SECRET).await? {
            return Ok(Some(AccountIdentity::Personal {
                marker_secret_id: marker.id,
            }));
        }
        if !create_marker {
            return Ok(None);
        }
        let marker = CreateSecret {
            name: ACCOUNT_MARKER_SECRET.to_owned(),
            value: "amicu account marker (not a credential)".to_owned(),
            description: Some(
                "amicu: identifies this RunPod account; do not delete while amicu manages \
                 deployments here"
                    .to_owned(),
            ),
        };
        let id = match self.create_secret(&marker).await {
            Ok(s) => s.id,
            // Created concurrently, or the response was lost: read it back.
            Err(RunPodError::Conflict { .. } | RunPodError::Transport(_)) => {
                self.find_secret(ACCOUNT_MARKER_SECRET)
                    .await?
                    .ok_or_else(|| RunPodError::NotFound {
                        what: format!("secret `{ACCOUNT_MARKER_SECRET}`"),
                    })?
                    .id
            }
            Err(e) => return Err(e),
        };
        Ok(Some(AccountIdentity::Personal {
            marker_secret_id: id,
        }))
    }

    /// `GET /catalog/gpus`: every GPU type with its serverless pool.
    pub async fn get_gpu_catalog(&self) -> Result<Vec<GpuType>, RunPodError> {
        let resp = self
            .send(HttpMethod::Get, "/catalog/gpus", None, "GPU catalog")
            .await?;
        let list: GpuCatalog = decode(&resp)?;
        Ok(list.gpus)
    }

    /// `POST /account/secrets`. Error details are withheld for this call.
    pub async fn create_secret(&self, req: &CreateSecret) -> Result<Secret, RunPodError> {
        let resp = self
            .send_inner(
                HttpMethod::Post,
                "/account/secrets",
                Some(req.to_body()),
                "secret",
                true,
            )
            .await?;
        decode(&resp)
    }

    /// `GET /account/secrets?name=...`: the secret named `name` (RunPod
    /// matches case-insensitively), or `None`. Needed to find the id for
    /// [`delete_secret`](Self::delete_secret).
    pub async fn find_secret(&self, name: &str) -> Result<Option<Secret>, RunPodError> {
        let path = format!("/account/secrets?name={}", encode_query(name));
        let resp = self.send(HttpMethod::Get, &path, None, "secrets").await?;
        let list: SecretsList = decode(&resp)?;
        Ok(list
            .secrets
            .into_iter()
            .find(|s| s.name.eq_ignore_ascii_case(name)))
    }

    /// `GET /account/secrets`: every secret of the account (no values). The
    /// API defines no pagination for this list.
    pub async fn list_secrets(&self) -> Result<Vec<Secret>, RunPodError> {
        let resp = self
            .send(HttpMethod::Get, "/account/secrets", None, "secrets")
            .await?;
        let list: SecretsList = decode(&resp)?;
        Ok(list.secrets)
    }

    /// `DELETE /account/secrets/{id}`. `404` is [`DeleteOutcome::AlreadyGone`].
    pub async fn delete_secret(&self, id: &str) -> Result<DeleteOutcome, RunPodError> {
        let path = format!("/account/secrets/{}", segment(id)?);
        self.delete(&path, &format!("secret `{id}`")).await
    }

    async fn delete(&self, path: &str, what: &str) -> Result<DeleteOutcome, RunPodError> {
        let resp = self.execute(HttpMethod::Delete, path, None).await?;
        if resp.status == 404 {
            return Ok(DeleteOutcome::AlreadyGone);
        }
        check(&resp, what, false)?;
        Ok(DeleteOutcome::Deleted)
    }

    async fn send(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<Value>,
        what: &str,
    ) -> Result<HttpResponse, RunPodError> {
        self.send_inner(method, path, body, what, false).await
    }

    async fn send_inner(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<Value>,
        what: &str,
        redact: bool,
    ) -> Result<HttpResponse, RunPodError> {
        let resp = self.execute(method, path, body).await?;
        check(&resp, what, redact)?;
        Ok(resp)
    }

    async fn execute(
        &self,
        method: HttpMethod,
        path: &str,
        body: Option<Value>,
    ) -> Result<HttpResponse, RunPodError> {
        let mut req = HttpRequest::new(method, format!("{}{path}", self.base_url))
            .with_bearer(self.key.0.clone());
        if let Some(body) = body {
            req = req.with_json(body);
        }
        self.transport
            .execute(req)
            .await
            .map_err(|e| RunPodError::Transport(e.to_string()))
    }
}

/// An id usable as one path segment: non-empty, no separators or anything
/// that could alter the route.
pub(crate) fn segment(id: &str) -> Result<&str, RunPodError> {
    if !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Ok(id)
    } else {
        Err(RunPodError::InvalidId(id.chars().take(40).collect()))
    }
}

/// Percent-encode a query value (RFC 3986 unreserved characters pass through).
fn encode_query(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn decode<T: DeserializeOwned>(resp: &HttpResponse) -> Result<T, RunPodError> {
    resp.json().map_err(|e| RunPodError::Decode(e.to_string()))
}

/// Map a non-`2xx` response onto a typed error. `redact` drops the
/// API-supplied detail (used where a body could echo a secret).
fn check(resp: &HttpResponse, what: &str, redact: bool) -> Result<(), RunPodError> {
    if resp.is_success() {
        return Ok(());
    }
    let detail = if redact {
        "details withheld".to_owned()
    } else {
        problem_detail(&resp.body)
    };
    Err(match resp.status {
        401 | 403 => RunPodError::CredentialsRejected {
            status: resp.status,
        },
        404 => RunPodError::NotFound {
            what: what.to_owned(),
        },
        409 => RunPodError::Conflict { detail },
        429 => RunPodError::RateLimited,
        400 | 422 => RunPodError::BadRequest {
            status: resp.status,
            detail,
        },
        status => RunPodError::Api { status, detail },
    })
}

/// `detail` (plus validation `errors`) from an `application/problem+json`
/// body, truncated; empty when the body is not that shape.
fn problem_detail(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return String::new();
    };
    let mut text = v
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Some(errors) = v.get("errors").and_then(Value::as_array) {
        let joined: Vec<&str> = errors.iter().filter_map(Value::as_str).collect();
        if !joined.is_empty() {
            if !text.is_empty() {
                text.push_str(": ");
            }
            text.push_str(&joined.join("; "));
        }
    }
    text.chars().take(MAX_DETAIL).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use amicu_providers::TransportError;
    use async_trait::async_trait;

    type Route = (HttpMethod, String, Result<HttpResponse, TransportError>);

    #[derive(Clone)]
    pub(super) struct Seen {
        pub(super) method: HttpMethod,
        pub(super) url: String,
        pub(super) bearer: Option<String>,
        pub(super) body: Option<Value>,
    }

    /// Canned responses; the first route matching `(method, url-suffix)` is
    /// used. Records every request.
    #[derive(Default)]
    pub(super) struct Mock {
        routes: Mutex<Vec<Route>>,
        seen: Mutex<Vec<Seen>>,
    }

    impl Mock {
        pub(super) fn on(self, method: HttpMethod, suffix: &str, status: u16, body: &str) -> Self {
            self.routes.lock().unwrap().push((
                method,
                suffix.into(),
                Ok(HttpResponse::new(status, body.as_bytes().to_vec())),
            ));
            self
        }

        pub(super) fn fail(self, method: HttpMethod, suffix: &str, e: TransportError) -> Self {
            self.routes
                .lock()
                .unwrap()
                .push((method, suffix.into(), Err(e)));
            self
        }

        pub(super) fn requests(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HttpTransport for Mock {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.seen.lock().unwrap().push(Seen {
                method: request.method,
                url: request.url.clone(),
                bearer: request.bearer.clone(),
                body: request.body.clone(),
            });
            let routes = self.routes.lock().unwrap();
            let hit = routes
                .iter()
                .find(|(m, s, _)| *m == request.method && request.url.ends_with(s.as_str()));
            match hit {
                Some((_, _, Ok(r))) => Ok(r.clone()),
                Some((_, _, Err(TransportError::Timeout))) => Err(TransportError::Timeout),
                Some((_, _, Err(TransportError::Connection(m)))) => {
                    Err(TransportError::Connection(m.clone()))
                }
                None => Ok(HttpResponse::new(500, b"unrouted".to_vec())),
            }
        }
    }

    pub(super) const KEY: &str = "rpa_SUPERSECRETKEY";
    const BASE: &str = "https://api.runpod.io/v2";

    pub(super) fn client(mock: Mock) -> (RunPodClient, Arc<Mock>) {
        let mock = Arc::new(mock);
        let c = RunPodClient::with_transport(ApiKey::new(KEY), mock.clone(), BASE);
        (c, mock)
    }

    const ENDPOINT: &str = r#"{"id":"ep1","name":"amicu-x","type":"LOAD_BALANCER",
        "image":"img:1","gpu":{"pools":["ADA_24"],"count":1},
        "workers":{"min":0,"max":2,"idleTimeout":5},"dataCenterIds":["EU-RO-1"],
        "requestUrls":{"base":"https://ep1.api.runpod.ai","health":"https://ep1.api.runpod.ai/ping"},
        "env":{"TOKEN":"leaky"},"createdAt":"2026-06-01T12:00:00Z"}"#;

    fn full_request() -> CreateEndpoint {
        let mut r = CreateEndpoint::new(
            "amicu-x",
            "ghcr.io/amicu/vllm:1",
            GpuRequest {
                pools: vec!["ADA_80_PRO".into()],
                count: 2,
                excluded_types: vec![],
                min_cuda_version: Some("12.8".into()),
            },
        );
        r.entrypoint = vec!["python".into()];
        r.cmd = vec!["-m".into(), "server".into()];
        r.env = BTreeMap::from([
            ("PORT".to_owned(), "8000".to_owned()),
            (
                "HF_TOKEN".to_owned(),
                "{{ RUNPOD_SECRET_hf-token }}".to_owned(),
            ),
        ]);
        r.ports = vec!["8000/http".into()];
        r.disk_gb = Some(50);
        r.workers = WorkersConfig {
            min: 0,
            max: 3,
            idle_timeout: Some(30),
        };
        r.request_count = 4;
        r.data_center_ids = vec!["EU-RO-1".into(), "EU-SE-1".into()];
        r.flashboot = FlashBoot::Flashboot;
        r.network_volume_ids = vec!["vol1".into()];
        r.timeout_ms = Some(600_000);
        r.registry_id = Some("reg1".into());
        r
    }

    #[tokio::test]
    async fn create_endpoint_sends_exact_body() {
        let (c, mock) = client(Mock::default().on(HttpMethod::Post, "/serverless", 201, ENDPOINT));
        let ep = c.create_endpoint(&full_request()).await.unwrap();
        assert_eq!(ep.id, "ep1");
        assert_eq!(ep.kind.as_deref(), Some("LOAD_BALANCER"));
        assert_eq!(ep.gpu.unwrap().pools, vec!["ADA_24"]);
        assert_eq!(
            ep.request_urls.unwrap().health.as_deref(),
            Some("https://ep1.api.runpod.ai/ping")
        );

        let seen = mock.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, HttpMethod::Post);
        assert_eq!(seen[0].url, "https://api.runpod.io/v2/serverless");
        assert_eq!(seen[0].bearer.as_deref(), Some(KEY));
        assert_eq!(
            seen[0].body.as_ref().unwrap(),
            &json!({
                "name": "amicu-x",
                "type": "LOAD_BALANCER",
                "image": "ghcr.io/amicu/vllm:1",
                "entrypoint": ["python"],
                "cmd": ["-m", "server"],
                "env": {"HF_TOKEN": "{{ RUNPOD_SECRET_hf-token }}", "PORT": "8000"},
                "ports": ["8000/http"],
                "disk": 50,
                "registry": "reg1",
                "gpu": {"pools": ["ADA_80_PRO"], "count": 2, "minCudaVersion": "12.8"},
                "workers": {"min": 0, "max": 3, "idleTimeout": 30},
                "scaling": {"type": "REQUEST_COUNT", "requestCount": 4},
                "dataCenterIds": ["EU-RO-1", "EU-SE-1"],
                "flashboot": "FLASHBOOT",
                "mounts": {"network": [{"volumeId": "vol1"}]},
                "timeout": 600_000
            })
        );
    }

    #[test]
    fn minimal_create_body_omits_empty_fields() {
        let r = CreateEndpoint::new(
            "n",
            "i",
            GpuRequest {
                pools: vec!["ADA_24".into()],
                count: 1,
                excluded_types: vec![],
                min_cuda_version: None,
            },
        );
        assert_eq!(
            r.to_body(),
            json!({
                "name": "n",
                "type": "LOAD_BALANCER",
                "image": "i",
                "gpu": {"pools": ["ADA_24"], "count": 1},
                "workers": {"min": 0, "max": 1},
                "scaling": {"type": "REQUEST_COUNT", "requestCount": 1},
                "flashboot": "OFF"
            })
        );
    }

    #[test]
    fn excluded_types_are_sent_when_present() {
        let g = GpuRequest {
            pools: vec!["ADA_48_PRO".into()],
            count: 1,
            excluded_types: vec!["NVIDIA L40".into()],
            min_cuda_version: None,
        };
        assert_eq!(g.to_json()["excludedTypes"], json!(["NVIDIA L40"]));
    }

    #[tokio::test]
    async fn get_endpoint_hits_id_path() {
        let (c, mock) =
            client(Mock::default().on(HttpMethod::Get, "/serverless/ep1", 200, ENDPOINT));
        let ep = c.get_endpoint("ep1").await.unwrap();
        assert_eq!(ep.name, "amicu-x");
        assert_eq!(ep.workers.unwrap().idle_timeout, Some(5));
        assert_eq!(
            mock.requests()[0].url,
            "https://api.runpod.io/v2/serverless/ep1"
        );
        assert!(mock.requests()[0].body.is_none());
    }

    #[tokio::test]
    async fn get_endpoint_404_is_not_found() {
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/serverless/gone", 404, "{}"));
        assert_eq!(
            c.get_endpoint("gone").await.unwrap_err(),
            RunPodError::NotFound {
                what: "endpoint `gone`".into()
            }
        );
    }

    #[tokio::test]
    async fn list_follows_pagination_and_finds_exact_name() {
        let page1 = r#"{"endpoints":[{"id":"a","name":"amicu-x-2"}],
            "pagination":{"nextCursor":"c u/1","hasNextPage":true}}"#;
        let page2 = r#"{"endpoints":[{"id":"b","name":"amicu-x"}],
            "pagination":{"nextCursor":null,"hasNextPage":false}}"#;
        let mock = Mock::default()
            .on(HttpMethod::Get, "/serverless?cursor=c%20u%2F1", 200, page2)
            .on(HttpMethod::Get, "/serverless", 200, page1);
        let (c, mock) = client(mock);
        let found = c.find_endpoint_by_name("amicu-x").await.unwrap();
        assert_eq!(found.unwrap().id, "b", "exact match, not prefix");
        let urls: Vec<String> = mock.requests().into_iter().map(|r| r.url).collect();
        assert_eq!(
            urls,
            vec![
                "https://api.runpod.io/v2/serverless".to_owned(),
                "https://api.runpod.io/v2/serverless?cursor=c%20u%2F1".to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn find_by_name_none_when_absent() {
        let body = r#"{"endpoints":[],"pagination":{"nextCursor":null,"hasNextPage":false}}"#;
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/serverless", 200, body));
        assert!(c.find_endpoint_by_name("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn create_transport_failure_is_ambiguous_and_not_retryable() {
        let (c, _) =
            client(Mock::default().fail(HttpMethod::Post, "/serverless", TransportError::Timeout));
        let err = c.create_endpoint(&full_request()).await.unwrap_err();
        assert!(matches!(err, RunPodError::AmbiguousCreate(_)), "{err:?}");
        assert!(!err.is_retryable());
    }

    #[tokio::test]
    async fn duplicate_names_are_listed_and_the_single_lookup_errors() {
        let body = r#"{"endpoints":[{"id":"a","name":"dup"},{"id":"b","name":"dup"},
            {"id":"c","name":"other"}],
            "pagination":{"nextCursor":null,"hasNextPage":false}}"#;
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/serverless", 200, body));
        let all = c.find_endpoints_by_name("dup").await.unwrap();
        assert_eq!(
            all.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            c.find_endpoint_by_name("dup").await.unwrap_err(),
            RunPodError::DuplicateName {
                name: "dup".into(),
                count: 2
            }
        );
        assert_eq!(
            c.find_endpoint_by_name("other").await.unwrap().unwrap().id,
            "c"
        );
    }

    #[tokio::test]
    async fn endless_pagination_is_an_error_not_a_partial_list() {
        let page = r#"{"endpoints":[],"pagination":{"nextCursor":"x","hasNextPage":true}}"#;
        let (c, mock) = client(
            Mock::default()
                .on(HttpMethod::Get, "/serverless?cursor=x", 200, page)
                .on(HttpMethod::Get, "/serverless", 200, page),
        );
        assert!(matches!(
            c.list_endpoints().await.unwrap_err(),
            RunPodError::Decode(_)
        ));
        assert_eq!(mock.requests().len(), MAX_PAGES);
    }

    #[tokio::test]
    async fn api_detail_is_withheld_when_env_is_sent() {
        let body = r#"{"detail":"env HF_TOKEN=env-secret-value is invalid"}"#;
        let mut req = full_request();
        req.env.insert("HF_TOKEN".into(), "env-secret-value".into());
        let (c, _) = client(Mock::default().on(HttpMethod::Post, "/serverless", 400, body));
        let err = c.create_endpoint(&req).await.unwrap_err();
        assert!(!format!("{err}{err:?}").contains("env-secret-value"));

        let (c, _) = client(Mock::default().on(HttpMethod::Patch, "/serverless/ep1", 400, body));
        let update = UpdateEndpoint {
            env: Some(BTreeMap::from([(
                "K".to_owned(),
                "env-secret-value".to_owned(),
            )])),
            ..UpdateEndpoint::default()
        };
        let err = c.update_endpoint("ep1", &update).await.unwrap_err();
        assert!(!format!("{err}{err:?}").contains("env-secret-value"));

        // Without env the detail is kept for diagnosis.
        let (c, _) = client(Mock::default().on(HttpMethod::Patch, "/serverless/ep1", 400, body));
        let err = c
            .update_endpoint("ep1", &UpdateEndpoint::default())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("is invalid"));
    }

    #[test]
    fn gpu_placement_exclusions_reach_the_request_body() {
        let g: GpuRequest = crate::hardware::parse_gpu_spec("l40s").unwrap().into();
        assert_eq!(
            g.to_json(),
            json!({
                "pools": ["ADA_48_PRO"],
                "count": 1,
                "excludedTypes": [
                    "NVIDIA L40",
                    "NVIDIA RTX 6000 Ada Generation",
                    "NVIDIA RTX PRO 6000 Blackwell Server Edition MIG 2g.48gb"
                ]
            })
        );
    }

    #[tokio::test]
    async fn update_is_a_patch_with_only_set_fields() {
        let (c, mock) =
            client(Mock::default().on(HttpMethod::Patch, "/serverless/ep1", 200, ENDPOINT));
        let update = UpdateEndpoint {
            workers: Some(WorkersConfig {
                min: 1,
                max: 5,
                idle_timeout: Some(10),
            }),
            request_count: Some(8),
            image: Some("img:2".into()),
            data_center_ids: Some(vec![]),
            network_volume_ids: Some(vec![]),
            cmd: Some(vec![]),
            ..UpdateEndpoint::default()
        };
        c.update_endpoint("ep1", &update).await.unwrap();
        let seen = mock.requests();
        assert_eq!(seen[0].method, HttpMethod::Patch);
        assert_eq!(seen[0].url, "https://api.runpod.io/v2/serverless/ep1");
        assert_eq!(
            seen[0].body.as_ref().unwrap(),
            &json!({
                "image": "img:2",
                "cmd": [],
                "workers": {"min": 1, "max": 5, "idleTimeout": 10},
                "scaling": {"type": "REQUEST_COUNT", "requestCount": 8},
                "dataCenterIds": [],
                "mounts": {"network": []}
            })
        );
        assert_eq!(UpdateEndpoint::default().to_body(), json!({}));
    }

    #[tokio::test]
    async fn delete_204_is_deleted() {
        let (c, mock) = client(Mock::default().on(HttpMethod::Delete, "/serverless/ep1", 204, ""));
        assert_eq!(
            c.delete_endpoint("ep1").await.unwrap(),
            DeleteOutcome::Deleted
        );
        assert_eq!(mock.requests()[0].method, HttpMethod::Delete);
    }

    #[tokio::test]
    async fn delete_404_is_already_gone_not_an_error() {
        let (c, _) = client(Mock::default().on(HttpMethod::Delete, "/serverless/ep1", 404, "{}"));
        assert_eq!(
            c.delete_endpoint("ep1").await.unwrap(),
            DeleteOutcome::AlreadyGone
        );
        let (c, _) =
            client(Mock::default().on(HttpMethod::Delete, "/account/secrets/s1", 404, "{}"));
        assert_eq!(
            c.delete_secret("s1").await.unwrap(),
            DeleteOutcome::AlreadyGone
        );
    }

    #[tokio::test]
    async fn delete_other_failures_still_error() {
        let (c, _) = client(Mock::default().on(HttpMethod::Delete, "/serverless/ep1", 401, "{}"));
        assert_eq!(
            c.delete_endpoint("ep1").await.unwrap_err(),
            RunPodError::CredentialsRejected { status: 401 }
        );
    }

    #[tokio::test]
    async fn list_workers_parses_summary() {
        let body = r#"{"endpointVersion":4,
            "summary":{"running":1,"idle":1,"initializing":0,"throttled":0,"unhealthy":0,"total":2},
            "workers":[{"id":"w1","status":"RUNNING","isStale":false,"gpuCount":1,
                "gpuTypeId":"NVIDIA GeForce RTX 4090","dataCenterId":"EU-RO-1"},
                {"id":"w2","status":"IDLE","isStale":true,"gpuCount":1}]}"#;
        let (c, mock) =
            client(Mock::default().on(HttpMethod::Get, "/serverless/ep1/workers", 200, body));
        let list = c.list_workers("ep1").await.unwrap();
        assert_eq!(list.endpoint_version, Some(4));
        assert_eq!(list.summary.total, 2);
        assert_eq!(list.workers[0].data_center_id.as_deref(), Some("EU-RO-1"));
        assert!(list.workers[1].is_stale);
        assert_eq!(
            mock.requests()[0].url,
            "https://api.runpod.io/v2/serverless/ep1/workers"
        );
    }

    #[tokio::test]
    async fn organization_identity() {
        let (c, mock) = client(Mock::default().on(
            HttpMethod::Get,
            "/organization",
            200,
            r#"{"id":"org_1","name":"my-org"}"#,
        ));
        let org = c.get_organization().await.unwrap();
        assert_eq!((org.id.as_str(), org.name.as_str()), ("org_1", "my-org"));
        assert_eq!(
            mock.requests()[0].url,
            "https://api.runpod.io/v2/organization"
        );
    }

    #[tokio::test]
    async fn create_secret_body_and_path() {
        let (c, mock) = client(Mock::default().on(
            HttpMethod::Post,
            "/account/secrets",
            201,
            r#"{"id":"s1","name":"hf-token","createdAt":"2026-09-10T17:23:45Z"}"#,
        ));
        let s = c
            .create_secret(&CreateSecret {
                name: "hf-token".into(),
                value: "hf_VALUE".into(),
                description: Some("HF".into()),
            })
            .await
            .unwrap();
        assert_eq!(s.id, "s1");
        let seen = mock.requests();
        assert_eq!(seen[0].url, "https://api.runpod.io/v2/account/secrets");
        assert_eq!(
            seen[0].body.as_ref().unwrap(),
            &json!({"name": "hf-token", "value": "hf_VALUE", "description": "HF"})
        );
    }

    #[tokio::test]
    async fn create_secret_conflict_never_echoes_the_value() {
        let body = r#"{"title":"Conflict","status":409,"detail":"name taken; value hf_VALUE"}"#;
        let (c, _) = client(Mock::default().on(HttpMethod::Post, "/account/secrets", 409, body));
        let err = c
            .create_secret(&CreateSecret {
                name: "hf-token".into(),
                value: "hf_VALUE".into(),
                description: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, RunPodError::Conflict { .. }));
        assert!(!format!("{err} {err:?}").contains("hf_VALUE"));
    }

    #[tokio::test]
    async fn find_secret_by_name_encodes_query() {
        let body = r#"{"secrets":[{"id":"s1","name":"Hf/Token","createdAt":"x"}]}"#;
        let (c, mock) = client(Mock::default().on(
            HttpMethod::Get,
            "/account/secrets?name=hf%2Ftoken",
            200,
            body,
        ));
        let s = c.find_secret("hf/token").await.unwrap().unwrap();
        assert_eq!(s.id, "s1");
        assert_eq!(
            mock.requests()[0].url,
            "https://api.runpod.io/v2/account/secrets?name=hf%2Ftoken"
        );
    }

    #[tokio::test]
    async fn list_secrets_reads_every_secret() {
        let body = r#"{"secrets":[{"id":"s1","name":"a"},{"id":"s2","name":"b"}]}"#;
        let (c, mock) = client(Mock::default().on(HttpMethod::Get, "/account/secrets", 200, body));
        let names: Vec<String> = c
            .list_secrets()
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(
            mock.requests()[0].url,
            "https://api.runpod.io/v2/account/secrets"
        );
    }

    #[tokio::test]
    async fn status_codes_map_to_typed_errors() {
        let detail = r#"{"title":"t","status":0,"detail":"bad thing","errors":["x is wrong"]}"#;
        for (status, expect) in [
            (401, RunPodError::CredentialsRejected { status: 401 }),
            (403, RunPodError::CredentialsRejected { status: 403 }),
            (
                404,
                RunPodError::NotFound {
                    what: "organization".into(),
                },
            ),
            (
                409,
                RunPodError::Conflict {
                    detail: "bad thing: x is wrong".into(),
                },
            ),
            (429, RunPodError::RateLimited),
            (
                400,
                RunPodError::BadRequest {
                    status: 400,
                    detail: "bad thing: x is wrong".into(),
                },
            ),
            (
                422,
                RunPodError::BadRequest {
                    status: 422,
                    detail: "bad thing: x is wrong".into(),
                },
            ),
            (
                503,
                RunPodError::Api {
                    status: 503,
                    detail: "bad thing: x is wrong".into(),
                },
            ),
        ] {
            let (c, _) =
                client(Mock::default().on(HttpMethod::Get, "/organization", status, detail));
            assert_eq!(c.get_organization().await.unwrap_err(), expect, "{status}");
        }
    }

    #[tokio::test]
    async fn non_json_error_body_and_long_detail_are_bounded() {
        let long = format!(r#"{{"detail":"{}"}}"#, "x".repeat(2000));
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/organization", 500, &long));
        let RunPodError::Api { detail, .. } = c.get_organization().await.unwrap_err() else {
            panic!("expected Api");
        };
        assert_eq!(detail.len(), MAX_DETAIL);
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/organization", 502, "<html>"));
        assert_eq!(
            c.get_organization().await.unwrap_err(),
            RunPodError::Api {
                status: 502,
                detail: String::new()
            }
        );
    }

    #[tokio::test]
    async fn transport_failures_are_retryable_transport_errors() {
        let (c, _) =
            client(Mock::default().fail(HttpMethod::Get, "/organization", TransportError::Timeout));
        let err = c.get_organization().await.unwrap_err();
        assert!(matches!(err, RunPodError::Transport(_)));
        assert!(err.is_retryable());
        assert!(RunPodError::RateLimited.is_retryable());
        assert!(!RunPodError::CredentialsRejected { status: 401 }.is_retryable());
    }

    #[tokio::test]
    async fn malformed_success_body_is_a_decode_error() {
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/organization", 200, "[]"));
        assert!(matches!(
            c.get_organization().await.unwrap_err(),
            RunPodError::Decode(_)
        ));
    }

    #[tokio::test]
    async fn unsafe_ids_are_rejected_before_any_request() {
        let (c, mock) = client(Mock::default());
        for id in ["", "../organization", "a/b", "a?x=1", "a b"] {
            assert!(matches!(
                c.get_endpoint(id).await.unwrap_err(),
                RunPodError::InvalidId(_)
            ));
            assert!(matches!(
                c.delete_endpoint(id).await.unwrap_err(),
                RunPodError::InvalidId(_)
            ));
        }
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn secrets_never_appear_in_debug_output() {
        let c = RunPodClient::with_transport(ApiKey::new(KEY), Arc::new(Mock::default()), BASE);
        let secret = CreateSecret {
            name: "n".into(),
            value: "hf_VALUE".into(),
            description: None,
        };
        let mut req = full_request();
        req.env
            .insert("API_TOKEN".into(), "env-secret-value".into());
        let update = UpdateEndpoint {
            env: Some(BTreeMap::from([(
                "K".to_owned(),
                "update-secret".to_owned(),
            )])),
            ..UpdateEndpoint::default()
        };
        let dump = format!(
            "{c:?} {:?} {secret:?} {req:?} {update:?} {:?}",
            ApiKey::new(KEY),
            c.key
        );
        for leak in [
            KEY,
            "SUPERSECRETKEY",
            "hf_VALUE",
            "env-secret-value",
            "update-secret",
        ] {
            assert!(!dump.contains(leak), "{leak} leaked in {dump}");
        }
        assert!(dump.contains("API_TOKEN"), "env names are fine to show");
    }

    #[test]
    fn endpoint_response_does_not_model_env_values() {
        let ep: Endpoint = serde_json::from_str(ENDPOINT).unwrap();
        assert!(!format!("{ep:?}").contains("leaky"));
        assert_eq!(ep.owner, None);
        let marked: Endpoint = serde_json::from_str(
            r#"{"id":"e","name":"n","env":{"AMICU_OWNER":"abc","T":"leaky"}}"#,
        )
        .unwrap();
        assert_eq!(marked.owner.as_deref(), Some("abc"));
        assert!(!format!("{marked:?}").contains("leaky"));
    }

    #[test]
    fn trailing_slash_on_base_url_is_trimmed() {
        let c = RunPodClient::with_transport(
            ApiKey::new(KEY),
            Arc::new(Mock::default()),
            "https://example.test/v2/",
        );
        assert_eq!(c.base_url, "https://example.test/v2");
    }

    const NO_ORGS: &str = r#"{"detail":"organizations are not enabled for this user"}"#;

    #[tokio::test]
    async fn verify_key_lists_one_endpoint_and_maps_a_refusal() {
        let body = r#"{"endpoints":[],"pagination":{"hasNextPage":false}}"#;
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/serverless?limit=1", 200, body));
        c.verify_key().await.unwrap();
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/serverless?limit=1", 401, "{}"));
        assert_eq!(
            c.verify_key().await.unwrap_err(),
            RunPodError::CredentialsRejected { status: 401 }
        );
    }

    #[tokio::test]
    async fn a_personal_account_is_not_a_rejected_key() {
        let (c, _) = client(Mock::default().on(HttpMethod::Get, "/organization", 403, NO_ORGS));
        assert_eq!(
            c.get_organization().await.unwrap_err(),
            RunPodError::OrganizationsDisabled
        );
        // Any other 403 still means the key is refused.
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            "/organization",
            403,
            r#"{"detail":"forbidden"}"#,
        ));
        assert_eq!(
            c.get_organization().await.unwrap_err(),
            RunPodError::CredentialsRejected { status: 403 }
        );
    }

    #[tokio::test]
    async fn account_identity_prefers_the_organization() {
        let (c, mock) = client(Mock::default().on(
            HttpMethod::Get,
            "/organization",
            200,
            r#"{"id":"org1","name":"o"}"#,
        ));
        let id = c.account_identity(true).await.unwrap().unwrap();
        assert_eq!(id.fingerprint(), "org:org1");
        assert_eq!(mock.requests().len(), 1, "no marker needed");
    }

    #[tokio::test]
    async fn a_personal_account_is_identified_by_its_marker_secret() {
        let found = r#"{"secrets":[{"id":"m1","name":"amicu-account-marker"}]}"#;
        let (c, _) = client(
            Mock::default()
                .on(HttpMethod::Get, "/organization", 403, NO_ORGS)
                .on(
                    HttpMethod::Get,
                    "/account/secrets?name=amicu-account-marker",
                    200,
                    found,
                ),
        );
        assert_eq!(
            c.account_identity(false).await.unwrap(),
            Some(AccountIdentity::Personal {
                marker_secret_id: "m1".into()
            })
        );

        // Missing marker: unknown without create, created with it.
        let none = r#"{"secrets":[]}"#;
        let mock = || {
            Mock::default()
                .on(HttpMethod::Get, "/organization", 403, NO_ORGS)
                .on(
                    HttpMethod::Get,
                    "/account/secrets?name=amicu-account-marker",
                    200,
                    none,
                )
                .on(
                    HttpMethod::Post,
                    "/account/secrets",
                    201,
                    r#"{"id":"m2","name":"amicu-account-marker"}"#,
                )
        };
        let (c, m) = client(mock());
        assert_eq!(c.account_identity(false).await.unwrap(), None);
        assert!(m.requests().iter().all(|s| s.method == HttpMethod::Get));
        let (c, m) = client(mock());
        assert_eq!(
            c.account_identity(true)
                .await
                .unwrap()
                .unwrap()
                .fingerprint(),
            "personal:m2"
        );
        let body = m
            .requests()
            .into_iter()
            .find(|s| s.method == HttpMethod::Post)
            .unwrap()
            .body
            .unwrap();
        assert_eq!(body["name"], json!(ACCOUNT_MARKER_SECRET));
    }

    #[tokio::test]
    async fn gpu_catalog_is_listed_with_pools() {
        let body = r#"{"gpus":[
            {"id":"NVIDIA L40S","name":"L40S","pool":"ADA_48_PRO","memory":48,"secure":true},
            {"id":"NVIDIA A40","name":"A40","pool":"AMPERE_48","memory":48},
            {"id":"Odd GPU","name":"Odd"}]}"#;
        let (c, mock) = client(Mock::default().on(HttpMethod::Get, "/catalog/gpus", 200, body));
        let gpus = c.get_gpu_catalog().await.unwrap();
        assert_eq!(
            mock.requests()[0].url,
            "https://api.runpod.io/v2/catalog/gpus"
        );
        assert_eq!(gpus.len(), 3);
        assert_eq!(gpus[0].pool.as_deref(), Some("ADA_48_PRO"));
        assert_eq!(gpus[0].memory, Some(48));
        assert_eq!(gpus[2].pool, None);
    }
}
