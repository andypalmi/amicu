//! Privacy-safe request logging.
//!
//! A [`RequestRecord`] records only *metadata* about an inference request:
//! when it happened, which deployment/model served it, token **counts**,
//! latency, and an outcome. It follows the same leak-prevention-by-omission
//! philosophy as [`SecretValue`](amicu_core::secret::SecretValue): the guard is
//! not a redaction step that could be forgotten, but the plain fact that the
//! prompt and completion text have *nowhere to live*. There is no field of any
//! type that can hold request or response content, and no constructor or setter
//! that accepts it. See [`RequestRecord`] for the enforced invariant.
//!
//! Emission is governed by a [`LoggingPolicy`]. Content is never a logging
//! option at any level — the only choice is whether metadata is emitted at all.

use std::sync::Mutex;

use amicu_core::id::{DeploymentId, ModelId};
use serde::{Deserialize, Serialize};

/// The terminal outcome of a request. Carries no free-form content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Completed successfully.
    Success,
    /// Failed (upstream/runtime error). The reason is not recorded here to keep
    /// the record content-free; classify errors via metrics/diagnostics instead.
    Error,
    /// Canceled or timed out before completion.
    Canceled,
}

/// Content-free metadata about a single inference request.
///
/// # Privacy invariant
/// By construction this type cannot retain prompt or completion text. Every
/// field is either an identifier ([`DeploymentId`]/[`ModelId`] — a name, not
/// content, exactly as [`SecretRef`](amicu_core::secret::SecretRef) is), a
/// numeric count/measurement, or the [`Outcome`] enum. There is deliberately
/// **no `String`/`Vec<u8>`/free-text field** and no API that accepts request or
/// response bodies. A round-trip test asserts the serialized key set to keep
/// this guarantee from regressing as fields are added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Caller-supplied wall-clock timestamp (ms since epoch). Passed in rather
    /// than read from a clock so records stay deterministic and testable.
    pub timestamp_ms: u64,
    /// Deployment that served the request.
    pub deployment: DeploymentId,
    /// Model the request targeted, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelId>,
    /// Number of prompt tokens (count only — never the prompt itself).
    pub prompt_tokens: u32,
    /// Number of generated tokens (count only — never the completion itself).
    pub completion_tokens: u32,
    /// End-to-end request latency, in milliseconds.
    pub latency_ms: f64,
    /// Terminal outcome.
    pub outcome: Outcome,
}

impl RequestRecord {
    /// A successful request record from counts and metadata.
    #[must_use]
    pub fn new(
        timestamp_ms: u64,
        deployment: DeploymentId,
        prompt_tokens: u32,
        completion_tokens: u32,
        latency_ms: f64,
    ) -> Self {
        Self {
            timestamp_ms,
            deployment,
            model: None,
            prompt_tokens,
            completion_tokens,
            latency_ms,
            outcome: Outcome::Success,
        }
    }

    /// Set the model this request targeted.
    #[must_use]
    pub fn with_model(mut self, model: ModelId) -> Self {
        self.model = Some(model);
        self
    }

    /// Set the terminal outcome.
    #[must_use]
    pub fn with_outcome(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// Total tokens (prompt + completion).
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        u64::from(self.prompt_tokens) + u64::from(self.completion_tokens)
    }
}

/// How much request metadata to emit. Content is never an option at any level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Emit nothing.
    Off,
    /// Emit content-free [`RequestRecord`] metadata.
    Metadata,
}

/// Controls whether request metadata is logged.
///
/// Defaults to [`LogLevel::Metadata`]: privacy-preserving by construction, since
/// no level can ever emit prompt or completion content — the strongest setting,
/// [`LogLevel::Off`], only suppresses the metadata as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggingPolicy {
    pub level: LogLevel,
}

impl Default for LoggingPolicy {
    fn default() -> Self {
        Self {
            level: LogLevel::Metadata,
        }
    }
}

impl LoggingPolicy {
    /// A policy that emits content-free metadata.
    #[must_use]
    pub fn metadata() -> Self {
        Self {
            level: LogLevel::Metadata,
        }
    }

    /// A policy that emits nothing.
    #[must_use]
    pub fn off() -> Self {
        Self {
            level: LogLevel::Off,
        }
    }

    /// Whether a metadata record should be emitted under this policy.
    #[must_use]
    pub fn should_emit(&self) -> bool {
        matches!(self.level, LogLevel::Metadata)
    }
}

/// Thread-safe buffer of emitted [`RequestRecord`]s, gated by a [`LoggingPolicy`].
///
/// Because it stores `RequestRecord`s, the buffer inherits their content-free
/// guarantee: there is no path by which prompt or completion text can be
/// retained here. Under [`LogLevel::Off`], [`log`](Self::log) is a no-op.
#[derive(Debug, Default)]
pub struct RequestLog {
    policy: LoggingPolicy,
    records: Mutex<Vec<RequestRecord>>,
}

impl RequestLog {
    /// A log with the given emission policy.
    #[must_use]
    pub fn new(policy: LoggingPolicy) -> Self {
        Self {
            policy,
            records: Mutex::new(Vec::new()),
        }
    }

    /// Record metadata if the policy permits; returns whether it was retained.
    pub fn log(&self, record: RequestRecord) -> bool {
        if !self.policy.should_emit() {
            return false;
        }
        self.records.lock().expect("log lock poisoned").push(record);
        true
    }

    /// Number of retained records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.lock().expect("log lock poisoned").len()
    }

    /// Whether no records are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot the retained records.
    #[must_use]
    pub fn records(&self) -> Vec<RequestRecord> {
        self.records.lock().expect("log lock poisoned").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_holds_only_counts_and_metadata() {
        let record = RequestRecord::new(
            1_700_000_000_000,
            DeploymentId::new("chat_local"),
            128,
            64,
            42.0,
        )
        .with_model(ModelId::new("support_chat"));

        let value = serde_json::to_value(&record).unwrap();
        let keys: std::collections::BTreeSet<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        // Exactly the metadata fields — no prompt/completion/content of any kind.
        let expected: std::collections::BTreeSet<&str> = [
            "timestamp_ms",
            "deployment",
            "model",
            "prompt_tokens",
            "completion_tokens",
            "latency_ms",
            "outcome",
        ]
        .into_iter()
        .collect();
        assert_eq!(keys, expected);
        assert_eq!(record.total_tokens(), 192);
    }

    #[test]
    fn default_policy_is_privacy_preserving_metadata() {
        assert_eq!(LoggingPolicy::default().level, LogLevel::Metadata);
        assert!(LoggingPolicy::default().should_emit());
        assert!(!LoggingPolicy::off().should_emit());
    }

    #[test]
    fn request_log_gates_on_policy() {
        let record = RequestRecord::new(1, DeploymentId::new("chat_local"), 8, 4, 10.0);

        let on = RequestLog::new(LoggingPolicy::metadata());
        assert!(on.log(record.clone()));
        assert_eq!(on.len(), 1);
        assert_eq!(on.records()[0], record);

        let off = RequestLog::new(LoggingPolicy::off());
        assert!(!off.log(record));
        assert!(off.is_empty());
    }

    #[test]
    fn outcome_round_trips() {
        for outcome in [Outcome::Success, Outcome::Error, Outcome::Canceled] {
            let json = serde_json::to_string(&outcome).unwrap();
            assert_eq!(serde_json::from_str::<Outcome>(&json).unwrap(), outcome);
        }
    }
}
