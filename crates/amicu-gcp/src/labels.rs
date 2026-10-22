//! The owner marker amicu puts on every GCP resource it creates, as labels, and
//! the check that a resource carries it before amicu changes or deletes it.
//!
//! GCP label keys and values are at most 63 characters of `[a-z0-9_-]` (keys
//! start with a lowercase letter). The marker is not a secret: a digest of the
//! workspace (empty when unknown), project and deployment ids, so `start` and
//! `stop` can tell their own resource from another one with the same name.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use amicu_core::naming::project_app_name;
use sha2::{Digest, Sha256};

use crate::error::GcpError;

/// The label key holding the owner marker.
pub const OWNER_LABEL: &str = "amicu-owner";

/// The label key holding the readable deployment name.
const DEPLOYMENT_LABEL: &str = "amicu-deployment";

const MAX_LABEL_LEN: usize = 63;

/// The owner marker for a deployment: 32 hex digits of a domain-separated
/// SHA-256 over the workspace id (empty when unknown), project id and
/// deployment id.
#[must_use]
pub fn owner_marker(workspace_id: Option<&str>, project_id: &str, deployment_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"amicu-gcp-owner/v1");
    for part in [workspace_id.unwrap_or(""), project_id, deployment_id] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    let mut hex = String::with_capacity(32);
    for b in h.finalize().iter().take(16) {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// A valid label value from arbitrary text: lowercased, every character outside
/// `[a-z0-9_-]` replaced with `-`, cut to 63 characters. May be empty.
#[must_use]
pub fn sanitize_label_value(raw: &str) -> String {
    raw.chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(MAX_LABEL_LEN)
        .collect()
}

/// The labels to put on a deployment's resources: the [`owner_marker`] and the
/// readable name from [`project_app_name`] (the same name the other backends
/// use for the deployment).
#[must_use]
pub fn owner_labels(
    workspace_id: Option<&str>,
    project_id: &str,
    deployment_id: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            OWNER_LABEL.to_owned(),
            owner_marker(workspace_id, project_id, deployment_id),
        ),
        (
            DEPLOYMENT_LABEL.to_owned(),
            sanitize_label_value(&project_app_name(project_id, deployment_id)),
        ),
    ])
}

/// Whether `labels` (as GCP returns them; `None` when the resource has none)
/// carry exactly the owner marker `expected`.
#[must_use]
pub fn has_owner_marker(labels: Option<&BTreeMap<String, String>>, expected: &str) -> bool {
    labels
        .and_then(|l| l.get(OWNER_LABEL))
        .is_some_and(|v| v == expected)
}

/// Passes when the resource carries the owner marker `expected`.
///
/// # Errors
/// [`GcpError::NotOwned`] (naming `what`) otherwise: amicu must not touch it.
pub fn ensure_owned(
    what: &str,
    labels: Option<&BTreeMap<String, String>>,
    expected: &str,
) -> Result<(), GcpError> {
    if has_owner_marker(labels, expected) {
        Ok(())
    } else {
        Err(GcpError::NotOwned {
            what: what.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(s: &str) -> bool {
        s.len() <= MAX_LABEL_LEN
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    }

    #[test]
    fn sanitising_lowercases_replaces_and_truncates() {
        assert_eq!(sanitize_label_value("My App.v2/é"), "my-app-v2--");
        assert_eq!(sanitize_label_value("a_b-c9"), "a_b-c9");
        let long = sanitize_label_value(&"X".repeat(100));
        assert_eq!(long.len(), 63);
        assert!(valid(&long));
        assert_eq!(sanitize_label_value(""), "");
    }

    #[test]
    fn owner_labels_are_valid_gcp_labels() {
        let labels = owner_labels(Some("ws"), "My Project!", "Prod Deployment/1");
        assert_eq!(labels.len(), 2);
        for (k, v) in &labels {
            assert!(
                valid(k) && k.starts_with(|c: char| c.is_ascii_lowercase()),
                "{k}"
            );
            assert!(valid(v), "{v}");
        }
        assert_eq!(
            labels[OWNER_LABEL],
            owner_marker(Some("ws"), "My Project!", "Prod Deployment/1")
        );
    }

    #[test]
    fn marker_is_stable_and_separates_workspace_project_and_deployment() {
        let a = owner_marker(Some("w"), "p", "d");
        assert_eq!(a, owner_marker(Some("w"), "p", "d"));
        assert_eq!(a.len(), 32);
        assert_ne!(a, owner_marker(None, "p", "d"));
        assert_ne!(a, owner_marker(Some("w"), "p", "e"));
        // Length-prefixed parts: shifting a boundary changes the digest.
        assert_ne!(owner_marker(None, "ab", "c"), owner_marker(None, "a", "bc"));
    }

    #[test]
    fn marker_check_requires_the_exact_value() {
        let mine = owner_labels(None, "p", "d");
        let marker = owner_marker(None, "p", "d");
        assert!(has_owner_marker(Some(&mine), &marker));
        assert!(ensure_owned("instance x", Some(&mine), &marker).is_ok());

        let theirs = owner_labels(None, "p", "other");
        assert!(!has_owner_marker(Some(&theirs), &marker));
        assert!(!has_owner_marker(None, &marker));
        let unlabeled = BTreeMap::from([("env".to_owned(), "prod".to_owned())]);
        let err = ensure_owned("instance x", Some(&unlabeled), &marker).unwrap_err();
        assert_eq!(
            err,
            GcpError::NotOwned {
                what: "instance x".into()
            }
        );
    }
}
