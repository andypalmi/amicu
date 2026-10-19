//! RunPod backend: a typed v2 REST client, GPU-pool mapping, data-center
//! selection, [`RunPodSupervisor`] (a load-balancing endpoint) and
//! [`PodSupervisor`] (an always-on Pod), both behind the shared
//! `ProcessSupervisor` contract, plus serverless worker logs and the
//! `/metrics` scrape.

pub mod client;
pub mod error;
pub mod hardware;
pub mod logs;
pub mod metrics;
pub mod pod;
pub mod regions;
mod secrets;
pub mod supervisor;

pub use amicu_core::naming::project_endpoint_name;
pub use client::{ACCOUNT_MARKER_SECRET, AccountIdentity, GpuType, OWNER_ENV_VAR};
pub use client::{
    ApiKey, BASE_URL, CreateEndpoint, CreateSecret, DeleteOutcome, Endpoint, EndpointGpu,
    EndpointWorkers, FlashBoot, GpuRequest, Organization, RequestUrls, RunPodClient, Secret,
    UpdateEndpoint, Worker, WorkerSummary, WorkersConfig, WorkersList,
};
pub use client::{
    Cloud, CreatePod, Pod, PodAction, PodEnv, PodGpu, PodStatus, UpdatePod, pod_proxy_url,
};
pub use error::RunPodError;
pub use hardware::{
    GpuPlacement, PodGpuPlacement, exclusions_from_catalog, gpu_placement, parse_gpu_spec,
    pod_gpu_placement, verify_catalog_memory,
};
pub use pod::PodSupervisor;
pub use regions::select_data_centers;
pub use supervisor::{
    ReadinessTiming, RunPodSupervisor, StopOutcome, StopRetry, endpoint_url, owner_marker,
    secret_name, secret_reference,
};
