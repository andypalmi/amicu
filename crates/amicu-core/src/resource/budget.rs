use serde::{Deserialize, Serialize};

use super::Money;
use crate::id::{BudgetId, TenantId};

/// Cost allocation for a tenant: a spend limit over a period plus alert
/// thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub id: BudgetId,
    /// Tenant this budget is attributed to, if scoped to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<TenantId>,
    pub limit: Money,
    pub period: BudgetPeriod,
    /// Spend fractions (0.0..=1.0) at which to raise alerts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alert_thresholds: Vec<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    Daily,
    Weekly,
    Monthly,
}
