//! The v2 Pods resource (`/pods`, `/pods/{id}`, `/pods/{id}/action`): an
//! always-on GPU Pod billed while it exists.

use std::collections::BTreeMap;
use std::fmt;

use amicu_providers::HttpMethod;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{
    DeleteOutcome, MAX_PAGES, OWNER_ENV_VAR, Pagination, RunPodClient, check, decode, encode_query,
    insert_container, non_empty, segment,
};
use crate::error::RunPodError;

/// Cloud tier a pod is placed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Cloud {
    /// RunPod-owned data-center hardware (the API default).
    #[default]
    Secure,
    /// Community-hosted hardware.
    Community,
    /// A tier this client does not know yet.
    #[serde(other)]
    Unknown,
}

impl Cloud {
    /// The wire spelling (`SECURE` / `COMMUNITY`).
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Secure => "SECURE",
            Self::Community => "COMMUNITY",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// Lifecycle status of a pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PodStatus {
    /// Being allocated.
    Provisioning,
    /// Container starting.
    Starting,
    /// Container healthy.
    Running,
    /// Container exited (stopped; disk kept, still billed for storage).
    Exited,
    /// Unrecoverable container error.
    Error,
    /// Permanently deleted.
    Terminated,
    /// A status this client does not know yet.
    #[serde(other)]
    Unknown,
}

impl PodStatus {
    /// The wire spelling (`UNKNOWN` for an unrecognised status).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "PROVISIONING",
            Self::Starting => "STARTING",
            Self::Running => "RUNNING",
            Self::Exited => "EXITED",
            Self::Error => "ERROR",
            Self::Terminated => "TERMINATED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// `POST /pods/{id}/action` transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodAction {
    /// Boot an `EXITED` / `ERROR` pod.
    Start,
    /// Release compute, keep the disk (`EXITED`).
    Stop,
    /// Restart a `RUNNING` pod's container.
    Restart,
    /// Delete the pod permanently.
    Terminate,
}

impl PodAction {
    fn wire(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Terminate => "terminate",
        }
    }
}

/// Body of `POST /pods`: a GPU pod from an image (no template), with only
/// the listed ports exposed and RunPod's SSH / Jupyter bootstrap off.
///
/// `Debug` prints environment variable names only, never their values.
#[derive(Clone)]
pub struct CreatePod {
    /// Pod name (the identity amicu looks it up by).
    pub name: String,
    /// Container image.
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
    /// Container disk in GB.
    pub disk_gb: Option<u32>,
    /// Exact GPU type id (`"NVIDIA H100 80GB HBM3"`).
    pub gpu_type_id: String,
    /// GPUs on the pod.
    pub gpu_count: u32,
    /// Lowest host CUDA version (`major.minor`) the pod may be placed on.
    pub min_cuda_version: Option<String>,
    /// Cloud tier.
    pub cloud: Cloud,
    /// Placement data centers; empty lets RunPod choose.
    pub data_center_ids: Vec<String>,
    /// Container registry credential id for a private image.
    pub registry_id: Option<String>,
}

impl CreatePod {
    /// A minimal request on the secure cloud. Fill in the public fields as
    /// needed.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        image: impl Into<String>,
        gpu_type_id: impl Into<String>,
        gpu_count: u32,
    ) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            entrypoint: Vec::new(),
            cmd: Vec::new(),
            env: BTreeMap::new(),
            ports: Vec::new(),
            disk_gb: None,
            gpu_type_id: gpu_type_id.into(),
            gpu_count,
            min_cuda_version: None,
            cloud: Cloud::Secure,
            data_center_ids: Vec::new(),
            registry_id: None,
        }
    }

    /// The exact JSON body sent to RunPod.
    #[must_use]
    pub fn to_body(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("image".into(), json!(self.image));
        insert_container(
            &mut m,
            non_empty(&self.entrypoint),
            non_empty(&self.cmd),
            (!self.env.is_empty()).then_some(&self.env),
            // An omitted `ports` could inherit defaults; always state them.
            Some(&self.ports),
            self.disk_gb,
            self.registry_id.as_deref(),
        );
        let mut gpu = json!({"id": self.gpu_type_id, "count": self.gpu_count});
        if let Some(v) = &self.min_cuda_version {
            gpu["minCudaVersion"] = json!(v);
        }
        m.insert("gpu".into(), gpu);
        m.insert("cloud".into(), json!(self.cloud.wire()));
        if !self.data_center_ids.is_empty() {
            m.insert("dataCenterIds".into(), json!(self.data_center_ids));
        }
        m.insert("startSsh".into(), json!(false));
        m.insert("startJupyter".into(), json!(false));
        Value::Object(m)
    }
}

impl fmt::Debug for CreatePod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreatePod")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("ports", &self.ports)
            .field("gpu_type_id", &self.gpu_type_id)
            .field("gpu_count", &self.gpu_count)
            .field("min_cuda_version", &self.min_cuda_version)
            .field("cloud", &self.cloud)
            .field("data_center_ids", &self.data_center_ids)
            .finish_non_exhaustive()
    }
}

/// Body of `PATCH /pods/{id}`: only `Some` fields are sent and changed. GPU,
/// cloud and placement are fixed at create. `Debug` prints env names only.
#[derive(Clone, Default)]
pub struct UpdatePod {
    /// New name.
    pub name: Option<String>,
    /// New image.
    pub image: Option<String>,
    /// Replacement ENTRYPOINT (`Some(vec![])` clears it).
    pub entrypoint: Option<Vec<String>>,
    /// Replacement CMD (`Some(vec![])` clears it).
    pub cmd: Option<Vec<String>>,
    /// Replacement environment (replaces the whole map).
    pub env: Option<BTreeMap<String, String>>,
    /// Replacement ports.
    pub ports: Option<Vec<String>>,
    /// New container disk in GB.
    pub disk_gb: Option<u32>,
    /// New registry credential id.
    pub registry_id: Option<String>,
}

impl UpdatePod {
    /// The exact JSON body sent to RunPod.
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
        Value::Object(m)
    }
}

impl fmt::Debug for UpdatePod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpdatePod")
            .field("name", &self.name)
            .field("image", &self.image)
            .field(
                "env_keys",
                &self.env.as_ref().map(|e| e.keys().collect::<Vec<_>>()),
            )
            .finish_non_exhaustive()
    }
}

/// GPU settings of a returned pod.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PodGpu {
    /// GPU type id.
    pub id: String,
    /// GPUs on the pod.
    #[serde(default)]
    pub count: Option<u32>,
}

/// A pod's env as RunPod returns it: the [`OWNER_ENV_VAR`] marker plus a
/// digest of the whole map, so it can be compared without keeping the values.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PodEnv {
    /// The owner marker, when present.
    pub owner: Option<String>,
    digest: Option<[u8; 32]>,
}

impl PodEnv {
    /// Whether the pod's env is exactly `env`.
    #[must_use]
    pub fn matches(&self, env: &BTreeMap<String, String>) -> bool {
        self.digest.is_some_and(|d| d == env_digest(env))
    }
}

impl fmt::Debug for PodEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PodEnv")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

fn env_digest(env: &BTreeMap<String, String>) -> [u8; 32] {
    let mut h = Sha256::new();
    for (k, v) in env {
        for part in [k, v] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
    }
    h.finalize().into()
}

fn pod_env<'de, D>(d: D) -> Result<PodEnv, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(raw) = Option::<BTreeMap<String, Value>>::deserialize(d)? else {
        return Ok(PodEnv::default());
    };
    let owner = raw
        .get(OWNER_ENV_VAR)
        .and_then(Value::as_str)
        .map(str::to_owned);
    let strings: Option<BTreeMap<String, String>> = raw
        .into_iter()
        .map(|(k, v)| match v {
            Value::String(s) => Some((k, s)),
            _ => None,
        })
        .collect();
    Ok(PodEnv {
        owner,
        digest: strings.as_ref().map(env_digest),
    })
}

/// A pod as RunPod returns it; env values are only kept as a [`PodEnv`].
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pod {
    /// Pod id.
    pub id: String,
    /// Pod name.
    pub name: String,
    /// Lifecycle status.
    pub status: PodStatus,
    /// Container image.
    #[serde(default)]
    pub image: Option<String>,
    /// Container ENTRYPOINT.
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// Container CMD.
    #[serde(default)]
    pub cmd: Vec<String>,
    /// Exposed ports (`port/protocol`).
    #[serde(default)]
    pub ports: Vec<String>,
    /// Container disk in GB.
    #[serde(default)]
    pub disk: Option<u32>,
    /// GPU settings (absent for CPU pods).
    #[serde(default)]
    pub gpu: Option<PodGpu>,
    /// Cloud tier.
    #[serde(default)]
    pub cloud: Option<Cloud>,
    /// Data center the scheduler placed the pod in.
    #[serde(default)]
    pub data_center_id: Option<String>,
    /// Current cost in USD per hour (`0` while `EXITED` / `TERMINATED`).
    #[serde(default)]
    pub cost: Option<f64>,
    /// Creation time (RFC 3339).
    #[serde(default)]
    pub created_at: Option<String>,
    /// Last start time (RFC 3339).
    #[serde(default)]
    pub started_at: Option<String>,
    /// The pod env, reduced to the owner marker and a digest.
    #[serde(default, deserialize_with = "pod_env")]
    pub env: PodEnv,
}

#[derive(Deserialize)]
struct PodsPage {
    pods: Vec<Pod>,
    pagination: Pagination,
}

/// The public proxy URL of `port` on pod `id`. The proxy adds no
/// authentication of its own.
#[must_use]
pub fn pod_proxy_url(id: &str, port: u16) -> String {
    format!("https://{id}-{port}.proxy.runpod.net")
}

impl RunPodClient {
    /// `POST /pods`. A transport failure is [`RunPodError::AmbiguousCreate`]
    /// (see [`create_endpoint`](Self::create_endpoint)): look the name up with
    /// [`find_pods_by_name`](Self::find_pods_by_name) before retrying. Error
    /// details are withheld when the request carries env values.
    pub async fn create_pod(&self, req: &CreatePod) -> Result<Pod, RunPodError> {
        let resp = self
            .send_inner(
                HttpMethod::Post,
                "/pods",
                Some(req.to_body()),
                "pod",
                !req.env.is_empty(),
            )
            .await
            .map_err(|e| match e {
                RunPodError::Transport(m) => RunPodError::AmbiguousCreate(m),
                other => other,
            })?;
        decode(&resp)
    }

    /// `GET /pods/{id}`.
    pub async fn get_pod(&self, id: &str) -> Result<Pod, RunPodError> {
        let path = format!("/pods/{}", segment(id)?);
        let resp = self
            .send(HttpMethod::Get, &path, None, &format!("pod `{id}`"))
            .await?;
        decode(&resp)
    }

    /// `GET /pods`, following pagination to the end (newest first; cluster
    /// members excluded).
    pub async fn list_pods(&self) -> Result<Vec<Pod>, RunPodError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let path = match &cursor {
                Some(c) => format!("/pods?cursor={}", encode_query(c)),
                None => "/pods".to_owned(),
            };
            let resp = self.send(HttpMethod::Get, &path, None, "pods").await?;
            let page: PodsPage = decode(&resp)?;
            all.extend(page.pods);
            match page.pagination.next_cursor {
                Some(next) if page.pagination.has_next_page => cursor = Some(next),
                _ => return Ok(all),
            }
        }
        Err(RunPodError::Decode(format!(
            "pod list still paginating after {MAX_PAGES} pages"
        )))
    }

    /// Every pod named exactly `name` that is not `TERMINATED`, newest first.
    pub async fn find_pods_by_name(&self, name: &str) -> Result<Vec<Pod>, RunPodError> {
        Ok(self
            .list_pods()
            .await?
            .into_iter()
            .filter(|p| p.name == name && p.status != PodStatus::Terminated)
            .collect())
    }

    /// The one pod named exactly `name`, or `None`; several is
    /// [`RunPodError::DuplicateName`].
    pub async fn find_pod_by_name(&self, name: &str) -> Result<Option<Pod>, RunPodError> {
        let mut found = self.find_pods_by_name(name).await?;
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            count => Err(RunPodError::DuplicateName {
                name: name.to_owned(),
                count,
            }),
        }
    }

    /// `PATCH /pods/{id}`. Error details are withheld when the request
    /// carries env values.
    pub async fn update_pod(&self, id: &str, update: &UpdatePod) -> Result<Pod, RunPodError> {
        let path = format!("/pods/{}", segment(id)?);
        let resp = self
            .send_inner(
                HttpMethod::Patch,
                &path,
                Some(update.to_body()),
                &format!("pod `{id}`"),
                update.env.as_ref().is_some_and(|e| !e.is_empty()),
            )
            .await?;
        decode(&resp)
    }

    /// `DELETE /pods/{id}` (terminates). `404` is [`DeleteOutcome::AlreadyGone`].
    pub async fn delete_pod(&self, id: &str) -> Result<DeleteOutcome, RunPodError> {
        let path = format!("/pods/{}", segment(id)?);
        self.delete(&path, &format!("pod `{id}`")).await
    }

    /// `POST /pods/{id}/action`. `start` / `stop` / `restart` return the
    /// updated pod; `terminate` answers `204` and returns `None`. An action
    /// the current status does not allow is [`RunPodError::Conflict`].
    pub async fn pod_action(
        &self,
        id: &str,
        action: PodAction,
    ) -> Result<Option<Pod>, RunPodError> {
        let path = format!("/pods/{}/action", segment(id)?);
        let resp = self
            .send(
                HttpMethod::Post,
                &path,
                Some(json!({"action": action.wire()})),
                &format!("pod `{id}`"),
            )
            .await?;
        if resp.status == 204 || resp.body.is_empty() {
            return Ok(None);
        }
        decode(&resp).map(Some)
    }

    /// Terminates pod `id` through the action endpoint. `404` is
    /// [`DeleteOutcome::AlreadyGone`].
    pub async fn terminate_pod(&self, id: &str) -> Result<DeleteOutcome, RunPodError> {
        let path = format!("/pods/{}/action", segment(id)?);
        let resp = self
            .execute(
                HttpMethod::Post,
                &path,
                Some(json!({"action": PodAction::Terminate.wire()})),
            )
            .await?;
        if resp.status == 404 {
            return Ok(DeleteOutcome::AlreadyGone);
        }
        check(&resp, &format!("pod `{id}`"), false)?;
        Ok(DeleteOutcome::Deleted)
    }
}

#[cfg(test)]
mod tests;
