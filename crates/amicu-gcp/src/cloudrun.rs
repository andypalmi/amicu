//! Cloud Run GPU services for amicu's Google Cloud backend: one vLLM
//! OpenAI-compatible server per deployment, scaled to zero when idle.
//!
//! - [`spec`]: the service spec and the Cloud Run v2 `services.create` body.
//! - [`lifecycle`]: `prepare` (write-ahead record), `create`, `get`, `delete`.
//! - [`health`]: waiting for `<url>/health` after create.
//!
//! API references (field names verified against these):
//! - <https://docs.cloud.google.com/run/docs/reference/rest/v2/projects.locations.services>
//!   (`Service`, `RevisionTemplate`, `RevisionScaling`, `Condition`)
//! - <https://docs.cloud.google.com/run/docs/reference/rest/v2/projects.locations.services/create>
//! - <https://docs.cloud.google.com/run/docs/reference/rest/v2/Container>
//! - <https://docs.cloud.google.com/run/docs/configuring/services/gpu>
//!
//! # Coexistence
//! amicu only creates, changes and deletes the one service named by its
//! [`ServiceRecord`], and only while it carries the owner label. It never
//! touches project IAM, APIs or other services, and never changes a service
//! IAM policy: public access uses `invokerIamDisabled` on our own service (see
//! [`ServiceAccess`]).
//!
//! # Cold start
//! With `minInstanceCount = 0` the first request after an idle period starts an
//! instance and loads the model, which takes minutes for a large model (longer
//! when the weights are downloaded). Cloud Run holds that request until the
//! container is ready or the request timeout passes, so clients need a long
//! timeout on the first call.

pub mod health;
pub mod lifecycle;
pub mod spec;

pub use health::{HealthTiming, wait_ready};
pub use lifecycle::{
    CloudRunLifecycle, DeleteOutcome, QUOTA_HINT, ReadyState, ServiceRecord, ServiceStatus,
};
pub use spec::{
    CONTAINER_PORT, DEFAULT_VLLM_IMAGE, SecretRef, ServiceAccess, ServiceSpec, service_name,
};

#[cfg(test)]
mod tests;
