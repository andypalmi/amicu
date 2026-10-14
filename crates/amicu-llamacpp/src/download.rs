//! Resumable GGUF download on top of the registry's [`ModelSourceAdapter`].
//!
//! The source owns resume/retry semantics: a transfer that stops partway leaves
//! a partial file, and the next [`download`](ModelSourceAdapter::download) call
//! continues from that offset. This retries transient failures, letting the
//! source resume rather than restart.

use std::path::Path;

use amicu_core::id::ModelId;
use amicu_registry::{ArtifactSelector, DownloadResult, ModelSourceAdapter};

use crate::error::LlamaCppError;

/// Default number of download attempts before giving up.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Resolve and download a model's GGUF artifact to `dest`, resuming across
/// transient failures for up to `max_attempts` tries.
///
/// `expected_sha256`, when `Some` (the `GgufVariant.sha256`
/// declared on the model), is checked against the resolved artifact's own
/// `checksum` (an optional case-insensitive `"sha256:"` prefix is stripped
/// before comparing) *before* any bytes are fetched - a mismatch fails fast
/// as [`LlamaCppError::ChecksumMismatch`] rather than downloading a file
/// already known to be wrong. `None` skips verification entirely, exactly
/// today's behavior.
///
/// # Errors
/// [`LlamaCppError::DownloadFailed`] when a non-retryable error occurs or the
/// attempt budget is exhausted; capability/memory errors from artifact
/// resolution surface with their original message;
/// [`LlamaCppError::ChecksumMismatch`] when `expected_sha256` is `Some` and
/// does not match the resolved artifact's checksum.
pub async fn download_gguf(
    source: &dyn ModelSourceAdapter,
    model: &ModelId,
    selector: &ArtifactSelector,
    dest: &Path,
    max_attempts: u32,
    expected_sha256: Option<&str>,
) -> Result<DownloadResult, LlamaCppError> {
    let artifact = source
        .resolve_artifact(model, selector)
        .await
        .map_err(|e| LlamaCppError::DownloadFailed(e.message().to_string()))?;

    if let Some(expected) = expected_sha256 {
        let actual = strip_sha256_prefix(&artifact.checksum);
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(LlamaCppError::ChecksumMismatch {
                expected: expected.to_owned(),
                actual: actual.to_owned(),
            });
        }
    }

    let attempts = max_attempts.max(1);
    let mut last = String::new();
    for _ in 0..attempts {
        match source.download(&artifact, dest).await {
            Ok(result) => return Ok(result),
            Err(e) if e.is_retryable() => last = e.message().to_string(),
            // Non-retryable: a partial file will not be salvaged by retrying.
            Err(e) => return Err(LlamaCppError::DownloadFailed(e.message().to_string())),
        }
    }
    Err(LlamaCppError::DownloadFailed(format!(
        "exhausted {attempts} attempts: {last}"
    )))
}

/// Strip a case-insensitive `"sha256:"` prefix from an [`ArtifactRef`]-style
/// checksum, if present; otherwise return `checksum` unchanged.
///
/// [`ArtifactRef`]: amicu_registry::ArtifactRef
fn strip_sha256_prefix(checksum: &str) -> &str {
    const PREFIX_LEN: usize = "sha256:".len();
    if checksum.len() >= PREFIX_LEN && checksum[..PREFIX_LEN].eq_ignore_ascii_case("sha256:") {
        &checksum[PREFIX_LEN..]
    } else {
        checksum
    }
}
