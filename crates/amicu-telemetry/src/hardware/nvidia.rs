//! `nvidia-smi --query-gpu=index,name,utilization.gpu,memory.used,
//! memory.total,temperature.gpu,power.draw --format=csv,noheader,nounits`.
//!
//! One CSV line per GPU, fields separated by `", "`. With `nounits`, memory
//! is MiB, power is W, temperature is C. A field the GPU does not support is
//! `[N/A]` / `[Not Supported]` and becomes `None`.

use super::{AcceleratorSample, MIB, Vendor, parse_number, scale_to_bytes};

const FIELDS: usize = 7;

pub(super) fn parse(csv: &str) -> Vec<AcceleratorSample> {
    csv.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<AcceleratorSample> {
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    if parts.len() < FIELDS {
        return None;
    }
    // A GPU name never contains a comma in practice, but if it does, the five
    // numeric fields are anchored from the right and the name is what is left.
    let tail = &parts[parts.len() - 5..];
    let name = parts[1..parts.len() - 5].join(", ");
    let index = parts[0].parse::<u32>().ok()?;
    let mut sample = AcceleratorSample::new(index, Vendor::Nvidia);
    sample.name = (!name.is_empty()).then_some(name);
    sample.util_pct = parse_number(tail[0]);
    sample.mem_used_bytes = parse_number(tail[1]).and_then(|v| scale_to_bytes(v, MIB));
    sample.mem_total_bytes = parse_number(tail[2]).and_then(|v| scale_to_bytes(v, MIB));
    sample.temp_c = parse_number(tail[3]);
    sample.power_w = parse_number(tail[4]);
    Some(sample)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::fixtures as fx;

    #[test]
    fn parses_two_gpus_and_converts_mib_to_bytes() {
        let gpus = parse(fx::NVIDIA_CSV);
        assert_eq!(gpus.len(), 2);
        let g = &gpus[0];
        assert_eq!(g.index, 0);
        assert_eq!(g.vendor, Vendor::Nvidia);
        assert_eq!(g.name.as_deref(), Some("NVIDIA H100 80GB HBM3"));
        assert_eq!(g.util_pct, Some(37.0));
        assert_eq!(g.mem_used_bytes, Some(21412 * MIB));
        assert_eq!(g.mem_total_bytes, Some(81559 * MIB));
        assert_eq!(g.temp_c, Some(45.0));
        assert_eq!(g.power_w, Some(289.45));
    }

    #[test]
    fn unsupported_fields_are_none_not_zero() {
        let g = &parse(fx::NVIDIA_CSV)[1];
        assert_eq!(g.index, 1);
        assert_eq!(g.util_pct, Some(0.0));
        assert_eq!(g.power_w, None);
        assert_eq!(g.temp_c, None);
    }

    #[test]
    fn driver_failure_text_and_short_lines_are_skipped() {
        assert!(parse("NVIDIA-SMI has failed because it couldn't communicate").is_empty());
        assert!(parse("0, GPU, 1, 2").is_empty());
        assert!(parse("x, GPU, 1, 2, 3, 4, 5").is_empty());
    }

    #[test]
    fn a_comma_in_the_name_is_kept() {
        let g = &parse("0, Weird, Name, 5, 100, 200, 30, 50.5")[0];
        assert_eq!(g.name.as_deref(), Some("Weird, Name"));
        assert_eq!(g.util_pct, Some(5.0));
    }
}
