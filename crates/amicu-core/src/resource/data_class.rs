use serde::{Deserialize, Serialize};

use crate::id::DataClassId;
use crate::resource::Residency;

/// Classifies data by sensitivity and the residency / handling / routing
/// constraints that follow from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataClass {
    pub id: DataClassId,
    pub sensitivity: Sensitivity,
    /// Allowed residency jurisdictions (e.g. [`Residency::Eu`]); empty means
    /// unrestricted. A deployment whose hardware region resolves to a
    /// jurisdiction outside this set is rejected at plan time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub residency: Vec<Residency>,
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
