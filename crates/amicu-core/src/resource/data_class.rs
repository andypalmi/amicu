use serde::{Deserialize, Serialize};

use crate::id::DataClassId;

/// Classifies data by sensitivity and the residency / handling / routing
/// constraints that follow from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataClass {
    pub id: DataClassId,
    pub sensitivity: Sensitivity,
    /// Allowed residency regions; empty means unrestricted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub residency: Vec<String>,
    /// Whether data of this class may leave first-party infrastructure.
    #[serde(default)]
    pub allow_external_providers: bool,
    /// Free-form handling constraint tags, e.g. `"no_logging"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handling: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Internal,
    Confidential,
    Restricted,
}
