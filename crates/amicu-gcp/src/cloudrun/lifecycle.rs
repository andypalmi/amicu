//! `prepare`, `create`, `get`, `delete` for one Cloud Run service.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use amicu_providers::HttpMethod;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::spec::ServiceSpec;
use crate::catalog::GcpCatalog;
use crate::client::{GcpClient, RUN_BASE_URL};
use crate::error::GcpError;
use crate::labels::{ensure_owned, owner_marker};
use crate::operations::{PollTiming, wait_run_operation};
use crate::validate::{validate_project_id, validate_region, validate_resource_name};

/// Appended to quota errors.
pub const QUOTA_HINT: &str = "Cloud Run GPU quota is per region and GPU type (new projects get \
     3 GPUs without zonal redundancy; RTX PRO 6000 is in milliGPUs). Request an increase on the \
     Quotas page, pick another region, or lower max instances.";

/// Extra time `create` waits beyond the startup budget: the operation also
/// covers pulling the image and creating the revision before the probe starts.
pub const CREATE_MARGIN: Duration = Duration::from_mins(5);

/// What `prepare` returns: enough to find, check and delete the service, to be
/// persisted BEFORE `create` (write-ahead), so a crash cannot orphan a billed
/// service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRecord {
    pub project: String,
    pub region: String,
    pub service: String,
    /// The `amicu-owner` label value the service must carry.
    pub owner: String,
}

impl ServiceRecord {
    /// `projects/<p>/locations/<r>/services/<name>`.
    #[must_use]
    pub fn resource_name(&self) -> String {
        format!(
            "projects/{}/locations/{}/services/{}",
            self.project, self.region, self.service
        )
    }

    fn url(&self) -> String {
        format!("{RUN_BASE_URL}/{}", self.resource_name())
    }

    fn validate(&self) -> Result<(), GcpError> {
        validate_project_id(&self.project)?;
        validate_region(&self.region)?;
        validate_resource_name(&self.service)
    }
}

/// The service's `Ready` condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyState {
    /// `CONDITION_SUCCEEDED`.
    Ready,
    /// Pending or reconciling.
    Pending,
    /// `CONDITION_FAILED`, with Google's message.
    Failed(String),
}

/// A snapshot of a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub state: ReadyState,
    pub latest_ready_revision: Option<String>,
    /// The `*.run.app` URL.
    pub uri: Option<String>,
    pub labels: BTreeMap<String, String>,
    /// Set once Google has started deleting the service.
    pub deleting: bool,
}

impl ServiceStatus {
    /// Ready condition true and a revision is serving.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state == ReadyState::Ready && self.latest_ready_revision.is_some()
    }

    fn parse(v: &Value) -> Self {
        // `terminalCondition` is the overall status; fall back to the `Ready`
        // entry of `conditions`.
        let cond = if v["terminalCondition"].is_object() {
            Some(&v["terminalCondition"])
        } else {
            v["conditions"]
                .as_array()
                .and_then(|c| c.iter().find(|c| c["type"] == "Ready"))
        };
        let state = match cond.and_then(|c| c["state"].as_str()) {
            Some("CONDITION_SUCCEEDED") => ReadyState::Ready,
            Some("CONDITION_FAILED") => ReadyState::Failed(
                cond.and_then(|c| c["message"].as_str())
                    .unwrap_or("no message")
                    .to_owned(),
            ),
            _ => ReadyState::Pending,
        };
        let text = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        Self {
            state,
            latest_ready_revision: text("latestReadyRevision"),
            uri: text("uri"),
            labels: v["labels"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect(),
            deleting: v["deleteTime"].is_string(),
        }
    }
}

/// What `delete` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// Deleted, and a follow-up `GET` returned 404.
    Deleted,
    /// There was no such service.
    AlreadyGone,
}

/// Creates, inspects and deletes Cloud Run services through a [`GcpClient`].
#[derive(Debug, Clone)]
pub struct CloudRunLifecycle {
    client: Arc<GcpClient>,
    timing: PollTiming,
    catalog: GcpCatalog,
}

impl CloudRunLifecycle {
    #[must_use]
    pub fn new(client: Arc<GcpClient>) -> Self {
        Self {
            client,
            timing: PollTiming::default(),
            catalog: GcpCatalog::new(),
        }
    }

    /// Overrides how long operations are awaited. [`create`](Self::create)
    /// never waits less than the spec's startup budget plus
    /// [`CREATE_MARGIN`], whatever is set here; a caller that wants to stop
    /// earlier drops the future and polls [`status`](Self::status).
    #[must_use]
    pub fn with_timing(mut self, timing: PollTiming) -> Self {
        self.timing = timing;
        self
    }

    /// Validates `spec` (region and GPU against the catalog) and returns the
    /// record to persist before [`create`](Self::create). Calls nothing.
    ///
    /// # Errors
    /// [`GcpError::Invalid`].
    pub fn prepare(&self, spec: &ServiceSpec) -> Result<ServiceRecord, GcpError> {
        spec.validate(&self.catalog)?;
        Ok(ServiceRecord {
            project: spec.project.clone(),
            region: spec.region.clone(),
            service: spec.service_name(),
            owner: owner_marker(
                spec.workspace_id.as_deref(),
                &spec.project,
                &spec.deployment_id,
            ),
        })
    }

    /// Creates the service and waits for the operation. Returns the service's
    /// status (check [`ServiceStatus::is_ready`]).
    ///
    /// A lost response (transport error or operation timeout) is recovered by
    /// reading the service: ours (owner label) is adopted. A `409` on a service
    /// we own is adopted too (a retried create); on any other service it is an
    /// error and nothing is touched. Quota errors carry [`QUOTA_HINT`].
    ///
    /// The wait is at least `startup_budget_secs` plus [`CREATE_MARGIN`] (the
    /// operation only finishes once the first revision's startup probe passes),
    /// even when the configured [`PollTiming`] is shorter. Past that, the
    /// timeout is [`GcpError::OperationTimeout`] and the caller polls
    /// [`status`](Self::status) until the service is ready.
    ///
    /// If the operation fails after the service resource exists (for example the
    /// first revision cannot start), the service is left in place for
    /// [`status`](Self::status) and [`delete`](Self::delete).
    ///
    /// # Errors
    /// [`GcpError::NotOwned`] for a foreign service of the same name, or the
    /// mapped Google error.
    pub async fn create(
        &self,
        record: &ServiceRecord,
        spec: &ServiceSpec,
    ) -> Result<ServiceStatus, GcpError> {
        record.validate()?;
        let body = spec.create_body(&self.catalog)?;
        let url = format!(
            "{RUN_BASE_URL}/projects/{}/locations/{}/services?serviceId={}",
            record.project, record.region, record.service
        );
        let started = self
            .client
            .request(HttpMethod::Post, &url, Some(body))
            .await;
        let outcome = match started {
            Ok(op) => match op["name"].as_str() {
                Some(name) => {
                    wait_run_operation(&self.client, name, create_timing(self.timing, spec))
                        .await
                        .map(|_| ())
                }
                None => Err(GcpError::Decode("create returned no operation name".into())),
            },
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => self.owned_status(record).await,
            Err(GcpError::Conflict { message }) => match self.get(record).await? {
                Some(s) => {
                    Self::check_owner(record, &s)?;
                    Ok(s)
                }
                None => Err(GcpError::Conflict { message }),
            },
            Err(e @ (GcpError::Transport(_) | GcpError::OperationTimeout { .. })) => {
                match self.get(record).await {
                    Ok(Some(s)) => {
                        Self::check_owner(record, &s)?;
                        Ok(s)
                    }
                    _ => Err(e),
                }
            }
            Err(e) => Err(with_quota_hint(e)),
        }
    }

    /// The service, or `None` once it is gone (404).
    ///
    /// # Errors
    /// Any error but not-found.
    pub async fn get(&self, record: &ServiceRecord) -> Result<Option<ServiceStatus>, GcpError> {
        record.validate()?;
        match self
            .client
            .request(HttpMethod::Get, &record.url(), None)
            .await
        {
            Ok(v) => Ok(Some(ServiceStatus::parse(&v))),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Status from the `Ready` condition and `latestReadyRevision`; `None` when
    /// the service does not exist. Refuses a service that is not ours.
    ///
    /// # Errors
    /// [`GcpError::NotOwned`] or the mapped Google error.
    pub async fn status(&self, record: &ServiceRecord) -> Result<Option<ServiceStatus>, GcpError> {
        match self.get(record).await? {
            Some(s) => {
                Self::check_owner(record, &s)?;
                Ok(Some(s))
            }
            None => Ok(None),
        }
    }

    /// The service URL, once Google reports it.
    ///
    /// # Errors
    /// As [`status`](Self::status); [`GcpError::NotFound`] when absent.
    pub async fn url(&self, record: &ServiceRecord) -> Result<Option<String>, GcpError> {
        match self.status(record).await? {
            Some(s) => Ok(s.uri),
            None => Err(GcpError::NotFound {
                what: record.resource_name(),
            }),
        }
    }

    /// Deletes the service if it carries our owner label, waits for the
    /// operation, then confirms with a `GET` that returns 404.
    ///
    /// # Errors
    /// [`GcpError::NotOwned`] (nothing is deleted), the operation's error, or
    /// [`GcpError::OperationFailed`] (`STILL_EXISTS`) if the service is still
    /// there afterwards.
    pub async fn delete(&self, record: &ServiceRecord) -> Result<DeleteOutcome, GcpError> {
        let Some(current) = self.get(record).await? else {
            return Ok(DeleteOutcome::AlreadyGone);
        };
        Self::check_owner(record, &current)?;
        let op = match self
            .client
            .request(HttpMethod::Delete, &record.url(), None)
            .await
        {
            Ok(op) => op,
            Err(e) if e.is_not_found() => return Ok(DeleteOutcome::AlreadyGone),
            Err(e) => return Err(e),
        };
        if let Some(name) = op["name"].as_str().filter(|_| op["done"] != true) {
            wait_run_operation(&self.client, name, self.timing).await?;
        }
        match self.get(record).await? {
            None => Ok(DeleteOutcome::Deleted),
            Some(_) => Err(GcpError::OperationFailed {
                operation: record.resource_name(),
                code: "STILL_EXISTS".into(),
                message: "the service still exists after the delete operation finished".into(),
            }),
        }
    }

    async fn owned_status(&self, record: &ServiceRecord) -> Result<ServiceStatus, GcpError> {
        let s = self.get(record).await?.ok_or_else(|| GcpError::NotFound {
            what: record.resource_name(),
        })?;
        Self::check_owner(record, &s)?;
        Ok(s)
    }

    fn check_owner(record: &ServiceRecord, s: &ServiceStatus) -> Result<(), GcpError> {
        ensure_owned(
            &format!("Cloud Run service {}", record.resource_name()),
            Some(&s.labels),
            &record.owner,
        )
    }
}

/// `timing` with the timeout raised to the startup budget plus the margin.
fn create_timing(timing: PollTiming, spec: &ServiceSpec) -> PollTiming {
    PollTiming {
        timeout: timing
            .timeout
            .max(Duration::from_secs(u64::from(spec.startup_budget_secs)) + CREATE_MARGIN),
        ..timing
    }
}

/// Quota failures arrive as HTTP 429 / `RESOURCE_EXHAUSTED`, or as a failed
/// operation with gRPC code 8.
fn with_quota_hint(e: GcpError) -> GcpError {
    match e {
        GcpError::QuotaExceeded { message } => GcpError::QuotaExceeded {
            message: format!("{message}. {QUOTA_HINT}"),
        },
        // Cloud Run answers an exhausted GPU quota with a `429`; waiting does
        // not help, so it counts as a quota error. A plain `429` stays a
        // retryable rate limit.
        GcpError::RateLimited { message } if message.to_ascii_lowercase().contains("quota") => {
            GcpError::QuotaExceeded {
                message: format!("{message}. {QUOTA_HINT}"),
            }
        }
        GcpError::OperationFailed {
            ref code,
            ref message,
            ..
        } if code == "8" || message.to_ascii_lowercase().contains("quota") => {
            GcpError::QuotaExceeded {
                message: format!("{message}. {QUOTA_HINT}"),
            }
        }
        other => other,
    }
}
