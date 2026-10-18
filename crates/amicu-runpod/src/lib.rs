//! RunPod serverless backend building blocks: a typed client over the RunPod
//! v2 REST API, amicu-GPU to RunPod-GPU-pool mapping, and residency-aware
//! data-center selection.
//!
//! Nothing here is wired into the CLI yet; this crate is the foundation the
//! deploy path builds on. The client talks through the
//! [`amicu_providers::HttpTransport`] seam, so everything is unit-tested with a
//! mock and no network or credentials.
//!
//! Credentials: the API key and secret values are never logged or
//! `Debug`-printed by this crate (see [`ApiKey`]).

pub mod client;
pub mod error;
pub mod hardware;
pub mod regions;

pub use amicu_core::naming::project_endpoint_name;
pub use client::{
    ApiKey, BASE_URL, CreateEndpoint, CreateSecret, DeleteOutcome, Endpoint, EndpointGpu,
    EndpointWorkers, FlashBoot, GpuRequest, Organization, RequestUrls, RunPodClient, Secret,
    UpdateEndpoint, Worker, WorkerSummary, WorkersConfig, WorkersList,
};
pub use error::RunPodError;
pub use hardware::{GpuPlacement, gpu_placement, parse_gpu_spec};
pub use regions::select_data_centers;
