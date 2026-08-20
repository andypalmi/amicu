//! Product-tier boundary for recommendations.
//!
//! Basic monitoring — the [`StackMetrics`](amicu_telemetry::StackMetrics)
//! surface — is [`Tier::Community`]. Advisory tuning recommendations diagnosed
//! against estimates and the performance DB are [`Tier::Pro`]. This marks the
//! boundary in the types; it does not implement billing.

use serde::{Deserialize, Serialize};

/// A product tier, ordered by capability (`Community` < `Pro` < `Enterprise` <
/// `HostedCloud`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Community,
    Pro,
    Enterprise,
    HostedCloud,
}

impl Tier {
    /// The tier that gates basic metric monitoring.
    pub const MONITORING: Tier = Tier::Community;
    /// The tier that gates advisory tuning recommendations.
    pub const RECOMMENDATIONS: Tier = Tier::Pro;

    /// Whether this (held) tier unlocks capability gated at `required`.
    #[must_use]
    pub fn unlocks(self, required: Tier) -> bool {
        self >= required
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_capability_ascending() {
        assert!(Tier::Community < Tier::Pro);
        assert!(Tier::Pro < Tier::Enterprise);
        assert!(Tier::Enterprise < Tier::HostedCloud);
    }

    #[test]
    fn unlocks_is_inclusive_upward() {
        assert!(Tier::Pro.unlocks(Tier::RECOMMENDATIONS));
        assert!(Tier::Enterprise.unlocks(Tier::RECOMMENDATIONS));
        assert!(!Tier::Community.unlocks(Tier::RECOMMENDATIONS));
        assert!(Tier::Community.unlocks(Tier::MONITORING));
    }

    #[test]
    fn serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Tier::HostedCloud).unwrap(),
            "\"hosted_cloud\""
        );
    }
}
