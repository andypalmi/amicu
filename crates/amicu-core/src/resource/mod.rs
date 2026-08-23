//! Faithful Rust representations of the canonical DSL resource kinds: the
//! original nine plus `credential` (amicu-planning#119).
//!
//! Every type derives `Serialize`, `Deserialize`, `Debug`, `Clone` and
//! `PartialEq`, uses `snake_case` field names to match the DSL/JSON, and
//! models constrained values as enums.

use serde::{Deserialize, Serialize};

mod budget;
mod credential;
mod data_class;
mod deployment;
mod governance;
mod hardware;
mod model;
mod region;
mod routing;
mod tenant;
mod use_case;

pub use budget::{Budget, BudgetPeriod};
pub use credential::Credential;
pub use data_class::{DataClass, Sensitivity};
pub use deployment::{Deployment, LlamaCppSettings, Runtime, VllmSettings};
pub use governance::{GovernanceRule, RuleEffect};
pub use hardware::{Accelerator, AcceleratorKind, Cpu, HardwareProfile, Interconnect, Price};
pub use model::{
    Artifact, ArtifactFormat, GgufVariant, Model, ModelSource, Quantization, SourceRegistry,
};
pub use region::{Region, Residency};
pub use routing::{FailoverPolicy, OptimizeFor, RoutingPolicy, RoutingTarget};
pub use tenant::{Quota, Tenant};
pub use use_case::{Priority, UseCase};

/// An input or output modality a model can handle or a use case requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Vision,
    Audio,
    Embedding,
}

/// A monetary amount in a given ISO currency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Money {
    pub amount: f64,
    /// ISO 4217 currency code, e.g. `"USD"`.
    pub currency: String,
}

impl Money {
    #[must_use]
    pub fn new(amount: f64, currency: impl Into<String>) -> Self {
        Self {
            amount,
            currency: currency.into(),
        }
    }
}
