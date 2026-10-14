//! The opt-in telemetry payload: a versioned, aggregates-only, content-free
//! report.
//!
//! # What is collected
//!
//! One [`TelemetryReport`] describes **one deployment over one time window**
//! (e.g. five minutes). It carries only rollups - never per-request records:
//!
//! | Section | Fields |
//! |---|---|
//! | envelope | `schema_version`, `install_id`, `deployment_id` (anonymous, not the deployment name), `window` (`start_unix_s`, `duration_s`) |
//! | `hardware` | `accelerator` (model label), `accelerator_count`, `vram_bytes`, `cpu_cores`, `memory_bytes` |
//! | `model` | `name`, `quantization` |
//! | `runtime` | `name`, `version` |
//! | placement | `provider`, `region` |
//! | `metrics` | `requests`, `errors`, `latency_e2e_ms` and `latency_ttft_ms` (`p50`/`p95`/`p99`), `output_tokens_per_s`, `requests_per_s`, `memory_used_bytes`, `kv_cache_utilization`, `gpu_utilization`, `cost_per_hour_eur` |
//!
//! # What can never be represented
//!
//! Prompts, completions, secrets and IP addresses have no field. The payload is
//! an allow-list enforced three ways:
//!
//! 1. **By construction**: every field is a number, an id, a [`Label`], or a
//!    nested struct of those. There is no free-form `String`, `Value`, `Map` or
//!    "extra" bag.
//! 2. **By strict decoding**: every struct is `deny_unknown_fields`, so a
//!    payload with any unlisted field is rejected, not ignored.
//! 3. **By value validation**: a [`Label`] is a short token over
//!    `[A-Za-z0-9._+/-]` (no whitespace, so a sentence cannot fit) and is
//!    rejected if it contains something shaped like an IP address;
//!    [`TelemetryReport::validate`] bounds every number.
//!
//! Server-side ingestion must additionally not persist the transport peer
//! address; that is outside this data model.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::identity::{AnonDeploymentId, InstallId};

/// Current schema version of [`TelemetryReport`]. Bump on any field change.
pub const TELEMETRY_SCHEMA_VERSION: u32 = 1;

/// Longest allowed window (one day), in seconds.
pub const MAX_WINDOW_SECONDS: u32 = 86_400;

/// Longest allowed [`Label`].
pub const MAX_LABEL_LEN: usize = 64;

/// Earliest plausible window start (2024-01-01T00:00:00Z).
pub const MIN_START_UNIX_S: u64 = 1_704_067_200;
/// Latest plausible window start (2100-01-01T00:00:00Z).
pub const MAX_START_UNIX_S: u64 = 4_102_444_800;
/// Most requests one deployment can plausibly serve in one window.
pub const MAX_REQUESTS: u64 = 10_000_000_000;
/// Largest plausible byte quantity (1 PiB).
pub const MAX_BYTES: u64 = 1 << 50;
/// Largest plausible per-second rate (tokens or requests).
pub const MAX_RATE: f64 = 10_000_000.0;
/// Largest plausible latency, ms (one day).
pub const MAX_LATENCY_MS: f64 = 86_400_000.0;
/// Largest plausible cost per hour, EUR.
pub const MAX_COST_PER_HOUR_EUR: f64 = 1_000_000.0;
/// Most accelerators one deployment can plausibly use.
pub const MAX_ACCELERATORS: u32 = 1_024;
/// Most CPU cores one host can plausibly have.
pub const MAX_CPU_CORES: u32 = 65_536;

/// Why a report or label was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// `schema_version` is not [`TELEMETRY_SCHEMA_VERSION`].
    UnsupportedSchema(u32),
    /// A label is empty, too long, or has a disallowed character.
    BadLabel(String),
    /// A label looks like an IP address.
    IpLikeLabel,
    /// A numeric field is out of range, non-finite, or inconsistent.
    BadNumber(&'static str),
    /// The payload is not valid JSON for the schema (includes unknown fields).
    Malformed(String),
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema(v) => write!(f, "unsupported telemetry schema version {v}"),
            Self::BadLabel(why) => write!(f, "invalid label: {why}"),
            Self::IpLikeLabel => f.write_str("label looks like an IP address"),
            Self::BadNumber(field) => write!(f, "field `{field}` is out of range"),
            Self::Malformed(why) => write!(f, "malformed telemetry payload: {why}"),
        }
    }
}

impl std::error::Error for ValidationError {}

/// A short, validated identifier token (model name, runtime, provider, ...).
///
/// The only string type in the payload. Charset `[A-Za-z0-9._+/-]`, 1 to
/// [`MAX_LABEL_LEN`] chars, and never IP-shaped. No whitespace or `:`/`@`, so a
/// prompt, a sentence, a `host:port` or a credential-looking value cannot be
/// stored.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Label(String);

impl Label {
    /// Validate and wrap `value`.
    ///
    /// # Errors
    /// [`ValidationError::BadLabel`] or [`ValidationError::IpLikeLabel`].
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_LABEL_LEN {
            return Err(ValidationError::BadLabel(format!(
                "length must be 1..={MAX_LABEL_LEN}"
            )));
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '/' | '-'))
        {
            return Err(ValidationError::BadLabel("disallowed character".into()));
        }
        if looks_like_ipv4(&value) {
            return Err(ValidationError::IpLikeLabel);
        }
        if looks_like_credential(&value) {
            return Err(ValidationError::BadLabel("looks like a credential".into()));
        }
        Ok(Self(value))
    }

    /// The label text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Best effort for an *optional* label, on the client side: the value if
    /// valid, otherwise `None` (drop the label). An IP-shaped value is dropped
    /// outright, never coarsened. One bad optional label must never cost the
    /// whole report. Server-side decoding stays strict ([`Label::new`]), and
    /// required labels use that strict constructor too.
    #[must_use]
    pub fn sanitized(value: &str) -> Option<Self> {
        Self::new(value).ok()
    }

    /// Like [`sanitized`](Self::sanitized), for a runtime *version* only: a
    /// four-part numeric version that merely looks like an IP address (for
    /// example `12.4.1.0`) is coarsened to its first three components, but only
    /// when it cannot be a real or private address (first component above 40, or
    /// a private/loopback/link-local range, or any component of 100 or more, is
    /// dropped instead) and the result is no longer IP-shaped. A bare
    /// `a.b.c.d` can never be told apart from a public address with certainty,
    /// so this is deliberately conservative.
    #[must_use]
    pub fn sanitized_version(value: &str) -> Option<Self> {
        match Self::new(value) {
            Ok(label) => Some(label),
            Err(ValidationError::IpLikeLabel) => {
                let parts: Vec<&str> = value.split('.').collect();
                // Only a plain four-part numeric version is a coarsening candidate.
                if parts.len() != 4 {
                    return None;
                }
                let octets: Vec<u16> = parts
                    .iter()
                    .map(|p| p.parse::<u16>().ok())
                    .collect::<Option<_>>()?;
                let private = matches!(
                    (octets[0], octets[1]),
                    (10 | 127, _) | (169, 254) | (172, 16..=31) | (192, 168) | (100, 64..=127)
                );
                if private || octets[0] > 40 || octets.iter().any(|o| *o >= 100) {
                    return None;
                }
                Self::new(parts[..3].join(".")).ok()
            }
            Err(_) => None,
        }
    }
}

impl TryFrom<String> for Label {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Label> for String {
    fn from(label: Label) -> Self {
        label.0
    }
}

/// Whether `s` contains an IPv4-shaped run: four dot-separated numeric groups,
/// each at most 255. Addresses in `0.0.0.0/8` ("this network", never a host)
/// are not flagged, so a four-part version such as `0.6.3.1` stays usable; any
/// other four-part numeric version must be written differently (e.g. `12.4.1-0`).
fn looks_like_ipv4(s: &str) -> bool {
    s.split(|c: char| !c.is_ascii_digit() && c != '.')
        .any(|run| {
            let parts: Vec<&str> = run.split('.').collect();
            parts.windows(4).any(|w| {
                let octets: Option<Vec<u16>> = w
                    .iter()
                    .map(|p| {
                        (!p.is_empty() && p.len() <= 3)
                            .then(|| p.parse::<u16>().ok())
                            .flatten()
                            .filter(|v| *v <= 255)
                    })
                    .collect();
                octets.is_some_and(|o| o[0] != 0)
            })
        })
}

/// Whether `s` is shaped like an API token or key: a well-known credential
/// prefix at the start of the label or of any `/` segment, a long hex string, or
/// a long mixed alphanumeric run.
fn looks_like_credential(s: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "hf_",
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "akia",
        "asia",
        "aiza",
        "ya29.",
    ];
    let lower = s.to_ascii_lowercase();
    if lower
        .split('/')
        .any(|seg| PREFIXES.iter().any(|p| seg.starts_with(p)))
    {
        return true;
    }
    if lower.len() >= 32 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|run| {
            run.len() >= 40
                && run.bytes().any(|b| b.is_ascii_digit())
                && run.bytes().any(|b| b.is_ascii_alphabetic())
        })
}

/// The observation window a report covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    /// Window start, seconds since the Unix epoch.
    pub start_unix_s: u64,
    /// Window length in seconds (1..=[`MAX_WINDOW_SECONDS`]).
    pub duration_s: u32,
}

/// Latency (or any positive quantity) summarized by percentiles.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

impl Percentiles {
    /// Explicit percentiles.
    #[must_use]
    pub fn new(p50: f64, p95: f64, p99: f64) -> Self {
        Self { p50, p95, p99 }
    }

    /// Summarize `samples` by nearest-rank percentile; empty input is all-zero.
    #[must_use]
    pub fn from_samples(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        Self {
            p50: nearest_rank(&sorted, 50.0),
            p95: nearest_rank(&sorted, 95.0),
            p99: nearest_rank(&sorted, 99.0),
        }
    }

    fn validate(&self, field: &'static str) -> Result<(), ValidationError> {
        let ok = [self.p50, self.p95, self.p99]
            .iter()
            .all(|v| v.is_finite() && (0.0..=MAX_LATENCY_MS).contains(v))
            && self.p50 <= self.p95
            && self.p95 <= self.p99;
        if ok {
            Ok(())
        } else {
            Err(ValidationError::BadNumber(field))
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn nearest_rank(sorted: &[f64], pct: f64) -> f64 {
    let n = sorted.len();
    let rank = (pct / 100.0 * n as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(n - 1)]
}

/// Hardware the deployment ran on. Model labels only; no hostnames or serials.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardwareInfo {
    /// Accelerator model, e.g. `h100-80gb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accelerator: Option<Label>,
    /// Number of accelerators.
    #[serde(default)]
    pub accelerator_count: u32,
    /// Per-accelerator memory, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_bytes: Option<u64>,
    /// Host CPU cores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    /// Host system memory, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
}

impl HardwareInfo {
    /// Set the accelerator model from raw text and its count; the model label
    /// is dropped if unacceptable (the count is kept).
    #[must_use]
    pub fn with_accelerator(mut self, model: &str, count: u32) -> Self {
        self.accelerator = Label::sanitized(model);
        self.accelerator_count = count;
        self
    }

    fn validate(&self) -> Result<(), ValidationError> {
        if self.accelerator_count > MAX_ACCELERATORS {
            return Err(ValidationError::BadNumber("hardware.accelerator_count"));
        }
        if self.cpu_cores.is_some_and(|c| c > MAX_CPU_CORES) {
            return Err(ValidationError::BadNumber("hardware.cpu_cores"));
        }
        if self.vram_bytes.is_some_and(|b| b > MAX_BYTES) {
            return Err(ValidationError::BadNumber("hardware.vram_bytes"));
        }
        if self.memory_bytes.is_some_and(|b| b > MAX_BYTES) {
            return Err(ValidationError::BadNumber("hardware.memory_bytes"));
        }
        // A named accelerator needs a count (the converse is allowed: the model
        // label is optional and may have been dropped client-side).
        if self.accelerator.is_some() && self.accelerator_count == 0 {
            return Err(ValidationError::BadNumber("hardware.accelerator_count"));
        }
        Ok(())
    }
}

/// The served model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    /// Model name, e.g. `meta-llama/Llama-3.1-8B-Instruct`.
    pub name: Label,
    /// Quantization, e.g. `Q4_K_M`, `fp8`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<Label>,
}

impl ModelInfo {
    /// A model with no quantization.
    #[must_use]
    pub fn new(name: Label) -> Self {
        Self {
            name,
            quantization: None,
        }
    }

    /// Set the quantization from raw text; dropped if unacceptable.
    #[must_use]
    pub fn with_quantization(mut self, quantization: &str) -> Self {
        self.quantization = Label::sanitized(quantization);
        self
    }
}

/// The serving runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInfo {
    /// Runtime name, e.g. `vllm`, `llama.cpp`.
    pub name: Label,
    /// Runtime version (optional; see [`RuntimeInfo::with_version`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Label>,
}

impl RuntimeInfo {
    /// A runtime with no version.
    #[must_use]
    pub fn new(name: Label) -> Self {
        Self {
            name,
            version: None,
        }
    }

    /// Set the version from raw text via [`Label::sanitized_version`]: coarsened
    /// to three components only when safe, otherwise dropped (never an error).
    #[must_use]
    pub fn with_version(mut self, version: &str) -> Self {
        self.version = Label::sanitized_version(version);
        self
    }
}

/// The measured rollup for the window. Aggregates only.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollup {
    /// Requests served in the window.
    pub requests: u64,
    /// Requests that errored (`errors <= requests`).
    #[serde(default)]
    pub errors: u64,
    /// End-to-end latency percentiles, ms.
    #[serde(default)]
    pub latency_e2e_ms: Percentiles,
    /// Time-to-first-token percentiles, ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ttft_ms: Option<Percentiles>,
    /// Generated tokens per second.
    #[serde(default)]
    pub output_tokens_per_s: f64,
    /// Requests per second.
    #[serde(default)]
    pub requests_per_s: f64,
    /// Device memory in use, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used_bytes: Option<u64>,
    /// KV-cache utilization, fraction in `[0, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv_cache_utilization: Option<f64>,
    /// GPU utilization, fraction in `[0, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_utilization: Option<f64>,
    /// Cost per hour in EUR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_per_hour_eur: Option<f64>,
}

impl Rollup {
    /// A rollup for `requests` requests of which `errors` failed. Fill the rest
    /// with the `with_*` setters straight from a `/metrics` scrape.
    #[must_use]
    pub fn new(requests: u64, errors: u64) -> Self {
        Self {
            requests,
            errors,
            ..Self::default()
        }
    }

    /// Error rate in `[0, 1]` (0 when there were no requests).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn error_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.errors as f64 / self.requests as f64
        }
    }

    /// Set end-to-end latency percentiles.
    #[must_use]
    pub fn with_latency_e2e_ms(mut self, p: Percentiles) -> Self {
        self.latency_e2e_ms = p;
        self
    }

    /// Set time-to-first-token percentiles.
    #[must_use]
    pub fn with_latency_ttft_ms(mut self, p: Percentiles) -> Self {
        self.latency_ttft_ms = Some(p);
        self
    }

    /// Set output-token and request throughput.
    #[must_use]
    pub fn with_throughput(mut self, output_tokens_per_s: f64, requests_per_s: f64) -> Self {
        self.output_tokens_per_s = output_tokens_per_s;
        self.requests_per_s = requests_per_s;
        self
    }

    /// Set device memory and KV-cache utilization.
    #[must_use]
    pub fn with_memory(mut self, used_bytes: u64, kv_cache_utilization: Option<f64>) -> Self {
        self.memory_used_bytes = Some(used_bytes);
        self.kv_cache_utilization = kv_cache_utilization;
        self
    }

    /// Set GPU utilization.
    #[must_use]
    pub fn with_gpu_utilization(mut self, fraction: f64) -> Self {
        self.gpu_utilization = Some(fraction);
        self
    }

    /// Set cost per hour in EUR.
    #[must_use]
    pub fn with_cost_per_hour_eur(mut self, cost: f64) -> Self {
        self.cost_per_hour_eur = Some(cost);
        self
    }

    fn validate(&self) -> Result<(), ValidationError> {
        if self.requests > MAX_REQUESTS || self.errors > self.requests {
            return Err(ValidationError::BadNumber("requests"));
        }
        self.latency_e2e_ms.validate("latency_e2e_ms")?;
        if let Some(t) = &self.latency_ttft_ms {
            t.validate("latency_ttft_ms")?;
        }
        let bounded = |v: f64, max: f64, field| {
            if v.is_finite() && (0.0..=max).contains(&v) {
                Ok(())
            } else {
                Err(ValidationError::BadNumber(field))
            }
        };
        if self.memory_used_bytes.is_some_and(|b| b > MAX_BYTES) {
            return Err(ValidationError::BadNumber("memory_used_bytes"));
        }
        let fraction = |v: Option<f64>, field| match v {
            Some(v) if !(v.is_finite() && (0.0..=1.0).contains(&v)) => {
                Err(ValidationError::BadNumber(field))
            }
            _ => Ok(()),
        };
        bounded(self.output_tokens_per_s, MAX_RATE, "output_tokens_per_s")?;
        bounded(self.requests_per_s, MAX_RATE, "requests_per_s")?;
        if let Some(c) = self.cost_per_hour_eur {
            bounded(c, MAX_COST_PER_HOUR_EUR, "cost_per_hour_eur")?;
        }
        fraction(self.kv_cache_utilization, "kv_cache_utilization")?;
        fraction(self.gpu_utilization, "gpu_utilization")
    }
}

/// One deployment's aggregated telemetry for one window. See the module docs
/// for every field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryReport {
    /// Payload schema version ([`TELEMETRY_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Random, rotatable install id.
    pub install_id: InstallId,
    /// Random id scoped to this deployment; not its name.
    pub deployment_id: AnonDeploymentId,
    /// The window covered.
    pub window: Window,
    /// Hardware.
    #[serde(default)]
    pub hardware: HardwareInfo,
    /// Model.
    pub model: ModelInfo,
    /// Runtime.
    pub runtime: RuntimeInfo,
    /// Provider kind, e.g. `hetzner`, `self-hosted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Label>,
    /// Provider region code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Label>,
    /// The measured rollup.
    pub metrics: Rollup,
}

impl TelemetryReport {
    /// Set the provider label from raw text; dropped if unacceptable.
    #[must_use]
    pub fn with_provider(mut self, provider: &str) -> Self {
        self.provider = Label::sanitized(provider);
        self
    }

    /// Set the region label from raw text; dropped if unacceptable.
    #[must_use]
    pub fn with_region(mut self, region: &str) -> Self {
        self.region = Label::sanitized(region);
        self
    }

    /// Check the whole report: schema version, window, and every number.
    ///
    /// # Errors
    /// The first [`ValidationError`] found.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema_version != TELEMETRY_SCHEMA_VERSION {
            return Err(ValidationError::UnsupportedSchema(self.schema_version));
        }
        if self.window.duration_s == 0 || self.window.duration_s > MAX_WINDOW_SECONDS {
            return Err(ValidationError::BadNumber("window.duration_s"));
        }
        if !(MIN_START_UNIX_S..=MAX_START_UNIX_S).contains(&self.window.start_unix_s) {
            return Err(ValidationError::BadNumber("window.start_unix_s"));
        }
        self.hardware.validate()?;
        self.metrics.validate()
    }

    /// Compact JSON (absent optionals omitted).
    ///
    /// # Errors
    /// Validation failure, so an invalid report is never serialized for sending.
    pub fn to_json(&self) -> Result<String, ValidationError> {
        self.validate()?;
        serde_json::to_string(self).map_err(|e| ValidationError::Malformed(e.to_string()))
    }

    /// Strictly decode and validate a payload: unknown fields, bad labels and
    /// out-of-range numbers are all rejected.
    ///
    /// # Errors
    /// [`ValidationError`].
    pub fn from_json(json: &str) -> Result<Self, ValidationError> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| ValidationError::Malformed(e.to_string()))?;
        // Read the version first: a newer schema legitimately has fields this
        // one denies, and must be reported as unsupported, not as malformed.
        let version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| ValidationError::Malformed("missing schema_version".into()))?;
        if version != u64::from(TELEMETRY_SCHEMA_VERSION) {
            return Err(ValidationError::UnsupportedSchema(
                u32::try_from(version).unwrap_or(u32::MAX),
            ));
        }
        let report: Self =
            serde_json::from_value(value).map_err(|e| ValidationError::Malformed(e.to_string()))?;
        report.validate()?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{AnonDeploymentId, InstallId};

    fn report() -> TelemetryReport {
        TelemetryReport {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            install_id: InstallId::from_random_bytes([1; 16]),
            deployment_id: AnonDeploymentId::from_random_bytes([2; 16]),
            window: Window {
                start_unix_s: 1_750_000_000,
                duration_s: 300,
            },
            hardware: HardwareInfo {
                accelerator: Some(Label::new("h100-80gb").unwrap()),
                accelerator_count: 1,
                vram_bytes: Some(80 << 30),
                cpu_cores: Some(24),
                memory_bytes: Some(256 << 30),
            },
            model: ModelInfo {
                name: Label::new("meta-llama/Llama-3.1-8B-Instruct").unwrap(),
                quantization: Some(Label::new("fp8").unwrap()),
            },
            runtime: RuntimeInfo::new(Label::new("vllm").unwrap()).with_version("0.11.0"),
            provider: Some(Label::new("hetzner").unwrap()),
            region: Some(Label::new("nbg1").unwrap()),
            metrics: Rollup::new(1000, 5)
                .with_latency_e2e_ms(Percentiles::from_samples(&[10.0, 20.0, 30.0, 40.0]))
                .with_latency_ttft_ms(Percentiles::new(5.0, 9.0, 12.0))
                .with_throughput(900.0, 3.3)
                .with_memory(60 << 30, Some(0.4))
                .with_gpu_utilization(0.8)
                .with_cost_per_hour_eur(2.5),
        }
    }

    #[test]
    fn round_trips_and_is_compact() {
        let r = report();
        let json = r.to_json().unwrap();
        assert!(!json.contains('\n') && !json.contains(": "));
        assert_eq!(TelemetryReport::from_json(&json).unwrap(), r);
        // Absent optionals are omitted.
        let mut minimal = report();
        minimal.provider = None;
        minimal.metrics.gpu_utilization = None;
        assert!(!minimal.to_json().unwrap().contains("gpu_utilization"));
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        let good = serde_json::to_value(report()).unwrap();
        for path in [
            vec![],
            vec!["metrics"],
            vec!["hardware"],
            vec!["model"],
            vec!["runtime"],
            vec!["window"],
            vec!["metrics", "latency_e2e_ms"],
        ] {
            let mut v = good.clone();
            let mut node = &mut v;
            for p in &path {
                node = node.get_mut(*p).unwrap();
            }
            node.as_object_mut()
                .unwrap()
                .insert("prompt".into(), "hello".into());
            let err = TelemetryReport::from_json(&v.to_string()).unwrap_err();
            assert!(matches!(err, ValidationError::Malformed(_)), "{path:?}");
        }
    }

    #[test]
    fn content_secret_and_ip_fields_cannot_enter() {
        // Forbidden concepts have no field name anywhere in the serialized
        // schema of a fully-populated report.
        let keys = collect_keys(&serde_json::to_value(report()).unwrap());
        for forbidden in [
            "prompt",
            "completion",
            "content",
            "secret",
            "api_key",
            "password",
            "ip",
            "address",
            "host",
            "user",
        ] {
            assert!(
                !keys.iter().any(|k| k == forbidden),
                "field named `{forbidden}` present"
            );
        }
    }

    fn collect_keys(v: &serde_json::Value) -> Vec<String> {
        match v {
            serde_json::Value::Object(m) => m
                .iter()
                .flat_map(|(k, v)| std::iter::once(k.clone()).chain(collect_keys(v)))
                .collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn labels_cannot_carry_prose_secrets_or_ips() {
        for bad in [
            "",
            "what is the capital of France",
            "sk-abc 123",
            "user@host",
            "host:8080",
            "10.0.0.1",
            "gpu-192.168.1.20",
            "::1",
            "hf_abcdefghijklmnop",
            "sk-proj-abc123",
            "ghp_abcdefghijklmnop",
            "github_pat_11ABCDEFG",
            "meta/sk-abc123",
            "0123456789abcdef0123456789abcdef",
            &"x".repeat(MAX_LABEL_LEN + 1),
        ] {
            assert!(Label::new(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in [
            "h100-80gb",
            "Q4_K_M",
            "0.11.0",
            "0.6.3.1",
            "1.2.3",
            "meta-llama/Llama-3.1-8B",
            "skylake",
        ] {
            assert!(Label::new(good).is_ok(), "{good:?} must be accepted");
        }
        // Rejected during deserialization too, not just construction.
        let mut v = serde_json::to_value(report()).unwrap();
        v["provider"] = "10.1.2.3".into();
        assert!(TelemetryReport::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn numbers_are_bounds_checked() {
        let mut r = report();
        r.metrics.errors = r.metrics.requests + 1;
        assert!(r.validate().is_err());
        let mut r = report();
        r.metrics.gpu_utilization = Some(1.5);
        assert!(r.validate().is_err());
        let mut r = report();
        r.metrics.output_tokens_per_s = f64::NAN;
        assert!(r.validate().is_err());
        let mut r = report();
        r.metrics.latency_e2e_ms = Percentiles::new(5.0, 2.0, 1.0);
        assert!(r.validate().is_err());
        let mut r = report();
        r.window.duration_s = 0;
        assert!(r.validate().is_err());
        let mut r = report();
        r.schema_version = 99;
        assert_eq!(r.validate(), Err(ValidationError::UnsupportedSchema(99)));
        assert!(r.to_json().is_err(), "invalid reports are never serialized");
    }

    #[test]
    fn hardware_and_plausibility_bounds() {
        let mut r = report();
        r.hardware.accelerator_count = 5_000;
        assert!(r.validate().is_err());
        let mut r = report();
        r.hardware.vram_bytes = Some(u64::MAX);
        assert!(r.validate().is_err());
        let mut r = report();
        r.hardware.cpu_cores = Some(10_000_000);
        assert!(r.validate().is_err());
        let mut r = report();
        r.hardware.accelerator = None; // model label dropped client-side: fine
        assert!(r.validate().is_ok());
        let mut r = report();
        r.hardware.accelerator_count = 0; // a named accelerator needs a count
        assert!(r.validate().is_err());
        let mut r = report();
        r.metrics.requests = u64::MAX;
        assert!(r.validate().is_err());
        let mut r = report();
        r.metrics.output_tokens_per_s = 1e12;
        assert!(r.validate().is_err());
        let mut r = report();
        r.metrics.latency_e2e_ms = Percentiles::new(1.0, 2.0, 1e12);
        assert!(r.validate().is_err());
        let mut r = report();
        r.metrics.cost_per_hour_eur = Some(1e12);
        assert!(r.validate().is_err());
        let mut r = report();
        r.window.start_unix_s = 0;
        assert!(r.validate().is_err());
        let mut r = report();
        r.window.start_unix_s = u64::MAX;
        assert!(r.validate().is_err());
    }

    #[test]
    fn optional_labels_are_coarsened_or_dropped_never_fatal() {
        // A version that merely looks like an address is coarsened to three
        // components only when it cannot be a private or real address...
        let rt = RuntimeInfo::new(Label::new("vllm").unwrap()).with_version("12.4.1.0");
        assert_eq!(rt.version.as_ref().map(Label::as_str), Some("12.4.1"));
        // ...and dropped when it looks like one.
        for ip in [
            "10.0.0.1",
            "192.168.1.20",
            "172.16.0.9",
            "127.0.0.1",
            "203.0.113.7",
        ] {
            let rt = RuntimeInfo::new(Label::new("vllm").unwrap()).with_version(ip);
            assert!(rt.version.is_none(), "{ip}");
        }
        // ...a four-part version in the non-routable range is kept as is...
        let rt = RuntimeInfo::new(Label::new("vllm").unwrap()).with_version("0.6.3.1");
        assert_eq!(rt.version.as_ref().map(Label::as_str), Some("0.6.3.1"));
        // ...and anything still suspicious is dropped rather than failing.
        for bad in ["sk-proj-abc123", "has spaces in it", "user@host", ""] {
            let rt = RuntimeInfo::new(Label::new("vllm").unwrap()).with_version(bad);
            assert!(rt.version.is_none(), "{bad:?}");
            let m = ModelInfo::new(Label::new("m").unwrap()).with_quantization(bad);
            assert!(m.quantization.is_none(), "{bad:?}");
        }
        let hw = HardwareInfo::default().with_accelerator("hf_notamodel", 2);
        assert!(hw.accelerator.is_none() && hw.accelerator_count == 2);
        // The report still builds, validates and round-trips.
        let mut r = report();
        r.runtime = RuntimeInfo::new(Label::new("vllm").unwrap()).with_version("sk-secret-1");
        r.model.quantization = Label::sanitized("a b");
        // Provider, region, quantization and accelerator are never coarsened:
        // an IP-shaped value is dropped outright.
        let r = r.with_provider("10.1.2.3").with_region("nbg1");
        assert!(r.provider.is_none());
        assert!(Label::sanitized("192.168.0.1").is_none());
        assert!(
            ModelInfo::new(Label::new("m").unwrap())
                .with_quantization("12.4.1.0")
                .quantization
                .is_none()
        );
        assert!(
            HardwareInfo::default()
                .with_accelerator("12.4.1.0", 1)
                .accelerator
                .is_none()
        );
        assert_eq!(
            TelemetryReport::from_json(&r.to_json().unwrap()).unwrap(),
            r
        );
        // Server-side decoding stays strict.
        let mut v = serde_json::to_value(&r).unwrap();
        v["runtime"]["version"] = "10.0.0.1".into();
        assert!(TelemetryReport::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn newer_schema_is_unsupported_not_malformed() {
        let mut v = serde_json::to_value(report()).unwrap();
        v["schema_version"] = 2.into();
        v["a_field_from_the_future"] = 1.into();
        assert_eq!(
            TelemetryReport::from_json(&v.to_string()),
            Err(ValidationError::UnsupportedSchema(2))
        );
        let mut v = serde_json::to_value(report()).unwrap();
        v.as_object_mut().unwrap().remove("schema_version");
        assert!(matches!(
            TelemetryReport::from_json(&v.to_string()),
            Err(ValidationError::Malformed(_))
        ));
    }

    #[test]
    fn error_rate_and_percentiles() {
        assert!((Rollup::new(200, 5).error_rate() - 0.025).abs() < 1e-12);
        assert!(Rollup::new(0, 0).error_rate().abs() < 1e-12);
        let s: Vec<f64> = (1..=100).map(f64::from).collect();
        let p = Percentiles::from_samples(&s);
        assert!((p.p50 - 50.0).abs() < 1e-9 && (p.p95 - 95.0).abs() < 1e-9);
    }
}
