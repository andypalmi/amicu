//! Intel: `xpu-smi stats -j` for live values, `xpu-smi discovery -j` and
//! `xpu-smi discovery -d N -j` for the name and physical memory size.
//!
//! Shapes (read from `ial/cmn/cmd_stats.cpp` and `cmd_discovery.cpp` in
//! intel/xpumanager): stats prints one object per device (an array when
//! several, a bare object for one), each with `device_index` and per-tile
//! blocks keyed `tile_N`:
//! `utilization.gpu_percent.tile_0` (a number), and
//! `power.gpu_power_w.tile_0`, `temperature.gpu_core_celsius.tile_0`,
//! `memory.used_mib.tile_0`, each `{avg, min, max, current}`.
//! Discovery lists `{"device_list": [{"device_id": 0, "device_name": ...}]}`;
//! the per-device form adds `memory_physical_size_byte` (an integer).
//!
//! Multi-tile devices are folded: utilisation is the tile mean, power and
//! memory are summed, temperature is the hottest tile.

use serde_json::Value;

use super::{AcceleratorSample, MIB, Section, Vendor, f64_to_u64, parse_number, scale_to_bytes};

fn first_json(text: &str) -> Option<Value> {
    let start = text.find(['[', '{'])?;
    serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => parse_number(s),
        _ => None,
    }
}

/// Per-tile values of `block` (`{"tile_0": X, ...}`), where each tile's value
/// is a number or an object whose `current` is the number.
fn tiles(block: Option<&Value>) -> Vec<f64> {
    let Some(Value::Object(map)) = block else {
        return Vec::new();
    };
    map.iter()
        .filter(|(k, _)| k.starts_with("tile_"))
        .filter_map(|(_, v)| match v {
            Value::Object(o) => o.get("current").and_then(num),
            other => num(other),
        })
        .collect()
}

fn mean(v: &[f64]) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

fn sum(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum())
}

fn max(v: &[f64]) -> Option<f64> {
    v.iter().copied().reduce(f64::max)
}

/// Device id -> (name, physical memory bytes) from the discovery sections.
fn discovery(sections: &[Section<'_>]) -> Vec<(u32, Option<String>, Option<u64>)> {
    let mut found: Vec<(u32, Option<String>, Option<u64>)> = Vec::new();
    let mut absorb = |obj: &Value| {
        let Some(id) = obj.get("device_id").and_then(Value::as_u64) else {
            return;
        };
        let Ok(id) = u32::try_from(id) else { return };
        let name = obj
            .get("device_name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mem = obj
            .get("memory_physical_size_byte")
            .and_then(num)
            .and_then(f64_to_u64);
        if let Some(e) = found.iter_mut().find(|e| e.0 == id) {
            e.1 = e.1.take().or(name);
            e.2 = e.2.or(mem);
        } else {
            found.push((id, name, mem));
        }
    };
    for section in sections {
        let list = section.name == "xpu-smi-discovery";
        if !list && !section.name.starts_with("xpu-smi-device-") {
            continue;
        }
        let Some(root) = first_json(section.body) else {
            continue;
        };
        match root.get("device_list") {
            Some(Value::Array(devices)) if list => devices.iter().for_each(&mut absorb),
            _ if !list => absorb(&root),
            _ => {}
        }
    }
    found
}

pub(super) fn parse(sections: &[Section<'_>]) -> Vec<AcceleratorSample> {
    let Some(stats) = sections.iter().find(|s| s.name == "xpu-smi-stats") else {
        return Vec::new();
    };
    let Some(root) = first_json(stats.body) else {
        return Vec::new();
    };
    let devices: &[Value] = match &root {
        Value::Array(a) => a,
        single @ Value::Object(_) => std::slice::from_ref(single),
        _ => &[],
    };
    let known = discovery(sections);
    devices
        .iter()
        .filter_map(|dev| {
            let index = u32::try_from(dev.get("device_index")?.as_u64()?).ok()?;
            let mut s = AcceleratorSample::new(index, Vendor::Intel);
            s.util_pct = mean(&tiles(dev.pointer("/utilization/gpu_percent")));
            s.power_w = sum(&tiles(dev.pointer("/power/gpu_power_w")));
            s.temp_c = max(&tiles(dev.pointer("/temperature/gpu_core_celsius")));
            s.mem_used_bytes = sum(&tiles(dev.pointer("/memory/used_mib")))
                .and_then(|mib| scale_to_bytes(mib, MIB));
            if let Some((_, name, total)) = known.iter().find(|k| k.0 == index) {
                s.name.clone_from(name);
                s.mem_total_bytes = *total;
            }
            Some(s)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{fixtures as fx, split_sections};

    fn probe() -> String {
        format!(
            "@@amicu:xpu-smi-discovery\n{}\n@@amicu:xpu-smi-stats\n{}\n\
             @@amicu:xpu-smi-device-0\n{}\n@@amicu:xpu-smi-device-1\n{}\n",
            fx::XPU_DISCOVERY_JSON,
            fx::XPU_STATS_JSON,
            fx::XPU_DEVICE0_JSON,
            fx::XPU_DEVICE1_JSON,
        )
    }

    #[test]
    fn folds_tiles_and_joins_discovery() {
        let text = probe();
        let gpus = parse(&split_sections(&text));
        assert_eq!(gpus.len(), 2);
        let g = &gpus[0];
        assert_eq!((g.index, g.vendor), (0, Vendor::Intel));
        assert_eq!(g.name.as_deref(), Some("Intel(R) Data Center GPU Max 1550"));
        // tile_0 = 40, tile_1 = 20 -> mean 30.
        assert_eq!(g.util_pct, Some(30.0));
        // 210.5 + 190.25 summed.
        assert_eq!(g.power_w, Some(400.75));
        // hottest tile.
        assert_eq!(g.temp_c, Some(61.0));
        assert_eq!(g.mem_used_bytes, Some((1024 + 2048) * MIB));
        assert_eq!(g.mem_total_bytes, Some(137_438_953_472));
    }

    #[test]
    fn device_without_power_or_memory_reports_none() {
        let text = probe();
        let g = &parse(&split_sections(&text))[1];
        assert_eq!(g.index, 1);
        assert_eq!(g.util_pct, Some(0.0));
        assert_eq!(g.power_w, None);
        assert_eq!(g.mem_used_bytes, None);
        assert_eq!(g.mem_total_bytes, Some(51_539_607_552));
    }

    #[test]
    fn a_single_device_prints_a_bare_object_and_discovery_is_optional() {
        let one = format!("@@amicu:xpu-smi-stats\n{}\n", fx::XPU_STATS_SINGLE_JSON);
        let gpus = parse(&split_sections(&one));
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].util_pct, Some(7.5));
        assert_eq!(gpus[0].name, None);
        assert_eq!(gpus[0].mem_total_bytes, None);
    }

    #[test]
    fn no_stats_section_or_garbage_is_empty() {
        assert!(parse(&split_sections("@@amicu:xpu-smi-discovery\n{}\n")).is_empty());
        assert!(
            parse(&split_sections(
                "@@amicu:xpu-smi-stats\nError: no devices\n"
            ))
            .is_empty()
        );
    }
}
