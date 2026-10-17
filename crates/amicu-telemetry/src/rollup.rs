//! Folding periodic `/metrics` scrapes into 5-minute [`Rollup`]s.
//!
//! The telemetry agent reads the runtime's own Prometheus endpoint out of
//! band (it never sits in the request path) about once a minute. Runtime
//! counters are cumulative since the server started, so each window gets the
//! difference between consecutive scrapes; gauges contribute their peak.
//!
//! What is read, by runtime (names as in `amicu-modal`'s `metrics` module):
//!
//! | value | vLLM | llama.cpp (`--metrics`) |
//! |---|---|---|
//! | requests | `vllm:request_success_total` (all reasons) | none |
//! | errors | `vllm:request_success_total{finished_reason="abort"}` | none |
//! | prompt / output tokens | `vllm:prompt_tokens_total`, `vllm:generation_tokens_total` | `llamacpp:prompt_tokens_total`, `llamacpp:tokens_predicted_total` |
//! | TTFT / e2e / TPOT histograms | `vllm:time_to_first_token_seconds`, `vllm:e2e_request_latency_seconds`, `vllm:request_time_per_output_token_seconds` | none |
//! | decode tokens/s | from the TPOT histogram | `llamacpp:predicted_tokens_seconds` |
//! | running / waiting | `vllm:num_requests_running` / `_waiting` | `llamacpp:requests_processing` / `_deferred` |
//! | KV-cache use | `vllm:kv_cache_usage_perc` | `llamacpp:kv_cache_usage_ratio` (older builds) |
//! | preemptions | `vllm:num_preemptions_total` | none |
//! | server memory | `process_resident_memory_bytes` | none |
//!
//! Accelerator utilization and memory come from the hardware probe
//! ([`crate::hardware`]), not from the runtime.

use crate::hardware::HardwareSample;
use crate::prometheus::{Sample, cumulative_buckets, first_sum, max, parse_prometheus, sum};
use crate::report::{Histogram, HistogramKind, Rollup, Window};

/// Rollup window length, seconds.
pub const WINDOW_SECONDS: u32 = 300;

/// Which runtime a scrape came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrapeRuntime {
    Vllm,
    LlamaCpp,
}

/// One scrape, reduced to what a rollup needs. Counters are cumulative.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scrape {
    pub requests_total: Option<f64>,
    pub aborted_total: Option<f64>,
    pub prompt_tokens_total: Option<f64>,
    pub output_tokens_total: Option<f64>,
    pub preemptions_total: Option<f64>,
    pub ttft: Option<Histogram>,
    pub e2e: Option<Histogram>,
    pub tpot: Option<Histogram>,
    pub running: Option<f64>,
    pub waiting: Option<f64>,
    pub kv_usage: Option<f64>,
    /// llama.cpp's own decode-speed gauge (averaged since the last read).
    pub decode_tokens_per_s: Option<f64>,
    pub resident_bytes: Option<f64>,
}

impl Scrape {
    /// Parse `/metrics` text. `None` when it has neither runtime's series.
    #[must_use]
    pub fn from_text(text: &str) -> Option<(ScrapeRuntime, Self)> {
        let s = parse_prometheus(text);
        let has = |prefix: &str| s.iter().any(|x| x.name.starts_with(prefix));
        if has("vllm:") {
            Some((ScrapeRuntime::Vllm, Self::vllm(&s)))
        } else if has("llamacpp:") {
            Some((ScrapeRuntime::LlamaCpp, Self::llamacpp(&s)))
        } else {
            None
        }
    }

    fn vllm(s: &[Sample]) -> Self {
        let hist = |kind, names: &[&str]| {
            names.iter().find_map(|base| {
                let sum_s = sum(s, &format!("{base}_sum")).unwrap_or(0.0);
                Histogram::from_cumulative(kind, &cumulative_buckets(s, base), sum_s)
            })
        };
        let success = ["vllm:request_success_total", "vllm:request_success"];
        Self {
            requests_total: first_sum(s, &success),
            aborted_total: success.iter().find_map(|name| {
                let mut found = false;
                let total = s
                    .iter()
                    .filter(|x| x.name == *name && x.value.is_finite())
                    .inspect(|_| found = true)
                    .filter(|x| x.label("finished_reason") == Some("abort"))
                    .map(|x| x.value)
                    .sum::<f64>();
                found.then_some(total)
            }),
            prompt_tokens_total: first_sum(s, &["vllm:prompt_tokens_total", "vllm:prompt_tokens"]),
            output_tokens_total: first_sum(
                s,
                &["vllm:generation_tokens_total", "vllm:generation_tokens"],
            ),
            preemptions_total: first_sum(
                s,
                &["vllm:num_preemptions_total", "vllm:num_preemptions"],
            ),
            ttft: hist(HistogramKind::Ttft, &["vllm:time_to_first_token_seconds"]),
            e2e: hist(HistogramKind::E2e, &["vllm:e2e_request_latency_seconds"]),
            tpot: hist(
                HistogramKind::Tpot,
                &[
                    "vllm:request_time_per_output_token_seconds",
                    "vllm:time_per_output_token_seconds",
                ],
            ),
            running: sum(s, "vllm:num_requests_running"),
            waiting: sum(s, "vllm:num_requests_waiting"),
            kv_usage: ["vllm:kv_cache_usage_perc", "vllm:gpu_cache_usage_perc"]
                .iter()
                .find_map(|n| max(s, n)),
            decode_tokens_per_s: None,
            resident_bytes: sum(s, "process_resident_memory_bytes"),
        }
    }

    fn llamacpp(s: &[Sample]) -> Self {
        Self {
            prompt_tokens_total: sum(s, "llamacpp:prompt_tokens_total"),
            output_tokens_total: sum(s, "llamacpp:tokens_predicted_total"),
            running: sum(s, "llamacpp:requests_processing"),
            waiting: sum(s, "llamacpp:requests_deferred"),
            kv_usage: sum(s, "llamacpp:kv_cache_usage_ratio"),
            decode_tokens_per_s: sum(s, "llamacpp:predicted_tokens_seconds"),
            ..Self::default()
        }
    }
}

/// The window aggregate under construction.
#[derive(Debug, Clone, Default)]
struct Accum {
    requests: Option<u64>,
    errors: Option<u64>,
    prompt_tokens: u64,
    output_tokens: u64,
    preemptions: Option<u64>,
    ttft: Option<Histogram>,
    e2e: Option<Histogram>,
    tpot: Option<Histogram>,
    running_peak: Option<f64>,
    waiting_peak: Option<f64>,
    kv_peak: Option<f64>,
    decode_gauges: Vec<f64>,
    gpu_util: Vec<f64>,
    gpu_mem_peak: Option<u64>,
    resident_peak: Option<f64>,
}

/// A finished window, ready to become a report.
#[derive(Debug, Clone, PartialEq)]
pub struct ClosedWindow {
    pub window: Window,
    pub metrics: Rollup,
}

/// Folds scrapes into aligned [`WINDOW_SECONDS`] windows. Clock-free: the
/// caller passes the time of every observation.
#[derive(Debug, Default)]
pub struct WindowAggregator {
    previous: Option<Scrape>,
    current: Option<(u64, Accum)>,
}

/// `now - before` for a cumulative counter; a counter that went down means the
/// server restarted, so everything since the restart (`now`) is the delta.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn counter_delta(now: Option<f64>, before: Option<f64>) -> Option<u64> {
    let now = now?;
    let delta = match before {
        Some(b) if now >= b => now - b,
        _ => now,
    };
    (delta.is_finite() && delta >= 0.0).then_some(delta as u64)
}

fn add(slot: &mut Option<u64>, v: Option<u64>) {
    if let Some(v) = v {
        *slot = Some(slot.unwrap_or(0).saturating_add(v));
    }
}

fn peak(slot: &mut Option<f64>, v: Option<f64>) {
    if let Some(v) = v.filter(|v| v.is_finite() && *v >= 0.0) {
        *slot = Some(slot.map_or(v, |p| p.max(v)));
    }
}

fn hist_delta(acc: &mut Option<Histogram>, now: Option<&Histogram>, before: Option<&Histogram>) {
    let Some(now) = now else { return };
    let delta = before
        .and_then(|b| now.delta_since(b))
        .unwrap_or_else(|| now.clone());
    match acc {
        Some(h) => h.merge(&delta),
        None => *acc = Some(delta),
    }
}

impl WindowAggregator {
    /// A fresh aggregator: the first scrape is only a baseline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn window_start(now_s: u64) -> u64 {
        now_s - now_s % u64::from(WINDOW_SECONDS)
    }

    /// Close the current window if `now_s` is past its end. Idle windows (no
    /// tokens and no requests) are dropped, not reported.
    pub fn close_due(&mut self, now_s: u64) -> Option<ClosedWindow> {
        let (start, _) = self.current.as_ref()?;
        if Self::window_start(now_s) == *start {
            return None;
        }
        let (start, acc) = self.current.take()?;
        finish(start, acc)
    }

    /// Record one scrape (`None`: the scrape failed) and one hardware sample
    /// taken at `now_s`. Returns the previous window when this observation
    /// starts a new one.
    pub fn observe(
        &mut self,
        now_s: u64,
        scrape: Option<Scrape>,
        hardware: Option<&HardwareSample>,
    ) -> Option<ClosedWindow> {
        let closed = self.close_due(now_s);
        let start = Self::window_start(now_s);
        let acc = &mut self
            .current
            .get_or_insert_with(|| (start, Accum::default()))
            .1;
        if let Some(hw) = hardware {
            let utils: Vec<f64> = hw
                .accelerators
                .iter()
                .filter_map(|a| a.util_pct)
                .filter(|u| u.is_finite())
                .collect();
            if !utils.is_empty() {
                #[allow(clippy::cast_precision_loss)]
                let mean = utils.iter().sum::<f64>() / utils.len() as f64;
                acc.gpu_util.push((mean / 100.0).clamp(0.0, 1.0));
            }
            if let Some(used) = hw.accelerator_memory_used() {
                acc.gpu_mem_peak = Some(acc.gpu_mem_peak.map_or(used, |p| p.max(used)));
            }
        }
        let Some(now) = scrape else {
            return closed;
        };
        peak(&mut acc.running_peak, now.running);
        peak(&mut acc.waiting_peak, now.waiting);
        peak(&mut acc.kv_peak, now.kv_usage);
        peak(&mut acc.resident_peak, now.resident_bytes);
        if let Some(g) = now
            .decode_tokens_per_s
            .filter(|g| g.is_finite() && *g > 0.0)
        {
            acc.decode_gauges.push(g);
        }
        if let Some(before) = &self.previous {
            add(
                &mut acc.requests,
                counter_delta(now.requests_total, before.requests_total),
            );
            add(
                &mut acc.errors,
                counter_delta(now.aborted_total, before.aborted_total),
            );
            acc.prompt_tokens +=
                counter_delta(now.prompt_tokens_total, before.prompt_tokens_total).unwrap_or(0);
            acc.output_tokens +=
                counter_delta(now.output_tokens_total, before.output_tokens_total).unwrap_or(0);
            add(
                &mut acc.preemptions,
                counter_delta(now.preemptions_total, before.preemptions_total),
            );
            hist_delta(&mut acc.ttft, now.ttft.as_ref(), before.ttft.as_ref());
            hist_delta(&mut acc.e2e, now.e2e.as_ref(), before.e2e.as_ref());
            hist_delta(&mut acc.tpot, now.tpot.as_ref(), before.tpot.as_ref());
        }
        self.previous = Some(now);
        closed
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn finish(start: u64, acc: Accum) -> Option<ClosedWindow> {
    let requests = acc.requests.unwrap_or(0);
    if acc.output_tokens == 0 && acc.prompt_tokens == 0 && requests == 0 {
        return None;
    }
    let decode_tokens_per_s = if acc.decode_gauges.is_empty() {
        // vLLM: mean time per output token, inverted.
        acc.tpot
            .as_ref()
            .filter(|h| h.total() > 0 && h.sum_ms > 0.0)
            .map(|h| 1000.0 * h.total() as f64 / h.sum_ms)
    } else {
        Some(acc.decode_gauges.iter().sum::<f64>() / acc.decode_gauges.len() as f64)
    };
    let mean = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    let as_u32 = |v: Option<f64>| v.map(|v| v.min(f64::from(u32::MAX)) as u32);
    let nonempty = |h: Option<Histogram>| h.filter(|h| h.total() > 0);
    Some(ClosedWindow {
        window: Window {
            start_unix_s: start,
            duration_s: WINDOW_SECONDS,
        },
        metrics: Rollup {
            // `errors` is meaningful only next to `requests`.
            errors: acc.requests.and(acc.errors).map(|e| e.min(requests)),
            requests: acc.requests,
            prompt_tokens: acc.prompt_tokens,
            output_tokens: acc.output_tokens,
            output_tokens_per_s: acc.output_tokens as f64 / f64::from(WINDOW_SECONDS),
            decode_tokens_per_s: decode_tokens_per_s.filter(|v| v.is_finite()),
            ttft_ms: nonempty(acc.ttft),
            e2e_ms: nonempty(acc.e2e),
            tpot_ms: nonempty(acc.tpot),
            running_peak: as_u32(acc.running_peak),
            waiting_peak: as_u32(acc.waiting_peak),
            kv_cache_utilization_peak: acc.kv_peak.map(|v| v.clamp(0.0, 1.0)),
            preemptions: acc.preemptions,
            gpu_utilization: mean(&acc.gpu_util),
            gpu_memory_peak_bytes: acc.gpu_mem_peak,
            host_memory_peak_bytes: acc.resident_peak.map(|v| v as u64),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{AcceleratorSample, Vendor};

    /// A vLLM `/metrics` body at a given point in time (trimmed to the series
    /// the agent reads, plus noise it must ignore).
    fn vllm(requests: u32, aborted: u32, tokens: u32, ttft_fast: u32, running: u32) -> String {
        format!(
            "# HELP python_gc_objects_collected_total x\n\
             python_gc_objects_collected_total{{generation=\"0\"}} 99\n\
             process_resident_memory_bytes 2.5e9\n\
             vllm:num_requests_running{{engine=\"0\",model_name=\"m\"}} {running}\n\
             vllm:num_requests_waiting{{engine=\"0\",model_name=\"m\"}} 0\n\
             vllm:kv_cache_usage_perc{{engine=\"0\",model_name=\"m\"}} 0.4\n\
             vllm:num_preemptions_total{{engine=\"0\",model_name=\"m\"}} 1\n\
             vllm:prompt_tokens_total{{engine=\"0\",model_name=\"m\"}} {p}\n\
             vllm:generation_tokens_total{{engine=\"0\",model_name=\"m\"}} {tokens}\n\
             vllm:request_success_total{{engine=\"0\",finished_reason=\"stop\",model_name=\"m\"}} {ok}\n\
             vllm:request_success_total{{engine=\"0\",finished_reason=\"abort\",model_name=\"m\"}} {aborted}\n\
             vllm:time_to_first_token_seconds_bucket{{engine=\"0\",le=\"0.1\",model_name=\"m\"}} {ttft_fast}\n\
             vllm:time_to_first_token_seconds_bucket{{engine=\"0\",le=\"1.0\",model_name=\"m\"}} {requests}\n\
             vllm:time_to_first_token_seconds_bucket{{engine=\"0\",le=\"+Inf\",model_name=\"m\"}} {requests}\n\
             vllm:time_to_first_token_seconds_sum{{engine=\"0\",model_name=\"m\"}} {ttft_sum}\n\
             vllm:request_time_per_output_token_seconds_bucket{{engine=\"0\",le=\"0.025\",model_name=\"m\"}} {requests}\n\
             vllm:request_time_per_output_token_seconds_bucket{{engine=\"0\",le=\"+Inf\",model_name=\"m\"}} {requests}\n\
             vllm:request_time_per_output_token_seconds_sum{{engine=\"0\",model_name=\"m\"}} {tpot_sum}\n",
            p = tokens * 2,
            ok = requests - aborted,
            ttft_sum = f64::from(requests) * 0.2,
            tpot_sum = f64::from(requests) * 0.02,
        )
    }

    const LLAMACPP: &str = "llamacpp:prompt_tokens_total 1200\n\
        llamacpp:tokens_predicted_total 3456\n\
        llamacpp:predicted_tokens_seconds 41.25\n\
        llamacpp:requests_processing 1\n\
        llamacpp:requests_deferred 0\n";

    const T0: u64 = 1_750_000_200; // window-aligned

    #[test]
    fn vllm_text_parses_every_series_the_agent_reads() {
        let (rt, s) = Scrape::from_text(&vllm(10, 1, 500, 4, 2)).unwrap();
        assert_eq!(rt, ScrapeRuntime::Vllm);
        assert_eq!(s.requests_total, Some(10.0));
        assert_eq!(s.aborted_total, Some(1.0));
        assert_eq!(s.prompt_tokens_total, Some(1000.0));
        assert_eq!(s.output_tokens_total, Some(500.0));
        assert_eq!(s.running, Some(2.0));
        assert_eq!(s.kv_usage, Some(0.4));
        assert_eq!(s.resident_bytes, Some(2.5e9));
        let ttft = s.ttft.unwrap();
        assert_eq!(ttft.total(), 10);
        assert!((ttft.sum_ms - 2000.0).abs() < 1e-6);
        assert_eq!(s.tpot.unwrap().total(), 10);
        assert!(s.e2e.is_none(), "absent histogram stays None");
    }

    #[test]
    fn llamacpp_text_parses_what_it_exposes() {
        let (rt, s) = Scrape::from_text(LLAMACPP).unwrap();
        assert_eq!(rt, ScrapeRuntime::LlamaCpp);
        assert_eq!(s.requests_total, None);
        assert_eq!(s.output_tokens_total, Some(3456.0));
        assert_eq!(s.decode_tokens_per_s, Some(41.25));
        assert!(s.ttft.is_none());
        assert!(Scrape::from_text("process_cpu_seconds_total 1\n").is_none());
    }

    fn scrape(text: &str) -> Option<Scrape> {
        Scrape::from_text(text).map(|(_, s)| s)
    }

    #[test]
    fn windows_hold_the_deltas_between_scrapes() {
        let mut agg = WindowAggregator::new();
        let hw = HardwareSample {
            accelerators: vec![AcceleratorSample {
                util_pct: Some(50.0),
                mem_used_bytes: Some(7 << 30),
                ..AcceleratorSample::new(0, Vendor::Nvidia)
            }],
            ..HardwareSample::default()
        };
        // Baseline, then two scrapes inside the first window.
        assert!(
            agg.observe(T0, scrape(&vllm(10, 1, 500, 4, 2)), Some(&hw))
                .is_none()
        );
        assert!(
            agg.observe(T0 + 60, scrape(&vllm(30, 2, 1500, 10, 5)), None)
                .is_none()
        );
        assert!(
            agg.observe(T0 + 120, scrape(&vllm(40, 2, 2000, 12, 1)), None)
                .is_none()
        );
        // The first observation of the next window closes it.
        let closed = agg
            .observe(T0 + 300, scrape(&vllm(40, 2, 2000, 12, 0)), None)
            .unwrap();
        assert_eq!(closed.window.start_unix_s, T0);
        assert_eq!(closed.window.duration_s, 300);
        let m = closed.metrics;
        assert_eq!(m.requests, Some(30));
        assert_eq!(m.errors, Some(1));
        assert_eq!(m.output_tokens, 1500);
        assert_eq!(m.prompt_tokens, 3000);
        assert!((m.output_tokens_per_s - 5.0).abs() < 1e-9);
        assert_eq!(m.running_peak, Some(5));
        assert_eq!(m.kv_cache_utilization_peak, Some(0.4));
        assert_eq!(m.gpu_utilization, Some(0.5));
        assert_eq!(m.gpu_memory_peak_bytes, Some(7 << 30));
        assert_eq!(m.host_memory_peak_bytes, Some(2_500_000_000));
        let ttft = m.ttft_ms.unwrap();
        assert_eq!(ttft.total(), 30);
        assert_eq!(ttft.counts[5], 8, "le=0.1 s is the 100 ms bucket");
        // 20 ms per token => 50 tokens/s while decoding.
        assert!((m.decode_tokens_per_s.unwrap() - 50.0).abs() < 1e-6);
        // The next window saw no traffic: dropped, not reported.
        assert!(agg.close_due(T0 + 600).is_none());
    }

    #[test]
    fn a_restart_counts_from_zero_instead_of_going_negative() {
        let mut agg = WindowAggregator::new();
        agg.observe(T0, scrape(&vllm(100, 0, 9000, 50, 0)), None);
        // The server restarted: counters are smaller than before.
        agg.observe(T0 + 60, scrape(&vllm(5, 0, 200, 5, 0)), None);
        let m = agg.close_due(T0 + 300).unwrap().metrics;
        assert_eq!(m.requests, Some(5));
        assert_eq!(m.output_tokens, 200);
        assert_eq!(m.ttft_ms.unwrap().total(), 5);
    }

    #[test]
    fn llamacpp_windows_have_no_request_count_but_have_decode_speed() {
        let mut agg = WindowAggregator::new();
        agg.observe(T0, scrape(LLAMACPP), None);
        let later = LLAMACPP.replace("3456", "4456").replace("41.25", "38.75");
        agg.observe(T0 + 60, scrape(&later), None);
        let m = agg.close_due(T0 + 300).unwrap().metrics;
        assert_eq!(m.requests, None);
        assert_eq!(m.errors, None);
        assert_eq!(m.output_tokens, 1000);
        assert!((m.decode_tokens_per_s.unwrap() - 40.0).abs() < 1e-9);
        assert!(m.ttft_ms.is_none());
    }

    #[test]
    fn failed_scrapes_and_a_lone_baseline_report_nothing() {
        let mut agg = WindowAggregator::new();
        agg.observe(T0, None, None);
        agg.observe(T0 + 60, scrape(LLAMACPP), None);
        assert!(agg.close_due(T0 + 300).is_none(), "baseline only: idle");
    }
}
