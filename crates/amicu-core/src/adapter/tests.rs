//! Example adapter family and fake, exercising the common contract.
//!
//! `ExampleAdapter` stands in for a real family trait (runtime, provider, ...)
//! to demonstrate the intended pattern end to end: a plain-data capability
//! descriptor, a pure planning method, an async apply method, a `thiserror`
//! enum classified into [`AdapterError`], and registration in a typed
//! [`Registry`].

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{
    Adapter, AdapterError, AdapterErrorKind, AdapterId, DescribeCapabilities, HealthStatus,
    Planned, Registry,
};
use crate::diagnostic::{Severity, catalog};

/// Plain-data capability descriptor, consumed without side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExampleCapabilities {
    max_replicas: u32,
    supports_teardown: bool,
}

/// Owned plan produced by the pure planning method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProvisionPlan {
    replicas: u32,
}

/// Result of the side-effecting apply method.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProvisionOutcome {
    provisioned: u32,
}

/// Adapter-specific error enum, classified into [`AdapterError`].
#[derive(Debug, thiserror::Error)]
enum ExampleError {
    #[error("capacity for {requested} replicas is unavailable (limit {limit})")]
    CapacityUnavailable { requested: u32, limit: u32 },
    #[error("adapter is misconfigured: {0}")]
    Misconfigured(String),
}

impl AdapterErrorKind for ExampleError {
    fn code(&self) -> crate::diagnostic::DiagnosticCode {
        match self {
            ExampleError::CapacityUnavailable { .. } => catalog::PROVIDER_CAPACITY_UNAVAILABLE,
            ExampleError::Misconfigured(_) => catalog::SCHEMA_MISSING_REQUIRED,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Error
    }

    fn retryable(&self) -> bool {
        // Capacity may free up (transient); misconfiguration will not (permanent).
        matches!(self, ExampleError::CapacityUnavailable { .. })
    }
}

/// Example family trait: base [`Adapter`] plus capabilities, a pure planning
/// method, and an async apply method.
#[async_trait]
trait ExampleAdapter: Adapter + DescribeCapabilities {
    fn capabilities(&self) -> ExampleCapabilities;

    /// Pure planning method: synchronous, owned output, no side effects.
    fn plan_provision(&self, replicas: u32) -> Planned<ProvisionPlan>;

    /// Side-effecting apply method: runs only during `apply`.
    async fn apply_provision(
        &self,
        plan: Planned<ProvisionPlan>,
    ) -> Result<ProvisionOutcome, AdapterError>;
}

struct FakeExampleAdapter {
    id: AdapterId,
    max_replicas: u32,
    health: HealthStatus,
}

impl FakeExampleAdapter {
    fn new(id: &str, max_replicas: u32) -> Self {
        Self {
            id: AdapterId::new(id),
            max_replicas,
            health: HealthStatus::Healthy,
        }
    }
}

#[async_trait]
impl Adapter for FakeExampleAdapter {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(self.health.clone())
    }
}

impl DescribeCapabilities for FakeExampleAdapter {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl ExampleAdapter for FakeExampleAdapter {
    fn capabilities(&self) -> ExampleCapabilities {
        ExampleCapabilities {
            max_replicas: self.max_replicas,
            supports_teardown: true,
        }
    }

    fn plan_provision(&self, replicas: u32) -> Planned<ProvisionPlan> {
        Planned::new(ProvisionPlan { replicas })
    }

    async fn apply_provision(
        &self,
        plan: Planned<ProvisionPlan>,
    ) -> Result<ProvisionOutcome, AdapterError> {
        let plan = plan.into_inner();
        if plan.replicas > self.max_replicas {
            return Err(ExampleError::CapacityUnavailable {
                requested: plan.replicas,
                limit: self.max_replicas,
            }
            .into_adapter_error());
        }
        Ok(ProvisionOutcome {
            provisioned: plan.replicas,
        })
    }
}

fn registry_with(adapters: Vec<Arc<dyn ExampleAdapter>>) -> Registry<dyn ExampleAdapter> {
    let mut registry = Registry::new();
    for adapter in adapters {
        registry.register(adapter);
    }
    registry
}

#[test]
fn registry_register_get_and_list() {
    let registry = registry_with(vec![
        Arc::new(FakeExampleAdapter::new("primary", 4)),
        Arc::new(FakeExampleAdapter::new("backup", 2)),
    ]);

    assert_eq!(registry.len(), 2);
    let primary = registry
        .get(&AdapterId::new("primary"))
        .expect("registered");
    assert_eq!(primary.id(), AdapterId::new("primary"));
    assert_eq!(primary.capabilities().max_replicas, 4);
    assert!(registry.get(&AdapterId::new("missing")).is_none());
    assert_eq!(registry.list().len(), 2);
}

#[test]
fn registry_discovers_capabilities_without_side_effects() {
    let registry = registry_with(vec![Arc::new(FakeExampleAdapter::new("primary", 4))]);

    let discovered = registry.discover();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].id, AdapterId::new("primary"));
    assert_eq!(discovered[0].capabilities["max_replicas"], 4);
    assert_eq!(discovered[0].capabilities["supports_teardown"], true);
}

#[test]
fn planning_is_pure_and_needs_no_runtime() {
    // No async, no tokio runtime: plan_provision is a pure synchronous fn.
    let adapter = FakeExampleAdapter::new("primary", 4);
    let plan = adapter.plan_provision(3);
    assert_eq!(plan, Planned::new(ProvisionPlan { replicas: 3 }));
}

#[tokio::test]
async fn id_and_health_report_without_side_effects() {
    let adapter = FakeExampleAdapter::new("primary", 4);
    assert_eq!(adapter.id(), AdapterId::new("primary"));
    let health = adapter.health().await.expect("health check");
    assert!(health.is_healthy());
}

#[tokio::test]
async fn apply_succeeds_within_capacity() {
    let adapter = FakeExampleAdapter::new("primary", 4);
    let plan = adapter.plan_provision(4);
    let outcome = adapter
        .apply_provision(plan)
        .await
        .expect("within capacity");
    assert_eq!(outcome.provisioned, 4);
}

#[tokio::test]
async fn apply_over_capacity_yields_classified_retryable_error() {
    let adapter = FakeExampleAdapter::new("primary", 4);
    let plan = adapter.plan_provision(9);
    let err = adapter
        .apply_provision(plan)
        .await
        .expect_err("over capacity");

    assert_eq!(err.code(), catalog::PROVIDER_CAPACITY_UNAVAILABLE);
    assert_eq!(err.code().to_string(), "AMICU-PROVIDER-001");
    assert_eq!(err.severity(), Severity::Error);
    assert!(err.is_retryable());
    // The originating enum is preserved as the error source.
    assert!(std::error::Error::source(&err).is_some());
}

#[test]
fn thiserror_enum_converts_and_classifies() {
    let permanent: AdapterError =
        ExampleError::Misconfigured("no endpoint".into()).into_adapter_error();
    assert_eq!(permanent.code(), catalog::SCHEMA_MISSING_REQUIRED);
    assert_eq!(permanent.severity(), Severity::Error);
    assert!(!permanent.is_retryable());
    assert!(permanent.message().contains("misconfigured"));

    let transient = ExampleError::CapacityUnavailable {
        requested: 5,
        limit: 2,
    }
    .into_adapter_error();
    assert!(transient.is_retryable());
}

#[test]
fn adapter_error_constructors_and_display() {
    let err = AdapterError::transient(catalog::STATE_LOCKED, "state is locked");
    assert!(err.is_retryable());
    assert_eq!(err.to_string(), "[AMICU-STATE-001] state is locked");

    let err = AdapterError::permanent(catalog::MEM_VRAM_EXCEEDED, "model too large");
    assert!(!err.is_retryable());
    assert_eq!(err.severity(), Severity::Error);
}

#[test]
fn adapter_id_and_health_serialize_as_plain_data() {
    let id = AdapterId::new("primary");
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"primary\"");

    let degraded = HealthStatus::Degraded {
        detail: "high latency".into(),
    };
    assert_eq!(
        serde_json::to_value(&degraded).unwrap(),
        serde_json::json!({"status": "degraded", "detail": "high latency"})
    );
    assert!(!degraded.is_healthy());
}
