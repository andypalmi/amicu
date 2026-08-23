//! The recorded canonical state and its history entries.

use std::fmt::Write as _;

use amicu_core::Graph;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::StateError;

/// A recorded snapshot of the canonical [`Graph`] plus the metadata that
/// identifies it and guards its integrity.
///
/// The `checksum` is a stable, versioned content hash: SHA-256 over the graph's
/// canonical JSON, prefixed with the algorithm tag (see [`checksum_of`]). Because
/// it is a fixed algorithm rather than a toolchain-dependent hasher, a snapshot
/// persisted under one Rust build recomputes to the same checksum under another,
/// so [`verify`] only fails on genuine tampering - never spuriously across builds.
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
    ///
    /// # Errors
    /// Returns [`StateError::Corrupted`] if `graph` cannot be serialized for
    /// checksumming (see [`checksum_of`]).
    pub fn new(
        version: impl Into<String>,
        recorded_at: impl Into<String>,
        graph: Graph,
    ) -> Result<Self, StateError> {
        let checksum = checksum_of(&graph)?;
        Ok(Self {
            version: version.into(),
            recorded_at: recorded_at.into(),
            checksum,
            graph,
        })
    }

    /// Recompute the checksum and confirm it matches the recorded one.
    ///
    /// # Errors
    /// Returns [`StateError::Corrupted`] when the graph no longer hashes to the
    /// stored checksum, or cannot be serialized for checksumming, signalling the
    /// snapshot needs manual recovery.
    pub fn verify(&self) -> Result<(), StateError> {
        let actual = checksum_of(&self.graph)?;
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

/// Algorithm tag prefixing every checksum, so the hash scheme is versioned and a
/// future change stays distinguishable from an integrity failure.
const CHECKSUM_ALGORITHM: &str = "sha256-v1";

/// Stable content hash over a graph's canonical JSON serialization.
///
/// SHA-256 is a fixed, cross-toolchain algorithm, so the returned checksum is
/// suitable for persistence: a graph rehashes identically on any build. The
/// graph serializes deterministically (its collections are `BTreeMap`/`Vec`, no
/// `HashMap`). The result is `"{CHECKSUM_ALGORITHM}:{hex}"`.
///
/// # Errors
/// Returns [`StateError::Corrupted`] if the graph fails to serialize. The error
/// is surfaced rather than swallowed: hashing an empty fallback string would let
/// every failing graph collide on one "valid" checksum.
fn checksum_of(graph: &Graph) -> Result<String, StateError> {
    let json = serde_json::to_string(graph).map_err(|source| StateError::Corrupted {
        detail: format!("graph failed to serialize for checksumming: {source}"),
    })?;
    let digest = Sha256::digest(json.as_bytes());
    let mut checksum = format!("{CHECKSUM_ALGORITHM}:");
    for byte in digest {
        write!(checksum, "{byte:02x}").expect("writing hex to a String is infallible");
    }
    Ok(checksum)
}
