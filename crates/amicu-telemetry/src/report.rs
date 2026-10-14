//! The opt-in telemetry payload: a versioned, aggregates-only, content-free
//! report (andypalmi/amicu-planning#228).
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
        Ok(Self(value))
    }

    /// The label text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
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

/// Whether `s` contains four dot-separated numeric groups (an IPv4 shape),
/// anywhere in the string.
fn looks_like_ipv4(s: &str) -> bool {
    let groups: Vec<&str> = s.split(|c: char| !c.is_ascii_digit() && c != '.').collect();
    groups.iter().any(|g| {
        let parts: Vec<&str> = g.split('.').collect();
        parts.windows(4).any(|w| {
            w.iter()
                .all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
        })
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
            .all(|v| v.is_finite() && *v >= 0.0)
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

/// The serving runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInfo {
    /// Runtime name, e.g. `vllm`, `llama.cpp`.
    pub name: Label,
    /// Runtime version.
    pub version: Label,
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
        if self.errors > self.requests {
            return Err(ValidationError::BadNumber("errors"));
        }
        self.latency_e2e_ms.validate("latency_e2e_ms")?;
        if let Some(t) = &self.latency_ttft_ms {
            t.validate("latency_ttft_ms")?;
        }
        let non_negative = |v: f64, field| {
            if v.is_finite() && v >= 0.0 {
                Ok(())
            } else {
                Err(ValidationError::BadNumber(field))
            }
        };
        let fraction = |v: Option<f64>, field| match v {
            Some(v) if !(v.is_finite() && (0.0..=1.0).contains(&v)) => {
                Err(ValidationError::BadNumber(field))
            }
            _ => Ok(()),
        };
        non_negative(self.output_tokens_per_s, "output_tokens_per_s")?;
        non_negative(self.requests_per_s, "requests_per_s")?;
        if let Some(c) = self.cost_per_hour_eur {
            non_negative(c, "cost_per_hour_eur")?;
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
        let report: Self =
            serde_json::from_str(json).map_err(|e| ValidationError::Malformed(e.to_string()))?;
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
                start_unix_s: 1_700_000_000,
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
            runtime: RuntimeInfo {
                name: Label::new("vllm").unwrap(),
                version: Label::new("0.11.0").unwrap(),
            },
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
            &"x".repeat(MAX_LABEL_LEN + 1),
        ] {
            assert!(Label::new(bad).is_err(), "{bad:?} must be rejected");
        }
        for good in ["h100-80gb", "Q4_K_M", "0.11.0", "meta-llama/Llama-3.1-8B"] {
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
    fn error_rate_and_percentiles() {
        assert!((Rollup::new(200, 5).error_rate() - 0.025).abs() < 1e-12);
        assert!(Rollup::new(0, 0).error_rate().abs() < 1e-12);
        let s: Vec<f64> = (1..=100).map(f64::from).collect();
        let p = Percentiles::from_samples(&s);
        assert!((p.p50 - 50.0).abs() < 1e-9 && (p.p95 - 95.0).abs() < 1e-9);
    }
}
