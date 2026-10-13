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
            if let Some(count) = scaled(bytes, value, i) {
                return Some(count);
            }
            // `<n>x<size>`: multiply an expert count by the per-expert size.
            if matches!(bytes.get(i), Some(b'x' | b'X'))
                && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
            {
                let (size, end) = scan_number(text, i + 1);
                if let Some(count) = scaled(bytes, size, end) {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    return Some(count * value as u64);
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Parse the digits (and dots) starting at `start`; return the value and end index.
fn scan_number(text: &str, start: usize) -> (f64, usize) {
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
        end += 1;
    }
    (text[start..end].parse().unwrap_or(0.0), end)
}

/// Scale `value` by the `B`/`M` suffix at `bytes[at]`, if there is one.
fn scaled(bytes: &[u8], value: f64, at: usize) -> Option<u64> {
    let scale = match bytes.get(at)? {
        b'b' | b'B' => 1_000_000_000.0,
        b'm' | b'M' => 1_000_000.0,
        _ => return None,
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((value * scale) as u64)
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
        ];
        for (name, expected) in cases {
            assert_eq!(parse_param_count(name), *expected, "{name}");
        }
    }
}
