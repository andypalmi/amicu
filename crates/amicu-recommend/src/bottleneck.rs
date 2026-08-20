//! Diagnosed bottleneck classes.

use serde::{Deserialize, Serialize};

/// The class of bottleneck a diagnosis assigns to a stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BottleneckClass {
    /// Saturated: high queue wait, high GPU util, KV evictions.
    UnderProvisioned,
    /// Idle burn: low GPU util at cost.
    OverProvisioned,
    /// Prompt processing dominates: high TTFT, healthy TPOT.
    PrefillBound,
    /// Token generation dominates: high TPOT.
    DecodeBound,
    /// KV pressure forcing preemption/recompute.
    PreemptionRecompute,
}

impl BottleneckClass {
    /// A stable, human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            BottleneckClass::UnderProvisioned => "under-provisioned",
            BottleneckClass::OverProvisioned => "over-provisioned",
            BottleneckClass::PrefillBound => "prefill-bound",
            BottleneckClass::DecodeBound => "decode-bound",
            BottleneckClass::PreemptionRecompute => "preemption/recompute",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&BottleneckClass::PreemptionRecompute).unwrap(),
            "\"preemption_recompute\""
        );
    }

    #[test]
    fn labels_are_distinct() {
        let classes = [
            BottleneckClass::UnderProvisioned,
            BottleneckClass::OverProvisioned,
            BottleneckClass::PrefillBound,
            BottleneckClass::DecodeBound,
            BottleneckClass::PreemptionRecompute,
        ];
        let labels: std::collections::BTreeSet<&str> = classes.iter().map(|c| c.label()).collect();
        assert_eq!(labels.len(), classes.len());
    }
}
