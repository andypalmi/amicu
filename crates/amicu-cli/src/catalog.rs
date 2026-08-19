//! Build a llama.cpp planning catalog from a parsed graph.

use amicu_core::graph::Graph;
use amicu_core::resource::Model;
use amicu_llamacpp::MapCatalog;

/// Build a [`MapCatalog`] over every model and hardware profile in `graph`.
pub(crate) fn build_catalog(graph: &Graph) -> MapCatalog {
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
pub(crate) fn estimate_parameters(model: &Model) -> u64 {
    parse_param_count(&model.source.reference)
        .or_else(|| parse_param_count(model.id.as_str()))
        .unwrap_or(0)
}

/// Find the first `<number>B` / `<number>M` token and scale it to a raw count.
fn parse_param_count(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if i < bytes.len() {
                let scale = match bytes[i] {
                    b'b' | b'B' => Some(1_000_000_000.0),
                    b'm' | b'M' => Some(1_000_000.0),
                    _ => None,
                };
                if let Some(scale) = scale
                    && let Ok(value) = text[start..i].parse::<f64>()
                {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    return Some((value * scale) as u64);
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_billions_and_millions() {
        assert_eq!(
            parse_param_count("Qwen/Qwen2.5-7B-Instruct"),
            Some(7_000_000_000)
        );
        assert_eq!(parse_param_count("org/tiny-350M"), Some(350_000_000));
        assert_eq!(parse_param_count("org/model-1.5B"), Some(1_500_000_000));
        assert_eq!(parse_param_count("no-size-here"), None);
    }
}
