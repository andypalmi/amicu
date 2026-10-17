//! The opt-in telemetry payload: a versioned, aggregates-only, content-free
//! report.
//!
//! # What is collected
//!
//! One [`TelemetryReport`] describes **one deployment over one time window**
//! (five minutes). It carries only rollups - never per-request records. The
//! upload body is a [`TelemetryBatch`] of reports. The wire format is also
//! described, field by field, by the JSON Schema checked in at
//! `crates/amicu-telemetry/schema/telemetry-batch.v2.schema.json`; a test keeps
//! the two in step.
//!
//! | Section | Fields |
//! |---|---|
//! | batch | `schema_version`, `batch_id` (random, the idempotency key), `reports` |
//! | envelope | `schema_version`, `install_id`, `deployment_id` (anonymous, not the deployment name), `window` (`start_unix_s`, `duration_s`), `amicu_version` |
//! | `hardware` | `accelerator` (model label), `accelerator_count`, `vram_bytes`, `cpu_cores`, `memory_bytes` |
//! | `model` | `name` (public id, or `custom`), `quantization` |
//! | `runtime` | `name`, `version` |
//! | placement | `provider`, `region` |
//! | configuration | `context_length`, `batch_size` |
//! | `metrics` | `requests`, `errors`, `prompt_tokens`, `output_tokens`, `output_tokens_per_s`, `decode_tokens_per_s`, `ttft_ms` / `e2e_ms` / `tpot_ms` (histograms: `counts`, `sum_ms`), `running_peak`, `waiting_peak`, `kv_cache_utilization_peak`, `preemptions`, `gpu_utilization`, `gpu_memory_peak_bytes`, `host_memory_peak_bytes` |
//!
//! Latency is sent as bucket counts over fixed bounds ([`TTFT_BOUNDS_MS`],
//! [`E2E_BOUNDS_MS`], [`TPOT_BOUNDS_MS`]), not percentiles, so the server can
//! add windows together exactly.
//!
//! # What can never be represented
//!
//! Prompts, completions, secrets and IP addresses have no field. The payload is
//! an allow-list enforced three ways:
//!
//! 1. **By construction**: every field is a number, an id, a [`Label`], a list
//!    of counts, or a nested struct of those. There is no free-form `String`, `Value`, `Map` or
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

use crate::identity::{AnonDeploymentId, BatchId, InstallId};

/// Current schema version of [`TelemetryReport`]. Bump on any field change.
pub const TELEMETRY_SCHEMA_VERSION: u32 = 2;

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
/// Most tokens one deployment can plausibly process in one window.
pub const MAX_TOKENS: u64 = 1 << 50;
/// Largest plausible concurrency or batch size.
pub const MAX_CONCURRENCY: u64 = 1 << 20;
/// Largest plausible context length, tokens.
pub const MAX_CONTEXT_LENGTH: u32 = 1 << 24;
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

/// Upper bounds (ms) of the time-to-first-token histogram buckets.
pub const TTFT_BOUNDS_MS: &[f64] = &[
    10.0, 20.0, 40.0, 60.0, 80.0, 100.0, 250.0, 500.0, 750.0, 1_000.0, 2_500.0, 5_000.0, 7_500.0,
    10_000.0, 20_000.0, 40_000.0, 80_000.0,
];
/// Upper bounds (ms) of the end-to-end request latency histogram buckets.
pub const E2E_BOUNDS_MS: &[f64] = &[
    300.0, 500.0, 800.0, 1_000.0, 1_500.0, 2_000.0, 2_500.0, 5_000.0, 10_000.0, 15_000.0, 20_000.0,
    30_000.0, 40_000.0, 50_000.0, 60_000.0, 120_000.0, 240_000.0, 480_000.0,
];
/// Upper bounds (ms) of the time-per-output-token histogram buckets.
pub const TPOT_BOUNDS_MS: &[f64] = &[
    10.0, 25.0, 50.0, 75.0, 100.0, 150.0, 200.0, 300.0, 400.0, 500.0, 750.0, 1_000.0, 2_500.0,
];

/// Which fixed bucket layout a [`Histogram`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistogramKind {
    /// [`TTFT_BOUNDS_MS`].
    Ttft,
    /// [`E2E_BOUNDS_MS`].
    E2e,
    /// [`TPOT_BOUNDS_MS`].
    Tpot,
}

impl HistogramKind {
    /// The bucket upper bounds, ms. The last bucket (`> bounds.last()`) is the
    /// overflow bucket.
    #[must_use]
    pub fn bounds_ms(self) -> &'static [f64] {
        match self {
            Self::Ttft => TTFT_BOUNDS_MS,
            Self::E2e => E2E_BOUNDS_MS,
            Self::Tpot => TPOT_BOUNDS_MS,
        }
    }
}

/// A latency histogram over one of the fixed layouts of [`HistogramKind`].
///
/// `counts[i]` is the number of observations in bucket `i` (not cumulative):
/// bucket `i` holds values in `(bounds[i-1], bounds[i]]`, and the extra last
/// bucket holds values above the last bound, so `counts.len() ==
/// bounds.len() + 1`. Bucket counts add up exactly, which is what makes hourly
/// and daily downsampling server-side exact. `sum_ms` is the sum of all
/// observations, so means are exact too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Histogram {
    pub counts: Vec<u64>,
    pub sum_ms: f64,
}

impl Histogram {
    /// An empty histogram of `kind`.
    #[must_use]
    pub fn empty(kind: HistogramKind) -> Self {
        Self {
            counts: vec![0; kind.bounds_ms().len() + 1],
            sum_ms: 0.0,
        }
    }

    /// Re-bucket a runtime's cumulative Prometheus buckets (`(le_seconds,
    /// cumulative_count)`, sorted, ending with `+Inf`) onto `kind`'s fixed
    /// bounds. The cumulative count at a fixed bound is the runtime's count at
    /// its largest bound not above it, so the mapping is exact when the
    /// runtime's bounds include the fixed ones (vLLM's do) and otherwise moves
    /// observations up, never down. `None` when there are no buckets.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn from_cumulative(
        kind: HistogramKind,
        buckets_s: &[(f64, f64)],
        sum_s: f64,
    ) -> Option<Self> {
        let total = buckets_s.last()?.1;
        let at = |bound_ms: f64| {
            buckets_s
                .iter()
                .take_while(|(le, _)| *le * 1000.0 <= bound_ms * (1.0 + 1e-9))
                .last()
                .map_or(0.0, |(_, c)| *c)
        };
        let count = |v: f64| {
            if v.is_finite() && v > 0.0 {
                v as u64
            } else {
                0
            }
        };
        let mut counts = Vec::with_capacity(kind.bounds_ms().len() + 1);
        let mut below = 0.0;
        for &bound in kind.bounds_ms() {
            let cumulative = at(bound).max(below);
            counts.push(count(cumulative - below));
            below = cumulative;
        }
        counts.push(count(total - below));
        Some(Self {
            counts,
            sum_ms: if sum_s.is_finite() && sum_s > 0.0 {
                sum_s * 1000.0
            } else {
                0.0
            },
        })
    }

    /// Observations in the histogram.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.counts.iter().fold(0, |a, c| a.saturating_add(*c))
    }

    /// `self - earlier`, bucket by bucket: the observations made in between.
    /// `None` when any bucket went down (the runtime restarted in between).
    #[must_use]
    pub fn delta_since(&self, earlier: &Self) -> Option<Self> {
        if self.counts.len() != earlier.counts.len() {
            return None;
        }
        let counts = self
            .counts
            .iter()
            .zip(&earlier.counts)
            .map(|(now, before)| now.checked_sub(*before))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            counts,
            sum_ms: (self.sum_ms - earlier.sum_ms).max(0.0),
        })
    }

    /// Add `other`'s observations (same layout) into `self`.
    pub fn merge(&mut self, other: &Self) {
        if self.counts.len() == other.counts.len() {
            for (a, b) in self.counts.iter_mut().zip(&other.counts) {
                *a = a.saturating_add(*b);
            }
            self.sum_ms += other.sum_ms;
        }
    }

    fn validate(&self, kind: HistogramKind, field: &'static str) -> Result<(), ValidationError> {
        #[allow(clippy::cast_precision_loss)]
        let max_sum = MAX_LATENCY_MS * self.total() as f64;
        let ok = self.counts.len() == kind.bounds_ms().len() + 1
            && self.total() <= MAX_REQUESTS
            && self.sum_ms.is_finite()
            && (0.0..=max_sum).contains(&self.sum_ms);
        if ok {
            Ok(())
        } else {
            Err(ValidationError::BadNumber(field))
        }
    }
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

/// The measured rollup for the window. Aggregates only. `None` means the
/// runtime does not expose the value, which is different from zero.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollup {
    /// Requests finished in the window (vLLM; llama.cpp has no counter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<u64>,
    /// Of those, requests that did not finish normally (vLLM `abort`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<u64>,
    /// Prompt tokens processed in the window.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Tokens generated in the window.
    #[serde(default)]
    pub output_tokens: u64,
    /// `output_tokens` over the window length.
    #[serde(default)]
    pub output_tokens_per_s: f64,
    /// Average generation speed of a request while it decodes, tokens/s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_tokens_per_s: Option<f64>,
    /// Time to first token ([`TTFT_BOUNDS_MS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<Histogram>,
    /// End-to-end request latency ([`E2E_BOUNDS_MS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2e_ms: Option<Histogram>,
    /// Time per output token ([`TPOT_BOUNDS_MS`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpot_ms: Option<Histogram>,
    /// Most requests running at once, as sampled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_peak: Option<u32>,
    /// Most requests queued at once, as sampled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_peak: Option<u32>,
    /// Highest KV-cache utilization sampled, a fraction in `[0, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv_cache_utilization_peak: Option<f64>,
    /// KV-cache preemptions in the window (vLLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preemptions: Option<u64>,
    /// Mean accelerator utilization sampled, a fraction in `[0, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_utilization: Option<f64>,
    /// Highest accelerator memory in use sampled (all accelerators), bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_peak_bytes: Option<u64>,
    /// Highest resident memory of the server process sampled, bytes (vLLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_memory_peak_bytes: Option<u64>,
}

impl Rollup {
    /// Error rate in `[0, 1]` (0 when unknown or there were no requests).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn error_rate(&self) -> f64 {
        match (self.requests, self.errors) {
            (Some(r), Some(e)) if r > 0 => e as f64 / r as f64,
            _ => 0.0,
        }
    }

    fn validate(&self) -> Result<(), ValidationError> {
        if self.requests.is_some_and(|r| r > MAX_REQUESTS)
            || self.errors.is_some_and(|e| e > self.requests.unwrap_or(0))
        {
            return Err(ValidationError::BadNumber("requests"));
        }
        if self.prompt_tokens > MAX_TOKENS || self.output_tokens > MAX_TOKENS {
            return Err(ValidationError::BadNumber("tokens"));
        }
        let bounded = |v: f64, max: f64, field| {
            if v.is_finite() && (0.0..=max).contains(&v) {
                Ok(())
            } else {
                Err(ValidationError::BadNumber(field))
            }
        };
        bounded(self.output_tokens_per_s, MAX_RATE, "output_tokens_per_s")?;
        if let Some(v) = self.decode_tokens_per_s {
            bounded(v, MAX_RATE, "decode_tokens_per_s")?;
        }
        for (h, kind, field) in [
            (&self.ttft_ms, HistogramKind::Ttft, "ttft_ms"),
            (&self.e2e_ms, HistogramKind::E2e, "e2e_ms"),
            (&self.tpot_ms, HistogramKind::Tpot, "tpot_ms"),
        ] {
            if let Some(h) = h {
                h.validate(kind, field)?;
            }
        }
        for (v, field) in [
            (self.kv_cache_utilization_peak, "kv_cache_utilization_peak"),
            (self.gpu_utilization, "gpu_utilization"),
        ] {
            if let Some(v) = v {
                bounded(v, 1.0, field)?;
            }
        }
        for (v, field) in [
            (self.gpu_memory_peak_bytes, "gpu_memory_peak_bytes"),
            (self.host_memory_peak_bytes, "host_memory_peak_bytes"),
        ] {
            if v.is_some_and(|b| b > MAX_BYTES) {
                return Err(ValidationError::BadNumber(field));
            }
        }
        if self.preemptions.is_some_and(|p| p > MAX_REQUESTS) {
            return Err(ValidationError::BadNumber("preemptions"));
        }
        if [self.running_peak, self.waiting_peak]
            .iter()
            .any(|v| v.is_some_and(|n| u64::from(n) > MAX_CONCURRENCY))
        {
            return Err(ValidationError::BadNumber("running_peak"));
        }
        Ok(())
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
    /// The amicu release that produced the report, e.g. `0.4.0`.
    pub amicu_version: Label,
    /// Hardware.
    #[serde(default)]
    pub hardware: HardwareInfo,
    /// Model.
    pub model: ModelInfo,
    /// Runtime.
    pub runtime: RuntimeInfo,
    /// Provider kind, e.g. `local`, `hetzner`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Label>,
    /// Provider region code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Label>,
    /// Configured context length, tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// Configured batch size (llama.cpp `--batch-size`, vLLM `max-num-seqs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<u32>,
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
        if self.context_length.is_some_and(|c| c > MAX_CONTEXT_LENGTH) {
            return Err(ValidationError::BadNumber("context_length"));
        }
        if self
            .batch_size
            .is_some_and(|b| u64::from(b) > MAX_CONCURRENCY)
        {
            return Err(ValidationError::BadNumber("batch_size"));
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
        Self::from_value(value)
    }

    fn from_value(value: serde_json::Value) -> Result<Self, ValidationError> {
        check_version(&value)?;
        let report: Self =
            serde_json::from_value(value).map_err(|e| ValidationError::Malformed(e.to_string()))?;
        report.validate()?;
        Ok(report)
    }
}

/// Read `schema_version` first: a newer schema legitimately has fields this
/// one denies, and must be reported as unsupported, not as malformed.
fn check_version(value: &serde_json::Value) -> Result<(), ValidationError> {
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| ValidationError::Malformed("missing schema_version".into()))?;
    if version == u64::from(TELEMETRY_SCHEMA_VERSION) {
        Ok(())
    } else {
        Err(ValidationError::UnsupportedSchema(
            u32::try_from(version).unwrap_or(u32::MAX),
        ))
    }
}

/// Largest upload body, bytes. The ingestion endpoint rejects anything bigger.
pub const MAX_BATCH_BYTES: usize = 64 * 1024;

/// Most reports in one batch.
pub const MAX_BATCH_REPORTS: usize = 288;

/// The upload body: a batch of reports with an idempotency key, so a retried
/// upload is stored once. Serialized size is at most [`MAX_BATCH_BYTES`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryBatch {
    /// Payload schema version ([`TELEMETRY_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Random id of this batch, reused verbatim on every retry of it.
    pub batch_id: BatchId,
    /// One to [`MAX_BATCH_REPORTS`] reports.
    pub reports: Vec<TelemetryReport>,
}

impl TelemetryBatch {
    /// Compact JSON, validated and size-checked.
    ///
    /// # Errors
    /// Any report's validation error, an empty or oversized batch.
    pub fn to_json(&self) -> Result<String, ValidationError> {
        self.validate()?;
        let json =
            serde_json::to_string(self).map_err(|e| ValidationError::Malformed(e.to_string()))?;
        if json.len() > MAX_BATCH_BYTES {
            return Err(ValidationError::BadNumber("batch size"));
        }
        Ok(json)
    }

    /// Strictly decode and validate a batch (what the ingestion service does).
    ///
    /// # Errors
    /// [`ValidationError`].
    pub fn from_json(json: &str) -> Result<Self, ValidationError> {
        if json.len() > MAX_BATCH_BYTES {
            return Err(ValidationError::BadNumber("batch size"));
        }
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| ValidationError::Malformed(e.to_string()))?;
        check_version(&value)?;
        let batch: Self =
            serde_json::from_value(value).map_err(|e| ValidationError::Malformed(e.to_string()))?;
        batch.validate()?;
        Ok(batch)
    }

    fn validate(&self) -> Result<(), ValidationError> {
        if self.schema_version != TELEMETRY_SCHEMA_VERSION {
            return Err(ValidationError::UnsupportedSchema(self.schema_version));
        }
        if self.reports.is_empty() || self.reports.len() > MAX_BATCH_REPORTS {
            return Err(ValidationError::BadNumber("reports"));
        }
        self.reports.iter().try_for_each(TelemetryReport::validate)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::identity::{AnonDeploymentId, BatchId, InstallId};

    const GOLDEN: &str = include_str!("../schema/telemetry-batch.v2.example.json");
    const SCHEMA: &str = include_str!("../schema/telemetry-batch.v2.schema.json");

    fn hist(kind: HistogramKind, at: usize, n: u64, sum_ms: f64) -> Histogram {
        let mut h = Histogram::empty(kind);
        h.counts[at] = n;
        h.sum_ms = sum_ms;
        h
    }

    fn report() -> TelemetryReport {
        TelemetryReport {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            install_id: InstallId::from_random_bytes([1; 16]),
            deployment_id: AnonDeploymentId::from_random_bytes([2; 16]),
            window: Window {
                start_unix_s: 1_750_000_200,
                duration_s: 300,
            },
            amicu_version: Label::new("0.4.0").unwrap(),
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
            provider: Some(Label::new("local").unwrap()),
            region: Some(Label::new("nbg1").unwrap()),
            context_length: Some(8192),
            batch_size: Some(64),
            metrics: Rollup {
                requests: Some(120),
                errors: Some(2),
                prompt_tokens: 48_000,
                output_tokens: 30_000,
                output_tokens_per_s: 100.0,
                decode_tokens_per_s: Some(42.5),
                ttft_ms: Some(hist(HistogramKind::Ttft, 6, 120, 18_000.0)),
                e2e_ms: Some(hist(HistogramKind::E2e, 7, 120, 480_000.0)),
                tpot_ms: Some(hist(HistogramKind::Tpot, 1, 120, 2_820.0)),
                running_peak: Some(8),
                waiting_peak: Some(1),
                kv_cache_utilization_peak: Some(0.5),
                preemptions: Some(0),
                gpu_utilization: Some(0.75),
                gpu_memory_peak_bytes: Some(70 << 30),
                host_memory_peak_bytes: Some(12 << 30),
            },
        }
    }

    fn batch() -> TelemetryBatch {
        TelemetryBatch {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            batch_id: BatchId::from_random_bytes([3; 16]),
            reports: vec![report()],
        }
    }

    fn collect_keys(v: &serde_json::Value, out: &mut BTreeSet<String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    out.insert(k.clone());
                    collect_keys(v, out);
                }
            }
            serde_json::Value::Array(a) => a.iter().for_each(|v| collect_keys(v, out)),
            _ => {}
        }
    }

    #[test]
    fn golden_batch_matches_the_checked_in_example() {
        let ours = serde_json::to_value(batch()).unwrap();
        let golden: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        assert_eq!(
            ours,
            golden,
            "wire format changed: update the example and schema (and bump the version)\n{}",
            serde_json::to_string_pretty(&ours).unwrap()
        );
        assert_eq!(TelemetryBatch::from_json(GOLDEN).unwrap(), batch());
    }

    fn schema_properties(v: &serde_json::Value, out: &mut BTreeSet<String>) {
        if let serde_json::Value::Object(m) = v {
            for (k, v) in m {
                if k == "properties" {
                    out.extend(v.as_object().unwrap().keys().cloned());
                }
                schema_properties(v, out);
            }
        }
    }

    #[test]
    fn json_schema_names_exactly_the_payload_fields() {
        // Every field of a fully-populated batch is declared in the schema, and
        // the schema declares nothing the types do not have.
        let mut payload = BTreeSet::new();
        collect_keys(&serde_json::to_value(batch()).unwrap(), &mut payload);
        let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        let mut declared = BTreeSet::new();
        schema_properties(&schema, &mut declared);
        assert_eq!(payload, declared);
        assert_eq!(
            schema["properties"]["schema_version"]["const"],
            TELEMETRY_SCHEMA_VERSION
        );
        // The fixed histogram layouts are published in the schema too.
        for (name, kind) in [
            ("ttft_bounds_ms", HistogramKind::Ttft),
            ("e2e_bounds_ms", HistogramKind::E2e),
            ("tpot_bounds_ms", HistogramKind::Tpot),
        ] {
            let listed: Vec<f64> = serde_json::from_value(schema["x-amicu"][name].clone()).unwrap();
            assert_eq!(listed, kind.bounds_ms(), "{name}");
        }
    }

    #[test]
    fn round_trips_and_is_compact() {
        let r = report();
        let json = r.to_json().unwrap();
        assert!(!json.contains('\n') && !json.contains(": "));
        assert_eq!(TelemetryReport::from_json(&json).unwrap(), r);
        let mut minimal = report();
        minimal.provider = None;
        minimal.metrics.gpu_utilization = None;
        minimal.metrics.requests = None;
        minimal.metrics.errors = None;
        let json = minimal.to_json().unwrap();
        assert!(!json.contains("gpu_utilization") && !json.contains("\"requests\""));
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
            vec!["metrics", "e2e_ms"],
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
        let mut b = serde_json::to_value(batch()).unwrap();
        b["hostname"] = "box".into();
        assert!(TelemetryBatch::from_json(&b.to_string()).is_err());
    }

    #[test]
    fn no_forbidden_field_exists_anywhere_in_the_payload() {
        let mut keys = BTreeSet::new();
        collect_keys(&serde_json::to_value(batch()).unwrap(), &mut keys);
        for key in &keys {
            for exact in ["prompt", "ip", "key", "host", "user", "token", "text"] {
                assert_ne!(key, exact, "field `{key}`");
            }
            for part in [
                "completion",
                "content",
                "secret",
                "password",
                "api_key",
                "address",
                "hostname",
                "username",
                "path",
                "url",
                "endpoint",
                "message",
            ] {
                assert!(!key.contains(part), "field `{key}` looks like `{part}`");
            }
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
            "custom",
        ] {
            assert!(Label::new(good).is_ok(), "{good:?} must be accepted");
        }
        let mut v = serde_json::to_value(report()).unwrap();
        v["provider"] = "10.1.2.3".into();
        assert!(TelemetryReport::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn numbers_are_bounds_checked() {
        let checks: Vec<fn(&mut TelemetryReport)> = vec![
            |r| r.metrics.errors = Some(r.metrics.requests.unwrap() + 1),
            |r| r.metrics.requests = Some(u64::MAX),
            |r| r.metrics.gpu_utilization = Some(1.5),
            |r| r.metrics.kv_cache_utilization_peak = Some(-0.1),
            |r| r.metrics.output_tokens_per_s = f64::NAN,
            |r| r.metrics.decode_tokens_per_s = Some(1e12),
            |r| r.metrics.output_tokens = u64::MAX,
            |r| r.metrics.gpu_memory_peak_bytes = Some(u64::MAX),
            |r| r.metrics.running_peak = Some(u32::MAX),
            |r| {
                r.metrics.ttft_ms.as_mut().unwrap().counts.pop();
            },
            |r| r.metrics.e2e_ms.as_mut().unwrap().sum_ms = f64::INFINITY,
            |r| r.metrics.tpot_ms.as_mut().unwrap().sum_ms = 1e18,
            |r| r.window.duration_s = 0,
            |r| r.window.start_unix_s = 0,
            |r| r.window.start_unix_s = u64::MAX,
            |r| r.context_length = Some(u32::MAX),
            |r| r.batch_size = Some(u32::MAX),
            |r| r.hardware.accelerator_count = 5_000,
            |r| r.hardware.vram_bytes = Some(u64::MAX),
            |r| r.hardware.cpu_cores = Some(10_000_000),
            |r| r.hardware.accelerator_count = 0,
        ];
        for (i, check) in checks.iter().enumerate() {
            let mut r = report();
            check(&mut r);
            assert!(r.validate().is_err(), "case {i}");
            assert!(r.to_json().is_err(), "invalid reports are never serialized");
        }
        let mut r = report();
        r.hardware.accelerator = None; // model label dropped client-side: fine
        assert!(r.validate().is_ok());
        let mut r = report();
        r.schema_version = 1;
        assert_eq!(r.validate(), Err(ValidationError::UnsupportedSchema(1)));
    }

    #[test]
    fn histograms_rebucket_exactly_onto_matching_bounds() {
        // vLLM-style TTFT buckets in seconds; 0.25 and 1.0 are fixed bounds.
        let src = [
            (0.1, 2.0),
            (0.25, 5.0),
            (0.3, 6.0),
            (1.0, 9.0),
            (f64::INFINITY, 10.0),
        ];
        let h = Histogram::from_cumulative(HistogramKind::Ttft, &src, 4.5).unwrap();
        assert_eq!(h.counts.len(), TTFT_BOUNDS_MS.len() + 1);
        assert_eq!(h.total(), 10);
        let at = |ms: f64| {
            TTFT_BOUNDS_MS
                .iter()
                .position(|b| (*b - ms).abs() < 1e-9)
                .unwrap()
        };
        assert_eq!(h.counts[at(100.0)], 2);
        assert_eq!(h.counts[at(250.0)], 3);
        // 0.3 s is not a fixed bound: its observations move up to 500 ms.
        assert_eq!(h.counts[at(500.0)], 1);
        assert_eq!(h.counts[at(1000.0)], 3);
        assert_eq!(*h.counts.last().unwrap(), 1, "overflow bucket");
        assert!((h.sum_ms - 4500.0).abs() < 1e-9);
        assert!(Histogram::from_cumulative(HistogramKind::Ttft, &[], 0.0).is_none());
    }

    #[test]
    fn histogram_deltas_detect_restarts_and_merge() {
        let a = hist(HistogramKind::Tpot, 2, 5, 100.0);
        let b = hist(HistogramKind::Tpot, 2, 8, 160.0);
        let d = b.delta_since(&a).unwrap();
        assert_eq!(d.counts[2], 3);
        assert!((d.sum_ms - 60.0).abs() < 1e-9);
        assert!(a.delta_since(&b).is_none(), "a counter went down: restart");
        let mut m = d.clone();
        m.merge(&d);
        assert_eq!(m.total(), 6);
    }

    #[test]
    fn batches_are_capped_and_strict() {
        let mut b = batch();
        b.reports.clear();
        assert!(b.to_json().is_err(), "empty batch");
        let mut b = batch();
        b.reports = vec![report(); MAX_BATCH_REPORTS + 1];
        assert!(b.to_json().is_err(), "too many reports");
        // Size cap: a batch over 64 KB never serializes, and is refused on decode.
        let mut b = batch();
        b.reports = vec![report(); 100];
        assert!(serde_json::to_string(&b).unwrap().len() > MAX_BATCH_BYTES);
        assert_eq!(b.to_json(), Err(ValidationError::BadNumber("batch size")));
        let big = serde_json::to_string(&b).unwrap();
        assert!(TelemetryBatch::from_json(&big).is_err());
        let mut b = batch();
        b.reports[0].metrics.gpu_utilization = Some(2.0);
        assert!(b.to_json().is_err(), "every report is validated");
    }

    #[test]
    fn optional_labels_are_coarsened_or_dropped_never_fatal() {
        let rt = RuntimeInfo::new(Label::new("vllm").unwrap()).with_version("12.4.1.0");
        assert_eq!(rt.version.as_ref().map(Label::as_str), Some("12.4.1"));
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
        let rt = RuntimeInfo::new(Label::new("vllm").unwrap()).with_version("0.6.3.1");
        assert_eq!(rt.version.as_ref().map(Label::as_str), Some("0.6.3.1"));
        for bad in ["sk-proj-abc123", "has spaces in it", "user@host", ""] {
            let rt = RuntimeInfo::new(Label::new("vllm").unwrap()).with_version(bad);
            assert!(rt.version.is_none(), "{bad:?}");
            let m = ModelInfo::new(Label::new("m").unwrap()).with_quantization(bad);
            assert!(m.quantization.is_none(), "{bad:?}");
        }
        let hw = HardwareInfo::default().with_accelerator("hf_notamodel", 2);
        assert!(hw.accelerator.is_none() && hw.accelerator_count == 2);
        let mut r = report();
        r.runtime = RuntimeInfo::new(Label::new("vllm").unwrap()).with_version("sk-secret-1");
        r.model.quantization = Label::sanitized("a b");
        let r = r.with_provider("10.1.2.3").with_region("nbg1");
        assert!(r.provider.is_none());
        assert!(Label::sanitized("192.168.0.1").is_none());
        assert_eq!(
            TelemetryReport::from_json(&r.to_json().unwrap()).unwrap(),
            r
        );
        let mut v = serde_json::to_value(&r).unwrap();
        v["runtime"]["version"] = "10.0.0.1".into();
        assert!(TelemetryReport::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn newer_schema_is_unsupported_not_malformed() {
        let mut v = serde_json::to_value(report()).unwrap();
        v["schema_version"] = 3.into();
        v["a_field_from_the_future"] = 1.into();
        assert_eq!(
            TelemetryReport::from_json(&v.to_string()),
            Err(ValidationError::UnsupportedSchema(3))
        );
        let mut v = serde_json::to_value(report()).unwrap();
        v.as_object_mut().unwrap().remove("schema_version");
        assert!(matches!(
            TelemetryReport::from_json(&v.to_string()),
            Err(ValidationError::Malformed(_))
        ));
    }

    #[test]
    fn error_rate() {
        let r = Rollup {
            requests: Some(200),
            errors: Some(5),
            ..Rollup::default()
        };
        assert!((r.error_rate() - 0.025).abs() < 1e-12);
        assert!(Rollup::default().error_rate().abs() < 1e-12);
    }
}
