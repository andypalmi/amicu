//! The recorded canonical state and its history entries.

use std::hash::{Hash, Hasher};

use amicu_core::Graph;
use serde::{Deserialize, Serialize};

use crate::error::StateError;

/// A recorded snapshot of the canonical [`Graph`] plus the metadata that
/// identifies it and guards its integrity.
///
/// The `checksum` is computed over the graph at construction; [`verify`] recomputes
/// it and reports [`StateError::Corrupted`] on mismatch, so tampering is caught
/// before the snapshot is trusted.
///
/// [`verify`]: StateSnapshot::verify
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Version id this snapshot was written under.
    pub version: String,
    /// Wall-clock marker of when the snapshot was recorded, as an opaque field.
    pub recorded_at: String,
    /// Integrity checksum over [`graph`](StateSnapshot::graph).
    pub checksum: String,
    /// The recorded canonical resource graph.
    pub graph: Graph,
}

impl StateSnapshot {
    /// Record `graph` under `version` at `recorded_at`, stamping a fresh checksum.
    #[must_use]
    pub fn new(version: impl Into<String>, recorded_at: impl Into<String>, graph: Graph) -> Self {
        let checksum = checksum_of(&graph);
        Self {
            version: version.into(),
            recorded_at: recorded_at.into(),
            checksum,
            graph,
        }
    }

    /// Recompute the checksum and confirm it matches the recorded one.
    ///
    /// # Errors
    /// Returns [`StateError::Corrupted`] when the graph no longer hashes to the
    /// stored checksum, signalling the snapshot needs manual recovery.
    pub fn verify(&self) -> Result<(), StateError> {
        let actual = checksum_of(&self.graph);
        if actual == self.checksum {
            Ok(())
        } else {
            Err(StateError::Corrupted {
                detail: format!(
                    "checksum mismatch for version {} (expected {}, computed {actual})",
                    self.version, self.checksum
                ),
            })
        }
    }
}

/// A single history entry: an id, its parent, and a human summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateVersion {
    /// Stable id of this version.
    pub id: String,
    /// Id of the version this one succeeded, if any.
    pub parent: Option<String>,
    /// Short human-readable summary of what this version recorded.
    pub label: String,
}

impl StateVersion {
    /// Construct a history entry.
    #[must_use]
    pub fn new(id: impl Into<String>, parent: Option<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            parent,
            label: label.into(),
        }
    }
}

/// Deterministic checksum over a graph's canonical JSON serialization.
fn checksum_of(graph: &Graph) -> String {
    let json = serde_json::to_string(graph).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    json.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
