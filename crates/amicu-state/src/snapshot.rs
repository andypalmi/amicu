//! The recorded canonical state and its history entries.
//!
//! # Secret-refs-by-type
//!
//! [`StateSnapshot`] wraps [`Graph`] directly and [`checksum_of`] serializes it
//! to canonical JSON for persistence and integrity checking. Because
//! `#[derive(Serialize)]` requires every field of a serialized type to
//! implement `Serialize`, and [`SecretValue`](amicu_core::secret::SecretValue)
//! deliberately does not, no field anywhere inside [`Graph`] can ever hold a
//! `SecretValue` without breaking the build - the whole crate simply fails to
//! compile. A secret-bearing field in state must instead be typed
//! [`SecretRef`](amicu_core::secret::SecretRef) (a pointer, safe to persist);
//! the plaintext lives only in a `SecretResolver` at apply time, never here.
//! See `tests/compile_fail/` for a guard that proves this holds.

use std::fmt::Write as _;

use amicu_core::Graph;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::StateError;
use crate::runtime::RuntimeState;

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
    /// Per-deployment runtime records (handles, endpoints, write-ahead
    /// pending records). Absent in snapshots written before this field
    /// existed, which read back as empty.
    #[serde(default)]
    pub runtime: RuntimeState,
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
        Self::with_runtime(version, recorded_at, graph, RuntimeState::default())
    }

    /// Record `graph` and its per-deployment `runtime` records under `version`
    /// at `recorded_at`, stamping a fresh checksum covering both.
    ///
    /// # Errors
    /// Returns [`StateError::Corrupted`] if the contents cannot be serialized
    /// for checksumming.
    pub fn with_runtime(
        version: impl Into<String>,
        recorded_at: impl Into<String>,
        graph: Graph,
        runtime: RuntimeState,
    ) -> Result<Self, StateError> {
        let checksum = checksum_of(&graph, &runtime)?;
        Ok(Self {
            version: version.into(),
            recorded_at: recorded_at.into(),
            checksum,
            graph,
            runtime,
        })
    }

    /// Recompute the checksum and confirm it matches the recorded one.
    ///
    /// # Errors
    /// Returns [`StateError::Corrupted`] when the graph no longer hashes to the
    /// stored checksum, or cannot be serialized for checksumming, signalling the
    /// snapshot needs manual recovery.
    pub fn verify(&self) -> Result<(), StateError> {
        // The stored tag decides how the snapshot was hashed; recompute the
        // *expected* scheme so a runtime section smuggled into (or stripped
        // from) a snapshot cannot hide behind a v1 tag.
        let actual = if self.checksum.starts_with(CHECKSUM_V2_PREFIX) {
            checksum_v2(&self.graph, &self.runtime)?
        } else {
            if !self.runtime.is_empty() {
                return Err(StateError::Corrupted {
                    detail: format!(
                        "version {} carries runtime records but a graph-only ({CHECKSUM_V1}) checksum",
                        self.version
                    ),
                });
            }
            checksum_v1(&self.graph)?
        };
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

/// Algorithm tag of the original, graph-only checksum.
const CHECKSUM_V1: &str = "sha256-v1";
/// Algorithm tag of the checksum covering graph and runtime records.
const CHECKSUM_V2: &str = "sha256-v2";
const CHECKSUM_V2_PREFIX: &str = "sha256-v2:";

/// Checksum for a snapshot's contents. The scheme is versioned: a snapshot
/// without runtime records keeps the original graph-only `sha256-v1` hash
/// (byte-identical to every snapshot written before runtime records existed,
/// so old rows keep verifying and old binaries can still read runtime-free
/// state), while one with runtime records uses `sha256-v2` over
/// `{"graph":..,"runtime":..}`.
fn checksum_of(graph: &Graph, runtime: &RuntimeState) -> Result<String, StateError> {
    if runtime.is_empty() {
        checksum_v1(graph)
    } else {
        checksum_v2(graph, runtime)
    }
}

/// Stable content hash over a graph's canonical JSON serialization.
///
/// SHA-256 is a fixed, cross-toolchain algorithm, so the returned checksum is
/// suitable for persistence: a graph rehashes identically on any build. The
/// graph serializes deterministically (its collections are `BTreeMap`/`Vec`, no
/// `HashMap`). The result is `"sha256-v1:{hex}"`.
///
/// # Errors
/// Returns [`StateError::Corrupted`] if the graph fails to serialize. The error
/// is surfaced rather than swallowed: hashing an empty fallback string would let
/// every failing graph collide on one "valid" checksum.
fn checksum_v1(graph: &Graph) -> Result<String, StateError> {
    let json = serde_json::to_string(graph).map_err(|source| StateError::Corrupted {
        detail: format!("graph failed to serialize for checksumming: {source}"),
    })?;
    Ok(hex_digest(CHECKSUM_V1, &json))
}

fn checksum_v2(graph: &Graph, runtime: &RuntimeState) -> Result<String, StateError> {
    #[derive(Serialize)]
    struct Canonical<'a> {
        graph: &'a Graph,
        runtime: &'a RuntimeState,
    }
    let json = serde_json::to_string(&Canonical { graph, runtime }).map_err(|source| {
        StateError::Corrupted {
            detail: format!("snapshot failed to serialize for checksumming: {source}"),
        }
    })?;
    Ok(hex_digest(CHECKSUM_V2, &json))
}

fn hex_digest(tag: &str, json: &str) -> String {
    let digest = Sha256::digest(json.as_bytes());
    let mut checksum = format!("{tag}:");
    for byte in digest {
        write!(checksum, "{byte:02x}").expect("writing hex to a String is infallible");
    }
    checksum
}
