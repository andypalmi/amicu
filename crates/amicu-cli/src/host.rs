//! Detect the machine amicu runs on: system memory, Apple Silicon chip, and
//! NVIDIA GPUs. The pure parsing is unit-tested; only [`HostInfo::detect`]
//! touches the real machine. Everything that consumes a host takes a
//! [`HostInfo`] by reference so tests can inject one.

use std::fmt::Write as _;
use std::process::Command;

use amicu_core::ByteSize;
use amicu_core::id::HardwareProfileId;
use amicu_core::resource::{Accelerator, AcceleratorKind, HardwareProfile};
use amicu_engine::host::HostFacts;
use amicu_runtime::discovery::discover_report;

const MIB: u64 = 1024 * 1024;

/// NVIDIA GPUs found through `nvidia-smi`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NvidiaGpu {
    /// Model slug, e.g. `rtx_4090`.
    pub model: String,
    pub count: u32,
    /// VRAM of one card (the smallest, when they differ).
    pub memory: ByteSize,
}

/// What was detected about this machine.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HostInfo {
    /// `macos` or `linux`.
    pub os: String,
    /// Discovered profile: memory, CPU, and the Apple Silicon accelerator.
    pub profile: HardwareProfile,
    pub nvidia: Option<NvidiaGpu>,
}

impl HostInfo {
    /// Probe the real machine. Fails on an OS discovery does not support.
    pub(crate) fn detect() -> Result<Self, String> {
        let report = discover_report().map_err(|e| e.to_string())?;
        let nvidia = if report.os == "linux" {
            nvidia_smi_output().and_then(|out| parse_nvidia_smi(&out))
        } else {
            None
        };
        Ok(Self {
            os: report.os,
            profile: report.profile,
            nvidia,
        })
    }

    /// The figures the plan-time host check compares a declared profile with.
    pub(crate) fn facts(&self) -> HostFacts {
        HostFacts {
            memory: self.profile.memory,
            accelerator_memory: self
                .nvidia
                .as_ref()
                .map(|g| g.memory)
                .or_else(|| self.profile.accelerator.as_ref().map(|a| a.memory)),
        }
    }

    /// The host as a hardware profile, GPU included, for memory-fit checks.
    pub(crate) fn fit_profile(&self) -> HardwareProfile {
        let mut p = self.profile.clone();
        if let Some(gpu) = &self.nvidia {
            p.id = HardwareProfileId::new("local-gpu");
            p.accelerator = Some(Accelerator {
                kind: AcceleratorKind::Gpu,
                model: gpu.model.clone(),
                count: gpu.count,
                memory: gpu.memory,
                memory_bandwidth_gbps: None,
            });
        }
        p
    }

    /// One-line description for human output.
    pub(crate) fn describe(&self) -> String {
        let mut s = format!("{} · {} memory", self.os, self.profile.memory.human());
        if let Some(a) = &self.profile.accelerator {
            let _ = write!(s, " · {}", a.model);
        }
        if let Some(g) = &self.nvidia {
            let _ = write!(s, " · {}x {} {}", g.count, g.model, g.memory.human());
        }
        s
    }
}

fn nvidia_smi_output() -> Option<String> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits`
/// (one `name, MiB` line per card). `None` when no card is listed.
pub(crate) fn parse_nvidia_smi(text: &str) -> Option<NvidiaGpu> {
    let mut count = 0u32;
    let mut min_mib: Option<u64> = None;
    let mut name = String::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (n, mib) = line.rsplit_once(',')?;
        let mib: u64 = mib.trim().parse().ok()?;
        if count == 0 {
            n.trim().clone_into(&mut name);
        }
        count += 1;
        min_mib = Some(min_mib.map_or(mib, |m| m.min(mib)));
    }
    Some(NvidiaGpu {
        model: slug(&name),
        count,
        memory: ByteSize::from_bytes(min_mib? * MIB),
    })
}

/// `"NVIDIA GeForce RTX 4090"` -> `"rtx_4090"`: lowercase, vendor words
/// dropped, runs of other characters folded to `_`.
pub(crate) fn slug(name: &str) -> String {
    let words: Vec<String> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|w| !matches!(w.as_str(), "nvidia" | "geforce"))
        .collect();
    if words.is_empty() {
        "gpu".to_owned()
    } else {
        words.join("_")
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use amicu_core::resource::Cpu;

    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    pub(crate) fn mac(chip: &str, gib: u64) -> HostInfo {
        HostInfo {
            os: "macos".into(),
            profile: HardwareProfile {
                id: HardwareProfileId::new(format!("local-{chip}")),
                provider: "local".into(),
                region: None,
                accelerator: Some(Accelerator {
                    kind: AcceleratorKind::AppleSilicon,
                    model: chip.into(),
                    count: 1,
                    memory: ByteSize::from_bytes(gib * GIB),
                    memory_bandwidth_gbps: None,
                }),
                cpu: Some(Cpu {
                    model: "Apple".into(),
                    cores: 10,
                }),
                memory: ByteSize::from_bytes(gib * GIB),
                interconnect: None,
                price: None,
            },
            nvidia: None,
        }
    }

    pub(crate) fn linux(gib: u64, gpu: Option<(&str, u64)>) -> HostInfo {
        HostInfo {
            os: "linux".into(),
            profile: HardwareProfile {
                id: HardwareProfileId::new("local-cpu"),
                provider: "local".into(),
                region: None,
                accelerator: None,
                cpu: Some(Cpu {
                    model: "AMD Ryzen 9 7950X".into(),
                    cores: 16,
                }),
                memory: ByteSize::from_bytes(gib * GIB),
                interconnect: None,
                price: None,
            },
            nvidia: gpu.map(|(model, vram)| NvidiaGpu {
                model: model.into(),
                count: 1,
                memory: ByteSize::from_bytes(vram * GIB),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nvidia_smi_csv() {
        let g = parse_nvidia_smi("NVIDIA GeForce RTX 4090, 24564\n").unwrap();
        assert_eq!(g.model, "rtx_4090");
        assert_eq!(g.count, 1);
        assert_eq!(g.memory.bytes(), 24564 * MIB);
    }

    #[test]
    fn multi_gpu_counts_cards_and_takes_the_smallest() {
        let g = parse_nvidia_smi("NVIDIA A100-SXM4-80GB, 81920\nNVIDIA A100-SXM4-80GB, 40960\n")
            .unwrap();
        assert_eq!(g.count, 2);
        assert_eq!(g.model, "a100_sxm4_80gb");
        assert_eq!(g.memory.bytes(), 40960 * MIB);
    }

    #[test]
    fn empty_or_garbled_output_is_no_gpu() {
        assert!(parse_nvidia_smi("").is_none());
        assert!(parse_nvidia_smi("no devices\n").is_none());
    }

    #[test]
    fn facts_prefer_gpu_vram_over_apple_unified() {
        let h = fake::linux(64, Some(("rtx_4090", 24)));
        assert_eq!(h.facts().accelerator_memory.unwrap().bytes(), 24 << 30);
        assert!(fake::linux(64, None).facts().accelerator_memory.is_none());
        assert_eq!(
            fake::mac("m4_pro", 48)
                .facts()
                .accelerator_memory
                .unwrap()
                .bytes(),
            48 << 30
        );
    }
}
