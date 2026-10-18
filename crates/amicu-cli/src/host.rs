//! Detect the machine amicu runs on: system memory, Apple Silicon chip, and
//! NVIDIA GPUs. The pure parsing is unit-tested; only [`HostInfo::detect`]
//! touches the real machine. Everything that consumes a host takes a
//! [`HostInfo`] by reference so tests can inject one.

use std::fmt::Write as _;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use amicu_core::ByteSize;
use amicu_core::id::HardwareProfileId;
use amicu_core::resource::{Accelerator, AcceleratorKind, HardwareProfile, resolve_gpu};
use amicu_engine::host::HostFacts;
use amicu_runtime::discovery::discover_report;

const MIB: u64 = 1024 * 1024;

/// NVIDIA GPUs found through `nvidia-smi`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NvidiaGpu {
    /// GPU catalog id (`rtx-4090-24gb`), else a slug of the name (`rtx_4090`).
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
        let nvidia = if matches!(report.os.as_str(), "linux" | "windows") {
            // `nvidia-smi` is found on PATH on both (the Windows driver installs it).
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

/// How long `nvidia-smi` may take before it is given up on: a wedged driver
/// must not hang `plan`, `doctor`, `init` or `catalog`.
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(3);

fn nvidia_smi_output() -> Option<String> {
    run_with_timeout(
        Command::new("nvidia-smi").args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ]),
        NVIDIA_SMI_TIMEOUT,
    )
}

/// Run `cmd` and return its stdout when it exits successfully within
/// `timeout`; kill it and return `None` otherwise.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Read on a thread so a full pipe cannot stall the child or this wait.
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let text = reader.join().ok()?;
    status.success().then_some(text)
}

/// Parse `nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits`
/// (one `name, MiB` line per card). `None` when no card is listed.
pub(crate) fn parse_nvidia_smi(text: &str) -> Option<NvidiaGpu> {
    let mut count = 0u32;
    let mut min_mib: Option<u64> = None;
    let mut name = String::new();
    for line in text.lines() {
        // Skip anything that is not `name, MiB`: blank lines, and rows where a
        // card reports `[N/A]` or `[Not Supported]` for its memory.
        let Some((n, mib)) = line.rsplit_once(',') else {
            continue;
        };
        let Ok(mib) = mib.trim().parse::<u64>() else {
            continue;
        };
        if count == 0 {
            n.trim().clone_into(&mut name);
        }
        count = count.saturating_add(1);
        min_mib = Some(min_mib.map_or(mib, |m| m.min(mib)));
    }
    let memory = ByteSize::from_bytes(min_mib?.saturating_mul(MIB));
    let model = resolve_gpu(&name, Some(memory)).map_or_else(|_| slug(&name), |c| c.id.to_owned());
    Some(NvidiaGpu {
        model,
        count,
        memory,
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
        assert_eq!(g.model, "rtx-4090-24gb");
        assert_eq!(g.count, 1);
        assert_eq!(g.memory.bytes(), 24564 * MIB);
        let g = parse_nvidia_smi("NVIDIA H100 80GB HBM3, 81559\n").unwrap();
        assert_eq!(g.model, "h100-sxm-80gb");
        let g = parse_nvidia_smi("NVIDIA Quadro P400, 2048\n").unwrap();
        assert_eq!(g.model, "quadro_p400");
    }

    #[test]
    fn multi_gpu_counts_cards_and_takes_the_smallest() {
        let g = parse_nvidia_smi("NVIDIA A100-SXM4-80GB, 81920\nNVIDIA A100-SXM4-80GB, 40960\n")
            .unwrap();
        assert_eq!(g.count, 2);
        assert_eq!(g.model, "a100-sxm4-80gb");
        assert_eq!(g.memory.bytes(), 40960 * MIB);
    }

    #[test]
    fn unparsable_rows_are_skipped_not_fatal() {
        let g = parse_nvidia_smi(
            "NVIDIA A, [N/A]
NVIDIA GeForce RTX 4090, 24564\nbroken line\n",
        )
        .unwrap();
        assert_eq!(g.count, 1);
        assert_eq!(g.model, "rtx-4090-24gb");
        // An absurd value saturates instead of overflowing.
        let g = parse_nvidia_smi(&format!("X, {}\n", u64::MAX)).unwrap();
        assert_eq!(g.memory.bytes(), u64::MAX);
    }

    #[test]
    fn a_hung_command_is_killed_at_the_timeout() {
        let start = Instant::now();
        let out = run_with_timeout(Command::new("sleep").arg("30"), Duration::from_millis(200));
        assert!(out.is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
        let ok = run_with_timeout(Command::new("echo").arg("hi"), Duration::from_secs(5));
        assert_eq!(ok.as_deref(), Some("hi\n"));
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
