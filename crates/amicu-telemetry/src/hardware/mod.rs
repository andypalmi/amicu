//! Vendor-agnostic hardware metrics: accelerator utilisation, memory,
//! temperature and power, plus the memory and CPU of the container (or host)
//! the workload runs in.
//!
//! The design splits *collection* from *parsing*:
//!
//! * Collection is one fixed POSIX `sh` script ([`PROBE_SCRIPT`]), embedded
//!   with `include_str!` and run verbatim - locally with [`probe_local`], or
//!   inside a Modal container by `amicu-modal` through `ContainerExec`. It
//!   prints each tool's raw output in a `@@amicu:<name>` delimited section
//!   and never interpolates user input.
//! * Parsing is pure: [`parse_probe_output`] splits the sections
//!   ([`split_sections`]) and hands each to a per-vendor parser. No vendor is
//!   privileged; each source is parsed independently and the results
//!   concatenated.
//!
//! Every value is optional. `None` means "this source did not report it" and
//! is never replaced by `0`.
//!
//! Sources (tool, then the section name the probe gives it):
//!
//! | vendor | source | section |
//! |---|---|---|
//! | NVIDIA | `nvidia-smi --query-gpu=... --format=csv,noheader,nounits` | `nvidia-smi` |
//! | AMD | `amd-smi metric --json` | `amd-smi` |
//! | AMD | `rocm-smi ... --json` (only when `amd-smi` printed nothing) | `rocm-smi` |
//! | AMD | amdgpu sysfs (only when neither tool reported a GPU) | `drm-cardN` |
//! | Intel | `xpu-smi stats -j` + `xpu-smi discovery [-d N] -j` | `xpu-smi-*` |
//! | Apple | `ioreg -r -d 1 -w 0 -c IOAccelerator` (macOS host only) | `ioreg` |
//! | container | cgroup v2, else cgroup v1, else `/proc/meminfo` | `cgroup*`, `meminfo` |
//!
//! Fixtures and their provenance live in the test-only `fixtures` module.

// Tests assert emptiness of parser output directly; the lint's suggestion
// needs an element type annotation that adds noise.
#![cfg_attr(test, allow(clippy::assert_is_empty))]

mod amd;
mod apple;
mod host;
mod intel;
mod local;
mod nvidia;

#[cfg(test)]
mod fixtures;

use serde::{Deserialize, Serialize};

pub use local::{HardwareError, probe_local};

/// The embedded probe script. Fixed text: run it as `sh -c <script>`.
pub const PROBE_SCRIPT: &str = include_str!("probe.sh");

/// How long a probe may run before the caller gives up on it.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound on the probe output a caller should accept. The script caps
/// each tool at 64 KiB; this is the overall ceiling.
pub const MAX_PROBE_OUTPUT_BYTES: usize = 256 * 1024;

/// The argv that runs [`PROBE_SCRIPT`].
#[must_use]
pub fn probe_command() -> Vec<String> {
    vec!["sh".to_owned(), "-c".to_owned(), PROBE_SCRIPT.to_owned()]
}

/// Who made an accelerator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Apple,
}

impl Vendor {
    /// Lower-case label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Nvidia => "nvidia",
            Self::Amd => "amd",
            Self::Intel => "intel",
            Self::Apple => "apple",
        }
    }
}

/// One accelerator. `None` = not reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcceleratorSample {
    /// Index within its vendor's own numbering.
    pub index: u32,
    pub vendor: Vendor,
    pub name: Option<String>,
    pub util_pct: Option<f64>,
    pub mem_used_bytes: Option<u64>,
    pub mem_total_bytes: Option<u64>,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
}

impl AcceleratorSample {
    pub(crate) fn new(index: u32, vendor: Vendor) -> Self {
        Self {
            index,
            vendor,
            name: None,
            util_pct: None,
            mem_used_bytes: None,
            mem_total_bytes: None,
            temp_c: None,
            power_w: None,
        }
    }
}

/// Where [`HostSample`]'s memory figures came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemorySource {
    #[serde(rename = "cgroup-v2")]
    CgroupV2,
    #[serde(rename = "cgroup-v1")]
    CgroupV1,
    #[serde(rename = "proc-meminfo")]
    ProcMeminfo,
}

impl MemorySource {
    /// Human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::CgroupV2 => "cgroup v2",
            Self::CgroupV1 => "cgroup v1",
            Self::ProcMeminfo => "/proc/meminfo",
        }
    }
}

/// Memory and CPU of the container (or host) the probe ran in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostSample {
    pub mem_used_bytes: Option<u64>,
    /// `None` also when the cgroup has no limit.
    pub mem_limit_bytes: Option<u64>,
    /// Cumulative CPU time, microseconds. A counter, not a rate.
    pub cpu_usage_usec: Option<u64>,
    pub memory_source: Option<MemorySource>,
}

/// Everything one probe run reported.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HardwareSample {
    pub accelerators: Vec<AcceleratorSample>,
    pub host: HostSample,
}

impl HardwareSample {
    /// Sum of the accelerators' used memory, `None` when none reported it.
    /// Feeds `RuntimeMetrics::memory_used`.
    #[must_use]
    pub fn accelerator_memory_used(&self) -> Option<u64> {
        self.accelerators
            .iter()
            .filter_map(|a| a.mem_used_bytes)
            .reduce(u64::saturating_add)
    }
}

/// One `@@amicu:<name>` block of probe output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section<'a> {
    pub name: &'a str,
    pub body: &'a str,
}

const SECTION_PREFIX: &str = "@@amicu:";

/// Splits probe output into sections. Text before the first header is
/// ignored; a header must start a line; bodies are trimmed.
#[must_use]
pub fn split_sections(output: &str) -> Vec<Section<'_>> {
    let mut sections = Vec::new();
    let mut current: Option<(&str, usize)> = None; // (name, body start)
    let mut offset = 0;
    for line in output.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix(SECTION_PREFIX) {
            if let Some((name, start)) = current.take() {
                sections.push(Section {
                    name,
                    body: output[start..offset].trim(),
                });
            }
            current = Some((rest.trim(), offset + line.len()));
        }
        offset += line.len();
    }
    if let Some((name, start)) = current {
        sections.push(Section {
            name,
            body: output[start..].trim(),
        });
    }
    sections
}

/// Parses the full output of [`PROBE_SCRIPT`]. Unknown sections and
/// malformed lines are skipped; nothing here panics on hostile input.
#[must_use]
pub fn parse_probe_output(output: &str) -> HardwareSample {
    let output = truncate_at_char_boundary(output, MAX_PROBE_OUTPUT_BYTES);
    let sections = split_sections(output);
    let body = |name: &str| sections.iter().find(|s| s.name == name).map(|s| s.body);

    let mut accelerators = Vec::new();
    if let Some(csv) = body("nvidia-smi") {
        accelerators.extend(nvidia::parse(csv));
    }
    let mut amd_found = false;
    for (name, parse) in [
        (
            "amd-smi",
            amd::parse_amd_smi as fn(&str) -> Vec<AcceleratorSample>,
        ),
        ("rocm-smi", amd::parse_rocm_smi),
    ] {
        if !amd_found && let Some(json) = body(name) {
            let found = parse(json);
            amd_found = !found.is_empty();
            accelerators.extend(found);
        }
    }
    if !amd_found {
        accelerators.extend(amd::parse_sysfs(&sections));
    }
    accelerators.extend(intel::parse(&sections));
    if let Some(text) = body("ioreg") {
        accelerators.extend(apple::parse(text));
    }

    HardwareSample {
        accelerators,
        host: host::parse(&sections),
    }
}

fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// --- shared parsing helpers ---------------------------------------------

/// Parses a number, treating the usual "not available" spellings as absent.
pub(crate) fn parse_number(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() || t.starts_with('[') || t.eq_ignore_ascii_case("n/a") {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn f64_to_u64(v: f64) -> Option<u64> {
    (v.is_finite() && (0.0..1.8e19).contains(&v)).then_some(v as u64)
}

/// `value * unit_bytes`, rounded down; `None` for negative/non-finite.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn scale_to_bytes(value: f64, unit_bytes: u64) -> Option<u64> {
    f64_to_u64(value * unit_bytes as f64)
}

pub(crate) const MIB: u64 = 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::fixtures as fx;

    #[test]
    fn splitter_separates_sections_and_trims_bodies() {
        let out = "noise before\n@@amicu:probe-v1\n@@amicu:a\nline1\nline2\n\n@@amicu:b\n  x  \n@@amicu:empty\n";
        let s = split_sections(out);
        let names: Vec<_> = s.iter().map(|s| s.name).collect();
        assert_eq!(names, ["probe-v1", "a", "b", "empty"]);
        assert_eq!(s[1].body, "line1\nline2");
        assert_eq!(s[2].body, "x");
        assert_eq!(s[3].body, "");
    }

    #[test]
    fn splitter_only_honours_headers_at_line_start() {
        let out = "@@amicu:a\nvalue @@amicu:fake\n  @@amicu:indented\n";
        let s = split_sections(out);
        assert_eq!(s.len(), 1);
        assert!(s[0].body.contains("@@amicu:fake"));
        assert!(s[0].body.contains("@@amicu:indented"));
    }

    #[test]
    fn splitter_handles_missing_trailing_newline_and_no_sections() {
        assert!(split_sections("").is_empty());
        assert!(split_sections("just text\n").is_empty());
        let s = split_sections("@@amicu:a\nlast");
        assert_eq!(s[0].body, "last");
    }

    #[test]
    fn full_probe_output_combines_vendors_and_host() {
        let out = format!(
            "@@amicu:probe-v1\n@@amicu:nvidia-smi\n{}\n@@amicu:cgroup2-memory-current\n1048576\n\
             @@amicu:cgroup2-memory-max\nmax\n",
            fx::NVIDIA_CSV
        );
        let sample = parse_probe_output(&out);
        assert_eq!(sample.accelerators.len(), 2);
        assert_eq!(sample.accelerators[0].vendor, Vendor::Nvidia);
        assert_eq!(sample.host.mem_used_bytes, Some(1_048_576));
        assert_eq!(sample.host.mem_limit_bytes, None);
        assert_eq!(sample.accelerator_memory_used(), Some((21412 + 4) * MIB),);
    }

    #[test]
    fn rocm_smi_is_ignored_when_amd_smi_reported_and_sysfs_is_the_last_resort() {
        let both = format!(
            "@@amicu:amd-smi\n{}\n@@amicu:rocm-smi\n{}\n@@amicu:drm-card0\n{}\n",
            fx::AMD_SMI_JSON,
            fx::ROCM_SMI_JSON,
            fx::AMD_SYSFS_CARD0
        );
        assert_eq!(parse_probe_output(&both).accelerators.len(), 2);

        let sysfs_only = format!("@@amicu:drm-card0\n{}\n", fx::AMD_SYSFS_CARD0);
        let sample = parse_probe_output(&sysfs_only);
        assert_eq!(sample.accelerators.len(), 1);
        assert_eq!(sample.accelerators[0].vendor, Vendor::Amd);
    }

    #[test]
    fn empty_or_garbage_output_yields_an_empty_sample() {
        assert_eq!(parse_probe_output(""), HardwareSample::default());
        let junk = "@@amicu:nvidia-smi\nNVIDIA-SMI has failed because it couldn't communicate\n\
                    @@amicu:amd-smi\nnot json\n@@amicu:ioreg\n\u{1F4A5}\n";
        let sample = parse_probe_output(junk);
        assert!(sample.accelerators.is_empty());
        assert_eq!(sample.accelerator_memory_used(), None);
    }

    #[test]
    fn oversized_output_is_truncated_safely() {
        let big = "é".repeat(MAX_PROBE_OUTPUT_BYTES);
        let _ = parse_probe_output(&big);
    }

    #[test]
    fn probe_script_is_fixed_posix_sh_with_no_placeholders() {
        assert!(PROBE_SCRIPT.starts_with("#!/bin/sh"));
        let cmd = probe_command();
        assert_eq!(&cmd[..2], ["sh", "-c"]);
        assert_eq!(cmd[2], PROBE_SCRIPT);
    }
}
