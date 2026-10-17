//! Scraping a deployed Modal server's Prometheus `/metrics` endpoint and
//! folding it into amicu's telemetry shapes.
//!
//! Two runtimes are recognised, by the prefix of the series they expose:
//!
//! **vLLM** (`vllm:`; on by default, `GET /metrics`). Names verified against
//! `vllm/v1/metrics/loggers.py` and `docs/design/metrics.md`. Counters carry
//! the `_total` suffix prometheus-client appends on exposition.
//! * `vllm:request_success_total{finished_reason=..}` - summed over reasons
//!   (including `abort`) for the request count.
//! * `vllm:e2e_request_latency_seconds` - histogram; p50/p90/p99 are
//!   interpolated from the cumulative buckets (what PromQL's
//!   `histogram_quantile` does). Likewise
//!   `vllm:time_to_first_token_seconds` and
//!   `vllm:request_time_per_output_token_seconds`.
//! * `vllm:generation_tokens_total`, `vllm:prompt_tokens_total` - counters.
//!   vLLM has no throughput gauge, so tokens/s is the counter delta between
//!   two scrapes ([`MetricsSnapshot::rate_since`]).
//! * `vllm:num_requests_running` / `vllm:num_requests_waiting` - gauges.
//! * `vllm:kv_cache_usage_perc` (0..1). Releases before 0.11 exposed it as
//!   `vllm:gpu_cache_usage_perc` (deprecated in 0.9, removed later), so both
//!   are read, new name first.
//! * `vllm:num_preemptions_total` - counter.
//!
//! **llama.cpp** (`llamacpp:`; only served with `--metrics`, which
//! `amicu-llamacpp` now always renders). Names verified against
//! `tools/server/server-task.cpp` (`server_task_result_metrics::to_metrics`),
//! identical at the image tag amicu pins (b11515) and `master`.
//! * `llamacpp:requests_processing` / `llamacpp:requests_deferred` - gauges.
//! * `llamacpp:tokens_predicted_total`, `llamacpp:prompt_tokens_total` -
//!   counters.
//! * `llamacpp:predicted_tokens_seconds`, `llamacpp:prompt_tokens_seconds` -
//!   gauges, averaged over the window since the previous scrape (reading
//!   resets it).
//! * `llamacpp:kv_cache_usage_ratio` - present on older builds only; recent
//!   ones dropped it, so it is read when there and `None` otherwise.
//!
//! llama.cpp exposes no request counter and no latency histogram, so
//! [`MetricsSnapshot::requests_total`] and the percentiles are `None` there.
//!
//! Every request goes through [`crate::proxy_auth::apply`]; errors carry only
//! the status code, a coarse transport reason and the auth hint - never a
//! header value.

use std::time::Duration;

use amicu_core::ByteSize;
use amicu_core::id::DeploymentId;
use amicu_runtime::RuntimeMetrics;
use amicu_telemetry::prometheus::{cumulative_buckets, first_sum, sum};
use amicu_telemetry::{Distribution, StackMetrics};

pub use amicu_telemetry::prometheus::{Sample, parse_prometheus};

/// Request timeout. Generous because a scaled-to-zero container cold-starts
/// on the first request.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(30);
/// Refuse absurdly large bodies rather than buffering them.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Which runtime produced a scrape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsRuntime {
    Vllm,
    LlamaCpp,
}

impl MetricsRuntime {
    /// Human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Vllm => "vLLM",
            Self::LlamaCpp => "llama.cpp",
        }
    }
}

/// Why a scrape failed. `Display` never contains a header value or token.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    /// Modal's proxy answered 401/403; carries the actionable hint.
    #[error("{0}")]
    Auth(String),
    /// The runtime answered but does not serve metrics (llama-server started
    /// without `--metrics`).
    #[error(
        "the server does not expose /metrics (HTTP {0}); llama-server needs `--metrics` - \
         redeploy with a current amicu"
    )]
    NotEnabled(u16),
    /// Any other non-2xx answer.
    #[error("metrics request failed with HTTP {0}{note}", note = status_note(*.0))]
    Status(u16),
    /// The request never completed.
    #[error("could not reach the metrics endpoint: {0}")]
    Unreachable(String),
    /// The body was not usable Prometheus text.
    #[error("the response is not Prometheus text (body too large or not UTF-8)")]
    BadBody,
    /// Parsed, but no `vllm:` or `llamacpp:` series were present.
    #[error("the endpoint exposes no recognised vLLM or llama.cpp series")]
    Unrecognized,
}

fn status_note(code: u16) -> &'static str {
    match code {
        502..=504 => " (the container may be cold-starting; retry shortly)",
        _ => "",
    }
}

/// p50/p90/p99 in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Percentiles {
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
}

impl Percentiles {
    fn distribution(self) -> Distribution {
        Distribution::new(self.p50_ms, self.p90_ms, self.p99_ms)
    }
}

/// The parts of one scrape amicu cares about. `None` means "this runtime/build
/// does not expose it", which is different from zero.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricsSnapshot {
    pub runtime: MetricsRuntime,
    /// Finished requests since process start (vLLM only).
    pub requests_total: Option<u64>,
    pub requests_running: Option<u32>,
    pub requests_waiting: Option<u32>,
    pub prompt_tokens_total: Option<u64>,
    pub generation_tokens_total: Option<u64>,
    /// Generation throughput. Direct for llama.cpp (gauge); for vLLM only
    /// after [`MetricsSnapshot::rate_since`].
    pub generation_tokens_per_second: Option<f64>,
    pub prompt_tokens_per_second: Option<f64>,
    /// KV-cache fill in `[0, 1]`.
    pub kv_cache_usage: Option<f64>,
    pub preemptions: Option<u64>,
    pub e2e_latency: Option<Percentiles>,
    pub time_to_first_token: Option<Percentiles>,
    pub time_per_output_token: Option<Percentiles>,
}

impl MetricsSnapshot {
    /// Fills throughput from the counter delta against an `earlier` scrape
    /// taken `elapsed` before this one. Leaves an existing gauge value alone.
    #[must_use]
    pub fn rate_since(mut self, earlier: &Self, elapsed: Duration) -> Self {
        let secs = elapsed.as_secs_f64();
        if secs <= 0.0 {
            return self;
        }
        let rate = |now: Option<u64>, before: Option<u64>| -> Option<f64> {
            // A counter going backwards means the process restarted.
            let (now, before) = (now?, before?);
            #[allow(clippy::cast_precision_loss)]
            (now >= before).then(|| (now - before) as f64 / secs)
        };
        if self.generation_tokens_per_second.is_none() {
            self.generation_tokens_per_second = rate(
                self.generation_tokens_total,
                earlier.generation_tokens_total,
            );
        }
        if self.prompt_tokens_per_second.is_none() {
            self.prompt_tokens_per_second =
                rate(self.prompt_tokens_total, earlier.prompt_tokens_total);
        }
        self
    }

    /// The coarse [`RuntimeMetrics`] the `ProcessSupervisor` trait returns.
    /// Absent values become zero. `memory_used` is zero: runtimes report KV
    /// fill as a ratio, not bytes (see [`MetricsSnapshot::kv_cache_usage`]).
    #[must_use]
    pub fn to_runtime_metrics(&self) -> RuntimeMetrics {
        RuntimeMetrics {
            requests_total: self.requests_total.unwrap_or(0),
            latency_p50_ms: self.e2e_latency.map_or(0.0, |p| p.p50_ms),
            tokens_per_second: self.generation_tokens_per_second.unwrap_or(0.0),
            memory_used: ByteSize::from_bytes(0),
        }
    }

    /// The richer [`StackMetrics`] surface, filling what the runtime exposes
    /// and leaving the rest (GPU, cost, reliability) at its default.
    #[must_use]
    pub fn to_stack_metrics(&self, deployment: DeploymentId, window_seconds: u64) -> StackMetrics {
        let mut m = StackMetrics::empty(deployment, window_seconds);
        let dist = |p: Option<Percentiles>| p.map(Percentiles::distribution).unwrap_or_default();
        m.latency.e2e_ms = dist(self.e2e_latency);
        m.latency.ttft_ms = dist(self.time_to_first_token);
        m.latency.tpot_ms = dist(self.time_per_output_token);
        m.throughput.output_tokens_per_s = self.generation_tokens_per_second.unwrap_or(0.0);
        m.throughput.prompt_tokens_per_s = self.prompt_tokens_per_second.unwrap_or(0.0);
        m.concurrency.in_flight_requests = self.requests_running.unwrap_or(0);
        m.queue.depth = self.requests_waiting.unwrap_or(0);
        m.memory.kv_cache_utilization = self.kv_cache_usage.unwrap_or(0.0);
        m.memory.preemptions = self.preemptions.unwrap_or(0);
        m
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn as_u64(v: f64) -> u64 {
    if v.is_finite() && v > 0.0 {
        v as u64
    } else {
        0
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn as_u32(v: f64) -> u32 {
    if v.is_finite() && v > 0.0 {
        v.min(f64::from(u32::MAX)) as u32
    } else {
        0
    }
}

/// p50/p90/p99 (milliseconds) of the histogram `base` (seconds), merged
/// across label sets, interpolating linearly inside the matching bucket.
fn histogram_percentiles(samples: &[Sample], base: &str) -> Option<Percentiles> {
    let buckets = cumulative_buckets(samples, base);
    let total = buckets.last().map(|b| b.1).filter(|t| *t > 0.0)?;
    let quantile = |q: f64| -> f64 {
        let target = q * total;
        let (mut prev_bound, mut prev_count) = (0.0_f64, 0.0_f64);
        for &(bound, count) in &buckets {
            if count >= target {
                if bound.is_infinite() {
                    return prev_bound;
                }
                let span = count - prev_count;
                if span <= 0.0 {
                    return bound;
                }
                return prev_bound + (bound - prev_bound) * ((target - prev_count) / span);
            }
            if bound.is_finite() {
                prev_bound = bound;
            }
            prev_count = count;
        }
        prev_bound
    };
    Some(Percentiles {
        p50_ms: quantile(0.5) * 1000.0,
        p90_ms: quantile(0.9) * 1000.0,
        p99_ms: quantile(0.99) * 1000.0,
    })
}

/// Builds a [`MetricsSnapshot`] from exposition text.
///
/// # Errors
/// [`MetricsError::Unrecognized`] when neither runtime's series are present.
pub fn snapshot_from_text(text: &str) -> Result<MetricsSnapshot, MetricsError> {
    let samples = parse_prometheus(text);
    let has = |prefix: &str| samples.iter().any(|s| s.name.starts_with(prefix));
    if has("vllm:") {
        Ok(vllm_snapshot(&samples))
    } else if has("llamacpp:") {
        Ok(llamacpp_snapshot(&samples))
    } else {
        Err(MetricsError::Unrecognized)
    }
}

fn vllm_snapshot(s: &[Sample]) -> MetricsSnapshot {
    MetricsSnapshot {
        runtime: MetricsRuntime::Vllm,
        requests_total: first_sum(s, &["vllm:request_success_total", "vllm:request_success"])
            .map(as_u64),
        requests_running: sum(s, "vllm:num_requests_running").map(as_u32),
        requests_waiting: sum(s, "vllm:num_requests_waiting").map(as_u32),
        prompt_tokens_total: first_sum(s, &["vllm:prompt_tokens_total", "vllm:prompt_tokens"])
            .map(as_u64),
        generation_tokens_total: first_sum(
            s,
            &["vllm:generation_tokens_total", "vllm:generation_tokens"],
        )
        .map(as_u64),
        generation_tokens_per_second: None,
        prompt_tokens_per_second: None,
        // Gauge: several engines would sum past 1, so average instead.
        kv_cache_usage: vllm_kv_usage(s),
        preemptions: first_sum(s, &["vllm:num_preemptions_total", "vllm:num_preemptions"])
            .map(as_u64),
        e2e_latency: histogram_percentiles(s, "vllm:e2e_request_latency_seconds"),
        time_to_first_token: histogram_percentiles(s, "vllm:time_to_first_token_seconds"),
        time_per_output_token: histogram_percentiles(
            s,
            "vllm:request_time_per_output_token_seconds",
        ),
    }
}

/// Mean of the KV-cache gauge across engines; new name first, then the
/// pre-0.11 `gpu_cache_usage_perc`.
fn vllm_kv_usage(s: &[Sample]) -> Option<f64> {
    ["vllm:kv_cache_usage_perc", "vllm:gpu_cache_usage_perc"]
        .iter()
        .find_map(|name| {
            let values: Vec<f64> = s
                .iter()
                .filter(|x| x.name == *name && x.value.is_finite())
                .map(|x| x.value)
                .collect();
            #[allow(clippy::cast_precision_loss)]
            (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
        })
}

fn llamacpp_snapshot(s: &[Sample]) -> MetricsSnapshot {
    MetricsSnapshot {
        runtime: MetricsRuntime::LlamaCpp,
        requests_total: None,
        requests_running: sum(s, "llamacpp:requests_processing").map(as_u32),
        requests_waiting: sum(s, "llamacpp:requests_deferred").map(as_u32),
        prompt_tokens_total: sum(s, "llamacpp:prompt_tokens_total").map(as_u64),
        generation_tokens_total: sum(s, "llamacpp:tokens_predicted_total").map(as_u64),
        generation_tokens_per_second: sum(s, "llamacpp:predicted_tokens_seconds"),
        prompt_tokens_per_second: sum(s, "llamacpp:prompt_tokens_seconds"),
        kv_cache_usage: sum(s, "llamacpp:kv_cache_usage_ratio"),
        preemptions: None,
        e2e_latency: None,
        time_to_first_token: None,
        time_per_output_token: None,
    }
}

/// `GET <endpoint>/metrics` through the Modal proxy (proxy-auth headers
/// attached from the environment) and parse the result.
///
/// # Errors
/// See [`MetricsError`]; none of its messages contain a header or token.
pub async fn scrape(endpoint: &str) -> Result<MetricsSnapshot, MetricsError> {
    let url = format!("{}/metrics", endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(SCRAPE_TIMEOUT)
        .build()
        .map_err(|e| MetricsError::Unreachable(transport_reason(&e)))?;
    let response = crate::proxy_auth::apply(client.get(url))
        .send()
        .await
        .map_err(|e| MetricsError::Unreachable(transport_reason(&e)))?;
    let status = response.status().as_u16();
    if let Some(hint) = crate::proxy_auth::hint_for_status(status, crate::proxy_auth::env_state()) {
        return Err(MetricsError::Auth(hint));
    }
    match status {
        200..=299 => {}
        404 | 501 => return Err(MetricsError::NotEnabled(status)),
        other => return Err(MetricsError::Status(other)),
    }
    // Enforce the cap while streaming: `bytes()` would buffer a hostile or
    // runaway body in full before any length check could run.
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Err(MetricsError::BadBody);
    }
    let mut response = response;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| MetricsError::Unreachable(transport_reason(&e)))?
    {
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(MetricsError::BadBody);
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| MetricsError::BadBody)?;
    snapshot_from_text(text)
}

/// A coarse, URL-free reason for a transport failure.
fn transport_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else {
        "request failed".to_owned()
    }
}

/// Takes a throughput-bearing snapshot: scrapes twice `window` apart so a
/// runtime without a throughput gauge (vLLM) still gets tokens/s. A runtime
/// that has the gauge (llama.cpp) is returned after the first scrape.
///
/// # Errors
/// As [`scrape`].
pub async fn scrape_with_rate(
    endpoint: &str,
    window: Duration,
) -> Result<MetricsSnapshot, MetricsError> {
    let first = scrape(endpoint).await?;
    if first.generation_tokens_per_second.is_some() {
        return Ok(first);
    }
    let started = std::time::Instant::now();
    tokio::time::sleep(window).await;
    let second = scrape(endpoint).await?;
    Ok(second.rate_since(&first, started.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape of a vLLM v0.11+ `/metrics` response (names per
    /// `docs/design/metrics.md`; histogram trimmed to a few buckets).
    const VLLM: &str = r#"# HELP python_gc_objects_collected_total Objects collected during gc
# TYPE python_gc_objects_collected_total counter
python_gc_objects_collected_total{generation="0"} 1234.0
# HELP vllm:num_requests_running Number of requests in model execution batches.
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{engine="0",model_name="Qwen/Qwen3-8B"} 3.0
# TYPE vllm:num_requests_waiting gauge
vllm:num_requests_waiting{engine="0",model_name="Qwen/Qwen3-8B"} 2.0
# TYPE vllm:kv_cache_usage_perc gauge
vllm:kv_cache_usage_perc{engine="0",model_name="Qwen/Qwen3-8B"} 0.25
# TYPE vllm:num_preemptions_total counter
vllm:num_preemptions_total{engine="0",model_name="Qwen/Qwen3-8B"} 4.0
# TYPE vllm:prompt_tokens_total counter
vllm:prompt_tokens_total{engine="0",model_name="Qwen/Qwen3-8B"} 5000.0
# TYPE vllm:generation_tokens_total counter
vllm:generation_tokens_total{engine="0",model_name="Qwen/Qwen3-8B"} 27453.0
# TYPE vllm:request_success_total counter
vllm:request_success_total{engine="0",finished_reason="stop",model_name="Qwen/Qwen3-8B"} 1.0
vllm:request_success_total{engine="0",finished_reason="length",model_name="Qwen/Qwen3-8B"} 131.0
vllm:request_success_total{engine="0",finished_reason="abort",model_name="Qwen/Qwen3-8B"} 2.0
# TYPE vllm:e2e_request_latency_seconds histogram
vllm:e2e_request_latency_seconds_bucket{engine="0",le="0.5",model_name="Qwen/Qwen3-8B"} 0.0
vllm:e2e_request_latency_seconds_bucket{engine="0",le="1.0",model_name="Qwen/Qwen3-8B"} 50.0
vllm:e2e_request_latency_seconds_bucket{engine="0",le="2.5",model_name="Qwen/Qwen3-8B"} 90.0
vllm:e2e_request_latency_seconds_bucket{engine="0",le="5.0",model_name="Qwen/Qwen3-8B"} 100.0
vllm:e2e_request_latency_seconds_bucket{engine="0",le="+Inf",model_name="Qwen/Qwen3-8B"} 100.0
vllm:e2e_request_latency_seconds_sum{engine="0",model_name="Qwen/Qwen3-8B"} 150.0
vllm:e2e_request_latency_seconds_count{engine="0",model_name="Qwen/Qwen3-8B"} 100.0
"#;

    /// llama.cpp `--metrics` output, same names as `to_metrics()` in
    /// `tools/server/server-task.cpp` (b11515 and master).
    const LLAMACPP: &str = r"# HELP llamacpp:prompt_tokens_total Number of prompt tokens processed, excluding cached tokens
# TYPE llamacpp:prompt_tokens_total counter
llamacpp:prompt_tokens_total 1200
# HELP llamacpp:tokens_predicted_total Number of generation tokens processed
# TYPE llamacpp:tokens_predicted_total counter
llamacpp:tokens_predicted_total 3456
# HELP llamacpp:n_decode_total Total number of llama_decode() calls
# TYPE llamacpp:n_decode_total counter
llamacpp:n_decode_total 3400
# HELP llamacpp:prompt_tokens_seconds Average prompt throughput in tokens/s
# TYPE llamacpp:prompt_tokens_seconds gauge
llamacpp:prompt_tokens_seconds 812.5
# HELP llamacpp:predicted_tokens_seconds Average generation throughput in tokens/s
# TYPE llamacpp:predicted_tokens_seconds gauge
llamacpp:predicted_tokens_seconds 41.25
# HELP llamacpp:requests_processing Number of requests processing
# TYPE llamacpp:requests_processing gauge
llamacpp:requests_processing 1
# HELP llamacpp:requests_deferred Number of requests deferred
# TYPE llamacpp:requests_deferred gauge
llamacpp:requests_deferred 0
";

    #[test]
    fn vllm_fixture_maps_every_field() {
        let m = snapshot_from_text(VLLM).expect("vllm");
        assert_eq!(m.runtime, MetricsRuntime::Vllm);
        assert_eq!(m.requests_total, Some(134));
        assert_eq!(m.requests_running, Some(3));
        assert_eq!(m.requests_waiting, Some(2));
        assert_eq!(m.prompt_tokens_total, Some(5000));
        assert_eq!(m.generation_tokens_total, Some(27453));
        assert_eq!(m.preemptions, Some(4));
        assert_eq!(m.kv_cache_usage, Some(0.25));
        assert_eq!(m.generation_tokens_per_second, None, "needs two scrapes");
        // 100 obs: p50 target 50 lands exactly on the le=1.0 bucket edge.
        let e2e = m.e2e_latency.expect("histogram");
        assert!((e2e.p50_ms - 1000.0).abs() < 1e-6, "{e2e:?}");
        // p90 target 90 is the le=2.5 edge; p99 interpolates in (2.5, 5.0].
        assert!((e2e.p90_ms - 2500.0).abs() < 1e-6, "{e2e:?}");
        assert!((e2e.p99_ms - 4750.0).abs() < 1e-6, "{e2e:?}");
        assert!(m.time_to_first_token.is_none());
    }

    #[test]
    fn vllm_old_kv_cache_name_is_still_read() {
        let old = "vllm:gpu_cache_usage_perc{model_name=\"m\"} 0.5\n";
        assert_eq!(snapshot_from_text(old).unwrap().kv_cache_usage, Some(0.5));
        let both = "vllm:gpu_cache_usage_perc 0.9\nvllm:kv_cache_usage_perc 0.1\n";
        assert_eq!(
            snapshot_from_text(both).unwrap().kv_cache_usage,
            Some(0.1),
            "new name wins"
        );
    }

    #[test]
    fn vllm_counters_without_total_suffix_are_accepted() {
        let t = "vllm:request_success{finished_reason=\"stop\"} 7\nvllm:generation_tokens 9\n";
        let m = snapshot_from_text(t).unwrap();
        assert_eq!(m.requests_total, Some(7));
        assert_eq!(m.generation_tokens_total, Some(9));
    }

    #[test]
    fn llamacpp_fixture_maps_what_it_exposes() {
        let m = snapshot_from_text(LLAMACPP).expect("llama.cpp");
        assert_eq!(m.runtime, MetricsRuntime::LlamaCpp);
        assert_eq!(m.requests_total, None, "llama.cpp has no request counter");
        assert_eq!(m.requests_running, Some(1));
        assert_eq!(m.requests_waiting, Some(0));
        assert_eq!(m.prompt_tokens_total, Some(1200));
        assert_eq!(m.generation_tokens_total, Some(3456));
        assert_eq!(m.generation_tokens_per_second, Some(41.25));
        assert_eq!(m.prompt_tokens_per_second, Some(812.5));
        assert_eq!(m.kv_cache_usage, None);
        assert!(m.e2e_latency.is_none());
    }

    #[test]
    fn llamacpp_older_kv_ratio_is_read_when_present() {
        let t = "llamacpp:kv_cache_usage_ratio 0.03\n";
        assert_eq!(snapshot_from_text(t).unwrap().kv_cache_usage, Some(0.03));
    }

    #[test]
    fn non_finite_values_are_dropped_not_displayed() {
        let t = "vllm:kv_cache_usage_perc NaN\nllamacpp:x 1\nvllm:num_requests_running +Inf\n";
        let m = snapshot_from_text(t).unwrap();
        assert_eq!(m.kv_cache_usage, None);
        assert_eq!(m.requests_running, None);
    }

    #[test]
    fn unknown_text_is_unrecognized() {
        assert!(matches!(
            snapshot_from_text("process_cpu_seconds_total 3\n"),
            Err(MetricsError::Unrecognized)
        ));
    }

    #[test]
    fn rate_since_uses_the_counter_delta_and_ignores_resets() {
        let a = snapshot_from_text(VLLM).unwrap();
        let mut b = a.clone();
        b.generation_tokens_total = Some(27453 + 500);
        b.prompt_tokens_total = Some(5000 + 100);
        let r = b.clone().rate_since(&a, Duration::from_secs(10));
        assert_eq!(r.generation_tokens_per_second, Some(50.0));
        assert_eq!(r.prompt_tokens_per_second, Some(10.0));

        b.generation_tokens_total = Some(1); // restart
        let r = b.rate_since(&a, Duration::from_secs(10));
        assert_eq!(r.generation_tokens_per_second, None);

        // The llama.cpp gauge is never overwritten.
        let l = snapshot_from_text(LLAMACPP).unwrap();
        let r = l.clone().rate_since(&l, Duration::from_secs(5));
        assert_eq!(r.generation_tokens_per_second, Some(41.25));
    }

    #[test]
    fn maps_into_runtime_and_stack_metrics() {
        let mut m = snapshot_from_text(VLLM).unwrap();
        m.generation_tokens_per_second = Some(12.0);
        let rt = m.to_runtime_metrics();
        assert_eq!(rt.requests_total, 134);
        assert!((rt.latency_p50_ms - 1000.0).abs() < 1e-6);
        assert!((rt.tokens_per_second - 12.0).abs() < f64::EPSILON);

        let st = m.to_stack_metrics(DeploymentId::from("d1"), 60);
        assert_eq!(st.concurrency.in_flight_requests, 3);
        assert_eq!(st.queue.depth, 2);
        assert!((st.memory.kv_cache_utilization - 0.25).abs() < f64::EPSILON);
        assert_eq!(st.memory.preemptions, 4);
        assert!((st.throughput.output_tokens_per_s - 12.0).abs() < f64::EPSILON);
        assert!((st.latency.e2e_ms.p99 - 4750.0).abs() < 1e-6);

        let zero = snapshot_from_text(LLAMACPP).unwrap().to_runtime_metrics();
        assert_eq!(zero.requests_total, 0);
    }

    #[test]
    fn error_messages_carry_no_secret() {
        for e in [
            MetricsError::NotEnabled(501),
            MetricsError::Status(503),
            MetricsError::Unreachable("timed out".into()),
        ] {
            let s = format!("{e} {e:?}");
            assert!(!s.contains("Modal-Secret") && !s.contains("ws-"));
        }
        assert!(MetricsError::Status(503).to_string().contains("cold-start"));
    }

    /// Serves one canned HTTP response on a loopback port; returns its URL.
    async fn serve_once(status: &str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 2048];
            let _ = sock.read(&mut buf).await;
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(body.as_bytes()).await.unwrap();
        });
        url
    }

    #[tokio::test]
    async fn scrape_parses_a_200_and_classifies_failures() {
        let ok = scrape(&serve_once("200 OK", LLAMACPP).await).await.unwrap();
        assert_eq!(ok.runtime, MetricsRuntime::LlamaCpp);

        let off = serve_once(
            "501 Not Implemented",
            "{\"error\":\"Start it with --metrics\"}",
        );
        assert!(matches!(
            scrape(&off.await).await,
            Err(MetricsError::NotEnabled(501))
        ));

        let cold = serve_once("503 Service Unavailable", "");
        assert!(matches!(
            scrape(&cold.await).await,
            Err(MetricsError::Status(503))
        ));

        let junk = serve_once("200 OK", "hello\n");
        assert!(matches!(
            scrape(&junk.await).await,
            Err(MetricsError::Unrecognized)
        ));
    }
}
