//! Apple GPU: `ioreg -r -d 1 -w 0 -c IOAccelerator`, filtered by the probe to
//! the `+-o <class>` line, `"model"` and `"PerformanceStatistics"`. Needs no
//! sudo.
//!
//! `PerformanceStatistics` is an inline dictionary:
//! `{"Device Utilization %"=6,"In use system memory"=210010112,...}`.
//! The GPU shares unified memory with the CPU, so there is no GPU-only total
//! and `mem_total_bytes` stays `None`. Temperature and power need
//! `powermetrics` (root) and are not reported.

use super::{AcceleratorSample, Vendor, f64_to_u64, parse_number};

pub(super) fn parse(text: &str) -> Vec<AcceleratorSample> {
    let mut out: Vec<AcceleratorSample> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("+-o ") {
            let index = u32::try_from(out.len()).unwrap_or(u32::MAX);
            out.push(AcceleratorSample::new(index, Vendor::Apple));
        } else if let Some(sample) = out.last_mut() {
            if let Some(model) = quoted_value(line, "\"model\" = ") {
                sample.name = Some(model.to_owned());
            } else if line.starts_with("\"PerformanceStatistics\" = ") {
                sample.util_pct = stat(line, "Device Utilization %");
                sample.mem_used_bytes = stat(line, "In use system memory").and_then(f64_to_u64);
            }
        }
    }
    out.retain(|s| s.util_pct.is_some() || s.mem_used_bytes.is_some() || s.name.is_some());
    out
}

/// `key = "value"` -> `value`.
fn quoted_value<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(prefix)?;
    rest.strip_prefix('"')?.strip_suffix('"')
}

/// Value of `"<key>"=<number>` inside the dictionary. The closing quote must
/// be followed directly by `=`, so `"In use system memory (driver)"` does not
/// match `"In use system memory"`.
fn stat(line: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\"=");
    let rest = &line[line.find(&needle)? + needle.len()..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    parse_number(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::fixtures as fx;

    #[test]
    fn parses_m1_performance_statistics() {
        let gpus = parse(fx::IOREG_M1);
        assert_eq!(gpus.len(), 1);
        let g = &gpus[0];
        assert_eq!(g.vendor, Vendor::Apple);
        assert_eq!(g.name.as_deref(), Some("Apple M1"));
        assert_eq!(g.util_pct, Some(6.0));
        // The exact-key match picks "In use system memory", not "(driver)".
        assert_eq!(g.mem_used_bytes, Some(210_010_112));
        assert_eq!(g.mem_total_bytes, None);
        assert_eq!(g.temp_c, None);
        assert_eq!(g.power_w, None);
    }

    #[test]
    fn missing_statistics_stay_none() {
        let g = &parse("+-o AGXAccelerator  <class x>\n \"model\" = \"Apple M9\"")[0];
        assert_eq!(g.name.as_deref(), Some("Apple M9"));
        assert_eq!(g.util_pct, None);
        assert_eq!(g.mem_used_bytes, None);
    }

    #[test]
    fn unrelated_text_yields_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("+-o Foo <class Foo>").is_empty());
    }
}
