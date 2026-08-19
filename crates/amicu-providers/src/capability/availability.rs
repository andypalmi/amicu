//! Provider / region capacity availability state.

use serde::{Deserialize, Serialize};

/// Capacity availability of a provider or offering, feeding later
/// `AMICU-PROVIDER-*` availability diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvailabilityState {
    /// Capacity is available for provisioning.
    Available,
    /// Capacity is constrained; provisioning may be delayed or partial.
    Limited,
    /// No capacity is currently available.
    Unavailable,
    /// Availability could not be determined.
    Unknown,
}

/// Availability of an offering, with an optional safe capacity note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Availability {
    /// The capacity state.
    pub state: AvailabilityState,
    /// Safe, secret-free capacity detail, e.g. `"spot only"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Availability {
    /// Availability in the given state with no detail.
    #[must_use]
    pub fn new(state: AvailabilityState) -> Self {
        Self {
            state,
            detail: None,
        }
    }

    /// Attach a safe, secret-free capacity note.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Whether capacity is available for provisioning.
    #[must_use]
    pub fn is_available(&self) -> bool {
        matches!(self.state, AvailabilityState::Available)
    }
}
