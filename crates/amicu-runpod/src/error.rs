//! Typed errors for the RunPod client.

/// Everything that can go wrong talking to RunPod or mapping amicu config onto
/// it. HTTP statuses follow the mapping the other REST providers use
/// (`401`/`403` credentials, `404` unknown, `409` conflict, `429` rate limit).
///
/// No variant carries credential material: `detail` strings come from the API's
/// `application/problem+json` `detail` field (truncated, with the request's
/// env values cut out), and the secret-write calls drop even that.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RunPodError {
    /// A `POST` that never got a response: the endpoint may or may not exist.
    /// Not retryable: look it up by name first or a duplicate may result.
    #[error(
        "RunPod create outcome unknown (transport failure: {0}); look up by name before retrying"
    )]
    AmbiguousCreate(String),
    /// More than one endpoint (or pod) carries the requested name.
    #[error("{count} RunPod resources are named `{name}`")]
    DuplicateName { name: String, count: usize },
    /// The exchange never completed (timeout, DNS, TLS). Transient.
    #[error("RunPod transport failure: {0}")]
    Transport(String),
    /// `GET /organization` answered `403` "organizations are not enabled for
    /// this user": a personal account, not a rejected key.
    #[error("RunPod organizations are not enabled for this account (a personal account)")]
    OrganizationsDisabled,
    /// `401`/`403`: the API key is missing, invalid, or lacks the scope.
    #[error("RunPod rejected the API key (HTTP {status})")]
    CredentialsRejected { status: u16 },
    /// `404`: the endpoint, secret, or organization does not exist.
    #[error("RunPod resource not found: {what}")]
    NotFound { what: String },
    /// `409`: a conflicting resource already exists (e.g. a secret name).
    #[error("RunPod conflict: {detail}")]
    Conflict { detail: String },
    /// `429`: rate limited or out of capacity. Retryable.
    #[error("RunPod rate limited or at capacity (HTTP 429)")]
    RateLimited,
    /// `400`/`422`: the request body was rejected.
    #[error("RunPod rejected the request (HTTP {status}): {detail}")]
    BadRequest { status: u16, detail: String },
    /// Any other non-`2xx` status.
    #[error("RunPod API returned HTTP {status}: {detail}")]
    Api { status: u16, detail: String },
    /// A `2xx` body that did not match the documented shape.
    #[error("RunPod returned an unexpected response body: {0}")]
    Decode(String),
    /// An id that is not a safe single path segment (never sent).
    #[error("invalid RunPod id `{0}`")]
    InvalidId(String),
    /// The amicu GPU model has no RunPod serverless pool.
    #[error("no RunPod GPU pool for accelerator `{model}`")]
    UnknownGpu { model: String },
    /// The accelerator is not a usable GPU request (wrong kind, zero count,
    /// malformed spec).
    #[error("invalid GPU request: {0}")]
    InvalidGpu(String),
    /// The region is neither a known RunPod region group nor a data center.
    #[error("unknown RunPod region `{region}`")]
    UnknownRegion { region: String },
    /// The region lies outside the required data residency.
    #[error("region `{region}` is not within the required residency ({required})")]
    ResidencyViolation { region: String, required: String },
    /// The network volume lives outside the data centers the deployment may
    /// be placed in.
    #[error(
        "network volume `{volume}` is in data center {data_center}, outside the deployment's \
         data centers ({allowed}); use a volume in one of them or change the profile's `region`"
    )]
    VolumeOutsidePlacement {
        volume: String,
        data_center: String,
        allowed: String,
    },
}

impl RunPodError {
    /// Whether retrying the same call later can plausibly succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::RateLimited => true,
            Self::Api { status, .. } => *status >= 500,
            _ => false,
        }
    }
}
