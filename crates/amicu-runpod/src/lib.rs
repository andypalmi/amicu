//! RunPod backend building blocks: a typed client over the RunPod v2 REST
//! API, amicu-GPU to RunPod pool / GPU type mapping, and residency-aware
//! data-center selection.
//!
//! [`RunPodSupervisor`] runs a deployment as a load-balancing endpoint and
//! [`PodSupervisor`] as an always-on Pod, both behind the shared
//! `ProcessSupervisor` contract (pods are not wired into the CLI yet). The
//! client talks through the [`amicu_providers::HttpTransport`] seam, so
//! everything is unit-tested with a mock and no network or credentials.
//!
//! Credentials: the API key and secret values are never logged or
//! `Debug`-printed by this crate (see [`ApiKey`]).

pub mod client;
pub mod error;
pub mod hardware;
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
