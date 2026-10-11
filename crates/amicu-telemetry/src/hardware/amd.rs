//! AMD: `amd-smi metric --json` (preferred), `rocm-smi ... --json` (fallback)
//! and the amdgpu sysfs files (last resort, no userspace tool needed).
//!
//! **amd-smi** (ROCm 6.x prints a top-level array, ROCm 7.x wraps it as
//! `{"gpu_data": [...]}`; both are read). Each entry is
//! `{"gpu": N, "usage": {"gfx_activity": {"value": 0, "unit": "%"}},
//! "power": {"socket_power": {"value": 35, "unit": "W"}},
//! "temperature": {"edge": {...}, "hotspot": {...}, "mem": {...}},
//! "mem_usage": {"total_vram": {"value": 262128, "unit": "MB"},
//! "used_vram": {...}}}`. A reading the GPU cannot give is the string
//! `"N/A"`. `MB` is MiB (the CLI divides bytes by `1024*1024`).
//!
//! **rocm-smi** prints `{"card0": {"GPU use (%)": "0",
//! "VRAM Total Memory (B)": "...", "VRAM Total Used Memory (B)": "...",
//! "Temperature (Sensor edge) (C)": "37.0",
//! "Average Graphics Package Power (W)": "7.0"}, "system": {...}}`; every
//! value is a string.
//!
//! **sysfs** (`drm-cardN` sections written by the probe as `key=value`):
//! `gpu_busy_percent`, `mem_info_vram_used`/`_total` (bytes),
//! `temp1_input` (milli-C), `power1_average`/`power1_input` (micro-W).

use serde_json::Value;

use super::{AcceleratorSample, MIB, Section, Vendor, f64_to_u64, parse_number, scale_to_bytes};

/// The first JSON value in `text`, tolerating leading warning lines and
/// trailing garbage.
fn first_json(text: &str) -> Option<Value> {
    let start = text.find(['[', '{'])?;
    serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
}

/// A reading that is either a bare number/numeric string or
/// `{"value": .., "unit": ".."}`. `"N/A"` is `None`.
fn reading(v: Option<&Value>) -> Option<(f64, Option<&str>)> {
    match v? {
        Value::Object(map) => {
            let unit = map.get("unit").and_then(Value::as_str);
            reading(map.get("value")).map(|(n, _)| (n, unit))
        }
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()).map(|f| (f, None)),
        Value::String(s) => parse_number(s).map(|f| (f, None)),
        _ => None,
    }
}

fn unit_bytes(unit: Option<&str>) -> Option<u64> {
    match unit.map(str::to_ascii_uppercase).as_deref() {
        None | Some("MB" | "MIB") => Some(MIB),
        Some("B") => Some(1),
        Some("KB" | "KIB") => Some(1024),
        Some("GB" | "GIB") => Some(MIB * 1024),
        Some(_) => None,
    }
}

fn bytes(v: Option<&Value>) -> Option<u64> {
    let (n, unit) = reading(v)?;
    scale_to_bytes(n, unit_bytes(unit)?)
}

pub(super) fn parse_amd_smi(text: &str) -> Vec<AcceleratorSample> {
    let Some(root) = first_json(text) else {
        return Vec::new();
    };
    let list = match &root {
        Value::Array(a) => a.as_slice(),
        Value::Object(o) => match o.get("gpu_data") {
            Some(Value::Array(a)) => a.as_slice(),
            _ => std::slice::from_ref(&root),
        },
        _ => &[],
    };
    list.iter()
        .enumerate()
        .filter_map(|(pos, entry)| {
            let entry = entry.as_object()?;
            let index = entry
                .get("gpu")
                .and_then(Value::as_u64)
                .or(u64::try_from(pos).ok())
                .and_then(|i| u32::try_from(i).ok())?;
            let mut s = AcceleratorSample::new(index, Vendor::Amd);
            s.util_pct =
                reading(entry.get("usage").and_then(|u| u.get("gfx_activity"))).map(|(n, _)| n);
            let mem = entry.get("mem_usage");
            s.mem_used_bytes = bytes(mem.and_then(|m| m.get("used_vram")));
            s.mem_total_bytes = bytes(mem.and_then(|m| m.get("total_vram")));
            let temps = entry.get("temperature");
            s.temp_c = ["edge", "hotspot"]
                .iter()
                .find_map(|k| reading(temps.and_then(|t| t.get(*k))))
                .map(|(n, _)| n);
            s.power_w =
                reading(entry.get("power").and_then(|p| p.get("socket_power"))).map(|(n, _)| n);
            // Skip entries that reported nothing at all (e.g. a CPU block).
            (s.util_pct.is_some()
                || s.mem_used_bytes.is_some()
                || s.mem_total_bytes.is_some()
                || s.temp_c.is_some()
                || s.power_w.is_some())
            .then_some(s)
        })
        .collect()
}

pub(super) fn parse_rocm_smi(text: &str) -> Vec<AcceleratorSample> {
    let Some(Value::Object(root)) = first_json(text) else {
        return Vec::new();
    };
    let mut out: Vec<AcceleratorSample> = root
        .iter()
        .filter_map(|(key, card)| {
            let index = key.strip_prefix("card")?.parse::<u32>().ok()?;
            let card = card.as_object()?;
            let get = |k: &str| reading(card.get(k)).map(|(n, _)| n);
            let mut s = AcceleratorSample::new(index, Vendor::Amd);
            s.util_pct = get("GPU use (%)");
            s.mem_used_bytes = get("VRAM Total Used Memory (B)").and_then(f64_to_u64);
            s.mem_total_bytes = get("VRAM Total Memory (B)").and_then(f64_to_u64);
            s.temp_c = get("Temperature (Sensor edge) (C)")
                .or_else(|| get("Temperature (Sensor junction) (C)"));
            // "Average Graphics Package Power (W)" or "Current Socket
            // Graphics Package Power (W)", depending on the ASIC.
            s.power_w = card
                .iter()
                .find(|(k, _)| k.ends_with("Graphics Package Power (W)"))
                .and_then(|(_, v)| reading(Some(v)))
                .map(|(n, _)| n);
            Some(s)
        })
        .collect();
    out.sort_by_key(|s| s.index);
    out
}

pub(super) fn parse_sysfs(sections: &[Section<'_>]) -> Vec<AcceleratorSample> {
    let mut out: Vec<AcceleratorSample> = sections
        .iter()
        .filter_map(|section| {
            let index = section.name.strip_prefix("drm-card")?.parse::<u32>().ok()?;
            let value = |key: &str| {
                section.body.lines().find_map(|l| {
                    let (k, v) = l.split_once('=')?;
                    (k == key).then(|| parse_number(v)).flatten()
                })
            };
            let mut s = AcceleratorSample::new(index, Vendor::Amd);
            s.util_pct = value("gpu_busy_percent");
            s.mem_used_bytes = value("mem_info_vram_used").and_then(f64_to_u64);
            s.mem_total_bytes = value("mem_info_vram_total").and_then(f64_to_u64);
            s.temp_c = value("temp1_input").map(|milli| milli / 1000.0);
            s.power_w = value("power1_average")
                .or_else(|| value("power1_input"))
                .map(|micro| micro / 1_000_000.0);
            Some(s)
        })
        .collect();
    out.sort_by_key(|s| s.index);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{fixtures as fx, split_sections};

    #[test]
    fn amd_smi_array_form() {
        let gpus = parse_amd_smi(fx::AMD_SMI_JSON);
        assert_eq!(gpus.len(), 2);
        let g = &gpus[0];
        assert_eq!(g.index, 0);
        assert_eq!(g.vendor, Vendor::Amd);
        assert_eq!(g.util_pct, Some(41.0));
        assert_eq!(g.mem_used_bytes, Some(283 * MIB));
        assert_eq!(g.mem_total_bytes, Some(262_128 * MIB));
        assert_eq!(g.temp_c, Some(38.0));
        assert_eq!(g.power_w, Some(142.0));
    }

    #[test]
    fn amd_smi_na_values_are_none() {
        let g = &parse_amd_smi(fx::AMD_SMI_JSON)[1];
        assert_eq!(g.index, 1);
        assert_eq!(g.util_pct, Some(0.0));
        assert_eq!(g.power_w, None);
        // edge is N/A, so the hotspot reading is used.
        assert_eq!(g.temp_c, Some(51.0));
        assert_eq!(g.mem_used_bytes, None);
    }

    #[test]
    fn amd_smi_gpu_data_wrapper_and_leading_warnings() {
        let wrapped = format!("WARNING: something\n{{\"gpu_data\": {}}}", fx::AMD_SMI_JSON);
        assert_eq!(parse_amd_smi(&wrapped).len(), 2);
    }

    #[test]
    fn amd_smi_garbage_is_empty() {
        assert!(parse_amd_smi("").is_empty());
        assert!(parse_amd_smi("no gpu found").is_empty());
        assert!(parse_amd_smi("[1, 2]").is_empty());
        assert!(parse_amd_smi("{\"gpu_data\": []}").is_empty());
    }

    #[test]
    fn rocm_smi_string_valued_json() {
        let gpus = parse_rocm_smi(fx::ROCM_SMI_JSON);
        assert_eq!(gpus.len(), 2);
        let g = &gpus[0];
        assert_eq!(g.index, 0);
        assert_eq!(g.util_pct, Some(12.0));
        assert_eq!(g.mem_total_bytes, Some(17_163_091_968));
        assert_eq!(g.mem_used_bytes, Some(13_840_384));
        assert_eq!(g.temp_c, Some(37.0));
        assert_eq!(g.power_w, Some(7.0));
        // Second card uses the junction sensor and the socket-power key.
        assert_eq!(gpus[1].temp_c, Some(55.0));
        assert_eq!(gpus[1].power_w, Some(95.5));
    }

    #[test]
    fn sysfs_units_are_converted() {
        let text = format!("@@amicu:drm-card0\n{}\n", fx::AMD_SYSFS_CARD0);
        let sections = split_sections(&text);
        let gpus = parse_sysfs(&sections);
        assert_eq!(gpus.len(), 1);
        let g = &gpus[0];
        assert_eq!(g.util_pct, Some(23.0));
        assert_eq!(g.mem_used_bytes, Some(1_073_741_824));
        assert_eq!(g.mem_total_bytes, Some(17_163_091_968));
        assert_eq!(g.temp_c, Some(44.0));
        assert_eq!(g.power_w, Some(31.5));
    }
}
