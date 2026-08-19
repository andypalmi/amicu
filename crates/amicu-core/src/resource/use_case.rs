use serde::{Deserialize, Serialize};

use super::Modality;
use crate::id::{DataClassId, UseCaseId};

/// Requirement metadata used to drive presets and recommendations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UseCase {
    pub id: UseCaseId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modalities: Vec<Modality>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_context_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_latency_ms: Option<u32>,
    /// Data class the workload must satisfy, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_data_class: Option<DataClassId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
}
