//! Build a llama.cpp planning catalog from a parsed graph.

use amicu_core::graph::Graph;
use amicu_core::resource::Model;
use amicu_llamacpp::MapCatalog;

/// Build a [`MapCatalog`] over every model and hardware profile in `graph`.
#[must_use]
pub fn build_catalog(graph: &Graph) -> MapCatalog {
    let mut catalog = MapCatalog::new();
    for model in &graph.models {
        catalog = catalog.with_model(model.clone(), estimate_parameters(model));
    }
    for hardware in &graph.hardware_profiles {
        catalog = catalog.with_hardware(hardware.clone());
    }
    catalog
}

/// Estimate a model's parameter count from its reference or id (e.g. `7B`).
///
/// Used only as a fallback for weight-fit when a variant declares no explicit
/// `size`; a declared size always takes precedence downstream.
#[must_use]
pub fn estimate_parameters(model: &Model) -> u64 {
    parse_param_count(&model.source.reference)
        .or_else(|| parse_param_count(model.id.as_str()))
        .unwrap_or(0)
}

/// Find the first `<number>B` / `<number>M` token and scale it to a raw count.
///
/// Mixture-of-Experts names are handled so the result is the weights-resident
/// total, never the active count:
/// - `<n>x<size>` (e.g. `Mixtral-8x7B`) yields `n * size`, a conservative upper
///   bound (the real total is lower because attention is shared), which is the
///   safe direction for a fit check.
/// - A fractional or zero expert count is not an `NxM` match.
/// - `<total>B-A<active>B` (e.g. `Qwen3-30B-A3B`) yields the first token, the
///   total; the trailing active count is never reached.
///
/// Version numbers such as the `3.1` in `Llama-3.1-8B` carry no `B`/`M` suffix
/// and are skipped.
fn parse_param_count(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let (value, next) = scan_number(text, i);
            i = next;
            if let Some(count) = value.and_then(|v| scaled(bytes, v, i)) {
                return Some(count);
            }
            // `<n>x<size>`: multiply a whole, non-zero expert count by the per-expert size.
            if let Some(experts) = value.filter(|v| v.fract() == 0.0 && *v >= 1.0)
                && matches!(bytes.get(i), Some(b'x' | b'X'))
                && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
            {
                let (size, end) = scan_number(text, i + 1);
                if let Some(count) = size.and_then(|v| scaled(bytes, v, end)) {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    return Some(count.saturating_mul(experts as u64));
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Scan the digits (and dots) starting at `start`; return the parsed value
/// (`None` if malformed, e.g. `1.2.3`) and the end index.
fn scan_number(text: &str, start: usize) -> (Option<f64>, usize) {
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
        end += 1;
    }
    (text[start..end].parse().ok(), end)
}

/// Scale `value` by the `B`/`M` suffix at `bytes[at]`; `None` without a suffix or for zero.
fn scaled(bytes: &[u8], value: f64, at: usize) -> Option<u64> {
    let scale = match bytes.get(at)? {
        b'b' | b'B' => 1_000_000_000.0,
        b'm' | b'M' => 1_000_000.0,
        _ => return None,
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let count = (value * scale) as u64;
    (count > 0).then_some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_param_counts() {
        const B: u64 = 1_000_000_000;
        let cases: &[(&str, Option<u64>)] = &[
            ("Qwen/Qwen2.5-7B-Instruct", Some(7 * B)),
            ("org/tiny-350M", Some(350_000_000)),
            ("org/model-1.5B", Some(3 * B / 2)),
            ("no-size-here", None),
            // MoE: NxMB is an upper bound on the total.
            ("mistralai/Mixtral-8x7B-Instruct-v0.1", Some(56 * B)),
            ("mistralai/Mixtral-8x22B-v0.1", Some(176 * B)),
            // MoE: total-A-active uses the total.
            ("Qwen/Qwen3-30B-A3B", Some(30 * B)),
            ("Qwen/Qwen3-235B-A22B", Some(235 * B)),
            ("Qwen/Qwen3-Next-80B-A3B-Instruct", Some(80 * B)),
            // Version numbers are not sizes.
            ("Qwen/Qwen3.5-9B", Some(9 * B)),
            ("Qwen/Qwen2.5-7B", Some(7 * B)),
            ("meta-llama/Llama-3.1-8B", Some(8 * B)),
            ("meta-llama/Llama-3.1-70B", Some(70 * B)),
            ("google/gemma-2-27b", Some(27 * B)),
            ("microsoft/Phi-3.5-MoE-instruct", None),
            ("mistralai/mixtral-8x7b", Some(56 * B)),
            // Malformed or zero sizes are no match, so the caller can fall through.
            ("org/m-1.2.3B", None),
            ("org/v1..B", None),
            // A fractional or zero expert count is not an `NxM` match; `7B` is found instead.
            ("org/m-0x7B", Some(7 * B)),
            ("org/m-1.5x7B", Some(7 * B)),
            // Huge values saturate instead of overflowing.
            ("99999999999x99999999999B", Some(u64::MAX)),
        ];
        for (name, expected) in cases {
            assert_eq!(parse_param_count(name), *expected, "{name}");
        }
    }
}
