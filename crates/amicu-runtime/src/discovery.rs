//! Local hardware discovery for on-host runtimes (Apple Silicon, CPU-only).
//!
//! Raw data acquisition is abstracted behind [`SystemProbe`] so the parsing
//! logic stays pure and is unit-tested against captured fixtures. [`discover`]
//! runs the real [`HostProbe`]; tests inject a fake probe.
//!
//! Windows is probed through PowerShell CIM queries emitting JSON.
//!
//! macOS unified memory is reported by `system_profiler` in binary "GB"
//! (`hw.memsize` confirms an "8 GB" M1 is exactly `8 * 1024^3` bytes), so those
//! values are interpreted as GiB.

use std::process::Command;

use amicu_core::ByteSize;
use amicu_core::id::HardwareProfileId;
use amicu_core::resource::{Accelerator, AcceleratorKind, Cpu, HardwareProfile};

const GIB: u64 = 1024 * 1024 * 1024;

/// Failures raised while discovering local hardware.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// A probe command or file read failed.
    #[error("probe '{source_name}' failed: {message}")]
    Probe {
        source_name: String,
        message: String,
    },

    /// A required field was absent from probe output.
    #[error("field '{field}' not found in {source_name} output")]
    FieldNotFound { field: String, source_name: String },

    /// A field was present but its value could not be parsed.
    #[error("could not parse '{field}' value {value:?} from {source_name}")]
    ParseValue {
        field: String,
        value: String,
        source_name: String,
    },

    /// The host OS has no discovery path.
    #[error("unsupported OS: {0}")]
    UnsupportedOs(String),
}

/// Abstracts raw system data acquisition so discovery parsing stays pure.
pub trait SystemProbe {
    /// Run `command` with `args`, returning its stdout.
    fn run(&self, command: &str, args: &[&str]) -> Result<String, DiscoveryError>;
    /// Read a file (e.g. `/proc/cpuinfo`) to a string.
    fn read_file(&self, path: &str) -> Result<String, DiscoveryError>;
}

/// The real probe: shells out and reads files on the host.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostProbe;

impl SystemProbe for HostProbe {
    fn run(&self, command: &str, args: &[&str]) -> Result<String, DiscoveryError> {
        let output =
            Command::new(command)
                .args(args)
                .output()
                .map_err(|e| DiscoveryError::Probe {
                    source_name: command.to_string(),
                    message: e.to_string(),
                })?;
        if !output.status.success() {
            return Err(DiscoveryError::Probe {
                source_name: command.to_string(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn read_file(&self, path: &str) -> Result<String, DiscoveryError> {
        std::fs::read_to_string(path).map_err(|e| DiscoveryError::Probe {
            source_name: path.to_string(),
            message: e.to_string(),
        })
    }
}

/// A discovered host: the [`HardwareProfile`] plus environment facts that do
/// not live on the profile (OS name and free disk).
#[derive(Debug, Clone, PartialEq)]
pub struct HostReport {
    pub profile: HardwareProfile,
    pub os: String,
    pub disk_free: ByteSize,
}

/// Discover the local [`HardwareProfile`] using the real host probe.
pub fn discover() -> Result<HardwareProfile, DiscoveryError> {
    discover_report().map(|r| r.profile)
}

/// Discover a full [`HostReport`] (profile + OS + free disk) using the host
/// probe. On macOS, Apple-Silicon detection is tried first with a CPU-only
/// fallback; Linux and Windows use the CPU-only path.
pub fn discover_report() -> Result<HostReport, DiscoveryError> {
    discover_with(&HostProbe, std::env::consts::OS)
}

fn discover_with(probe: &impl SystemProbe, os: &str) -> Result<HostReport, DiscoveryError> {
    match os {
        "macos" => discover_macos(probe, os),
        "linux" => discover_linux(probe, os),
        "windows" => discover_windows(probe, os),
        other => Err(DiscoveryError::UnsupportedOs(other.to_string())),
    }
}

fn discover_macos(probe: &impl SystemProbe, os: &str) -> Result<HostReport, DiscoveryError> {
    let disk_free = disk_free(probe)?;
    let hardware = probe.run("system_profiler", &["SPHardwareDataType"])?;
    // Only a missing "Chip" field (e.g. Intel Mac) falls back to sysctl CPU
    // discovery; any other parse error on a real Apple Silicon host propagates.
    let profile = match parse_apple_silicon(&hardware) {
        Ok(profile) => profile,
        Err(DiscoveryError::FieldNotFound { ref field, .. }) if field == "Chip" => {
            let sysctl = probe.run(
                "sysctl",
                &["machdep.cpu.brand_string", "hw.memsize", "hw.ncpu"],
            )?;
            parse_macos_cpu(&sysctl)?
        }
        Err(e) => return Err(e),
    };
    Ok(HostReport {
        profile,
        os: os.to_string(),
        disk_free,
    })
}

fn discover_linux(probe: &impl SystemProbe, os: &str) -> Result<HostReport, DiscoveryError> {
    let cpuinfo = probe.read_file("/proc/cpuinfo")?;
    let meminfo = probe.read_file("/proc/meminfo")?;
    let mut profile = parse_linux_cpu(&cpuinfo, &meminfo)?;
    // Inside a container the cgroup limit, not the host RAM, is what the
    // process can use.
    if let Some(limit) = cgroup_memory_limit(probe) {
        profile.memory = ByteSize::from_bytes(profile.memory.bytes().min(limit));
    }
    let disk_free = disk_free(probe)?;
    Ok(HostReport {
        profile,
        os: os.to_string(),
        disk_free,
    })
}

fn discover_windows(probe: &impl SystemProbe, os: &str) -> Result<HostReport, DiscoveryError> {
    let system = powershell_json(
        probe,
        "Get-CimInstance Win32_ComputerSystem | Select-Object TotalPhysicalMemory",
        "Win32_ComputerSystem",
    )?;
    let processors = powershell_json(
        probe,
        "Get-CimInstance Win32_Processor | Select-Object Name,NumberOfLogicalProcessors",
        "Win32_Processor",
    )?;
    let disk = powershell_json(
        probe,
        "Get-CimInstance Win32_LogicalDisk -Filter \"DeviceID='$env:SystemDrive'\" | Select-Object FreeSpace",
        "Win32_LogicalDisk",
    )?;
    let profile = parse_windows_cpu(&system, &processors)?;
    let disk_free = parse_windows_disk_free(&disk)?;
    Ok(HostReport {
        profile,
        os: os.to_string(),
        disk_free,
    })
}

/// Run a PowerShell CIM query whose output is piped through `ConvertTo-Json`
/// and parse it. `source_name` names the CIM class in errors.
fn powershell_json(
    probe: &impl SystemProbe,
    query: &str,
    source_name: &str,
) -> Result<serde_json::Value, DiscoveryError> {
    let command = format!("{query} | ConvertTo-Json");
    let out = probe.run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &command],
    )?;
    // Windows PowerShell may prefix redirected output with a byte-order mark.
    let text = out.trim().trim_start_matches('\u{feff}');
    serde_json::from_str(text).map_err(|_| DiscoveryError::ParseValue {
        field: "json".to_string(),
        value: text.chars().take(80).collect(),
        source_name: source_name.to_string(),
    })
}

/// `ConvertTo-Json` yields a bare object for one CIM instance and an array for
/// several (e.g. a multi-socket machine).
fn json_objects(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => items.iter().collect(),
        other => vec![other],
    }
}

/// A JSON field that is a number, or a numeric string, as an unsigned integer.
fn json_u64(obj: &serde_json::Value, key: &str, source_name: &str) -> Result<u64, DiscoveryError> {
    match obj.get(key) {
        None | Some(serde_json::Value::Null) => Err(DiscoveryError::FieldNotFound {
            field: key.to_string(),
            source_name: source_name.to_string(),
        }),
        Some(serde_json::Value::Number(n)) => {
            n.as_u64().ok_or_else(|| DiscoveryError::ParseValue {
                field: key.to_string(),
                value: n.to_string(),
                source_name: source_name.to_string(),
            })
        }
        Some(serde_json::Value::String(s)) => parse_u64(s, key, source_name),
        Some(other) => Err(DiscoveryError::ParseValue {
            field: key.to_string(),
            value: other.to_string(),
            source_name: source_name.to_string(),
        }),
    }
}

fn parse_windows_cpu(
    system: &serde_json::Value,
    processors: &serde_json::Value,
) -> Result<HardwareProfile, DiscoveryError> {
    const SYSTEM: &str = "Win32_ComputerSystem";
    const CPU: &str = "Win32_Processor";
    let memory = json_u64(
        json_objects(system).first().unwrap_or(&system),
        "TotalPhysicalMemory",
        SYSTEM,
    )?;
    let sockets = json_objects(processors);
    let model = sockets
        .iter()
        .find_map(|p| p.get("Name")?.as_str())
        .map(|n| n.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|n| !n.is_empty())
        .ok_or_else(|| DiscoveryError::FieldNotFound {
            field: "Name".to_string(),
            source_name: CPU.to_string(),
        })?;
    let mut cores = 0u32;
    for socket in &sockets {
        let n = json_u64(socket, "NumberOfLogicalProcessors", CPU)?;
        cores = cores.saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
    }
    if cores == 0 {
        return Err(DiscoveryError::FieldNotFound {
            field: "NumberOfLogicalProcessors".to_string(),
            source_name: CPU.to_string(),
        });
    }
    Ok(HardwareProfile {
        id: HardwareProfileId::new("local-cpu"),
        provider: "local".to_string(),
        region: None,
        accelerator: None,
        cpu: Some(Cpu { model, cores }),
        memory: ByteSize::from_bytes(memory),
        interconnect: None,
        price: None,
    })
}

fn parse_windows_disk_free(disk: &serde_json::Value) -> Result<ByteSize, DiscoveryError> {
    let first = json_objects(disk)
        .into_iter()
        .next()
        .unwrap_or(&serde_json::Value::Null);
    json_u64(first, "FreeSpace", "Win32_LogicalDisk").map(ByteSize::from_bytes)
}

/// The cgroup memory limit in bytes (v2 `memory.max`, then v1
/// `memory.limit_in_bytes`), or `None` when there is no readable, numeric
/// limit (`max` means unlimited).
fn cgroup_memory_limit(probe: &impl SystemProbe) -> Option<u64> {
    [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .find_map(|path| probe.read_file(path).ok()?.trim().parse::<u64>().ok())
    .filter(|b| *b > 0)
}

fn disk_free(probe: &impl SystemProbe) -> Result<ByteSize, DiscoveryError> {
    let df = probe.run("df", &["-Pk", "/"])?;
    parse_df_available(&df)
}

fn parse_apple_silicon(text: &str) -> Result<HardwareProfile, DiscoveryError> {
    let chip = field(text, "Chip", "system_profiler")?;
    let memory = parse_apple_memory(&field(text, "Memory", "system_profiler")?)?;
    let cores = parse_leading_u32(
        &field(text, "Total Number of Cores", "system_profiler")?,
        "Total Number of Cores",
        "system_profiler",
    )?;
    let model = normalize_chip(&chip);
    Ok(HardwareProfile {
        id: HardwareProfileId::new(format!("local-{model}")),
        provider: "local".to_string(),
        region: None,
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::AppleSilicon,
            model,
            count: 1,
            memory,
            memory_bandwidth_gbps: None,
        }),
        cpu: Some(Cpu { model: chip, cores }),
        memory,
        interconnect: None,
        price: None,
    })
}

fn parse_macos_cpu(text: &str) -> Result<HardwareProfile, DiscoveryError> {
    let model = field(text, "machdep.cpu.brand_string", "sysctl")?;
    let memsize = parse_u64(
        &field(text, "hw.memsize", "sysctl")?,
        "hw.memsize",
        "sysctl",
    )?;
    let cores = parse_u32(&field(text, "hw.ncpu", "sysctl")?, "hw.ncpu", "sysctl")?;
    let memory = ByteSize::from_bytes(memsize);
    Ok(HardwareProfile {
        id: HardwareProfileId::new("local-cpu"),
        provider: "local".to_string(),
        region: None,
        accelerator: None,
        cpu: Some(Cpu { model, cores }),
        memory,
        interconnect: None,
        price: None,
    })
}

fn parse_linux_cpu(cpuinfo: &str, meminfo: &str) -> Result<HardwareProfile, DiscoveryError> {
    let model = field(cpuinfo, "model name", "/proc/cpuinfo")?;
    let cores = cpuinfo
        .lines()
        .filter(|l| {
            l.split_once(':')
                .is_some_and(|(k, _)| k.trim() == "processor")
        })
        .count();
    let cores = u32::try_from(cores).unwrap_or(u32::MAX);
    if cores == 0 {
        return Err(DiscoveryError::FieldNotFound {
            field: "processor".to_string(),
            source_name: "/proc/cpuinfo".to_string(),
        });
    }
    let mem_kb = parse_meminfo_total(meminfo)?;
    let memory = ByteSize::from_bytes(mem_kb.saturating_mul(1024));
    Ok(HardwareProfile {
        id: HardwareProfileId::new("local-cpu"),
        provider: "local".to_string(),
        region: None,
        accelerator: None,
        cpu: Some(Cpu { model, cores }),
        memory,
        interconnect: None,
        price: None,
    })
}

/// Extract the value of a `Key: value` line, matching `key` against the trimmed
/// text before the first colon. Serves `system_profiler`, `sysctl` and
/// `/proc/cpuinfo` alike.
fn field(text: &str, key: &str, source_name: &str) -> Result<String, DiscoveryError> {
    text.lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
        .ok_or_else(|| DiscoveryError::FieldNotFound {
            field: key.to_string(),
            source_name: source_name.to_string(),
        })
}

/// `"Apple M3 Max"` -> `"m3_max"`.
fn normalize_chip(chip: &str) -> String {
    chip.strip_prefix("Apple ")
        .unwrap_or(chip)
        .to_lowercase()
        .replace(' ', "_")
}

/// Apple reports unified memory in binary "GB"; interpret as GiB.
fn parse_apple_memory(s: &str) -> Result<ByteSize, DiscoveryError> {
    let gib = parse_leading_u64(s, "Memory", "system_profiler")?;
    Ok(ByteSize::from_bytes(gib.saturating_mul(GIB)))
}

fn parse_meminfo_total(text: &str) -> Result<u64, DiscoveryError> {
    let line = text
        .lines()
        .find(|l| l.starts_with("MemTotal:"))
        .ok_or_else(|| DiscoveryError::FieldNotFound {
            field: "MemTotal".to_string(),
            source_name: "/proc/meminfo".to_string(),
        })?;
    let value = line.split_whitespace().nth(1).unwrap_or_default();
    parse_u64(value, "MemTotal", "/proc/meminfo")
}

fn parse_df_available(text: &str) -> Result<ByteSize, DiscoveryError> {
    let row = text
        .lines()
        .nth(1)
        .ok_or_else(|| DiscoveryError::FieldNotFound {
            field: "row".to_string(),
            source_name: "df".to_string(),
        })?;
    // df -Pk columns: Filesystem, 1024-blocks, Used, Available, Capacity, Mount.
    let available = row.split_whitespace().nth(3).unwrap_or_default();
    let blocks = parse_u64(available, "Available", "df")?;
    Ok(ByteSize::from_bytes(blocks.saturating_mul(1024)))
}

fn parse_u64(s: &str, field: &str, source_name: &str) -> Result<u64, DiscoveryError> {
    s.trim().parse().map_err(|_| DiscoveryError::ParseValue {
        field: field.to_string(),
        value: s.to_string(),
        source_name: source_name.to_string(),
    })
}

fn parse_u32(s: &str, field: &str, source_name: &str) -> Result<u32, DiscoveryError> {
    s.trim().parse().map_err(|_| DiscoveryError::ParseValue {
        field: field.to_string(),
        value: s.to_string(),
        source_name: source_name.to_string(),
    })
}

fn parse_leading_u64(s: &str, field: &str, source_name: &str) -> Result<u64, DiscoveryError> {
    let digits: String = s.trim().chars().take_while(char::is_ascii_digit).collect();
    parse_u64(&digits, field, source_name).map_err(|_| DiscoveryError::ParseValue {
        field: field.to_string(),
        value: s.to_string(),
        source_name: source_name.to_string(),
    })
}

fn parse_leading_u32(s: &str, field: &str, source_name: &str) -> Result<u32, DiscoveryError> {
    let digits: String = s.trim().chars().take_while(char::is_ascii_digit).collect();
    parse_u32(&digits, field, source_name).map_err(|_| DiscoveryError::ParseValue {
        field: field.to_string(),
        value: s.to_string(),
        source_name: source_name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const SP_HARDWARE_M1: &str = "Hardware:

    Hardware Overview:

      Model Name: MacBook Air
      Model Identifier: MacBookAir10,1
      Chip: Apple M1
      Total Number of Cores: 8 (4 performance and 4 efficiency)
      Memory: 8 GB
      System Firmware Version: mBoot-18000.161.9
";

    const SP_HARDWARE_M3_MAX: &str = "Hardware:

    Hardware Overview:

      Chip: Apple M3 Max
      Total Number of Cores: 14 (10 performance and 4 efficiency)
      Memory: 48 GB
";

    const SYSCTL_INTEL: &str = "machdep.cpu.brand_string: Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz
hw.memsize: 17179869184
hw.ncpu: 12
";

    const CPUINFO: &str = "processor\t: 0
vendor_id\t: GenuineIntel
model name\t: Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz
cpu cores\t: 4

processor\t: 1
vendor_id\t: GenuineIntel
model name\t: Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz
cpu cores\t: 4
";

    const MEMINFO: &str = "MemTotal:       16384000 kB
MemFree:         1234567 kB
MemAvailable:    8000000 kB
";

    const DF_MACOS: &str = "Filesystem     1024-blocks      Used Available Capacity  Mounted on
/dev/disk3s1s1   239362496  11001500  78500996    13%    /
";

    const DF_LINUX: &str = "Filesystem     1K-blocks     Used Available Use% Mounted on
/dev/sda1      103081248 25000000  72836000  26% /
";

    #[derive(Default)]
    struct FakeProbe {
        commands: HashMap<String, String>,
        files: HashMap<String, String>,
    }

    impl FakeProbe {
        fn command(mut self, key: &str, out: &str) -> Self {
            self.commands.insert(key.to_string(), out.to_string());
            self
        }
        fn file(mut self, path: &str, out: &str) -> Self {
            self.files.insert(path.to_string(), out.to_string());
            self
        }
    }

    impl SystemProbe for FakeProbe {
        fn run(&self, command: &str, args: &[&str]) -> Result<String, DiscoveryError> {
            let key = std::iter::once(command)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.commands
                .get(&key)
                .cloned()
                .ok_or_else(|| DiscoveryError::Probe {
                    source_name: key,
                    message: "no fixture".to_string(),
                })
        }
        fn read_file(&self, path: &str) -> Result<String, DiscoveryError> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| DiscoveryError::Probe {
                    source_name: path.to_string(),
                    message: "no fixture".to_string(),
                })
        }
    }

    #[test]
    fn parses_apple_silicon() {
        let profile = parse_apple_silicon(SP_HARDWARE_M3_MAX).unwrap();
        let acc = profile.accelerator.unwrap();
        assert_eq!(acc.kind, AcceleratorKind::AppleSilicon);
        assert_eq!(acc.model, "m3_max");
        assert_eq!(acc.count, 1);
        assert_eq!(acc.memory.bytes(), 48 * GIB);
        assert_eq!(profile.memory.bytes(), 48 * GIB);
        let cpu = profile.cpu.unwrap();
        assert_eq!(cpu.model, "Apple M3 Max");
        assert_eq!(cpu.cores, 14);
        assert_eq!(profile.provider, "local");
    }

    #[test]
    fn parses_apple_silicon_m1_binary_memory() {
        let profile = parse_apple_silicon(SP_HARDWARE_M1).unwrap();
        // system_profiler "8 GB" == sysctl hw.memsize 8589934592 == 8 GiB.
        assert_eq!(profile.memory.bytes(), 8_589_934_592);
        assert_eq!(profile.accelerator.unwrap().model, "m1");
    }

    #[test]
    fn parses_macos_cpu() {
        let profile = parse_macos_cpu(SYSCTL_INTEL).unwrap();
        assert!(profile.accelerator.is_none());
        let cpu = profile.cpu.unwrap();
        assert_eq!(cpu.model, "Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz");
        assert_eq!(cpu.cores, 12);
        assert_eq!(profile.memory.bytes(), 17_179_869_184);
    }

    #[test]
    fn parses_linux_cpu() {
        let profile = parse_linux_cpu(CPUINFO, MEMINFO).unwrap();
        let cpu = profile.cpu.unwrap();
        assert_eq!(cpu.model, "Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz");
        assert_eq!(cpu.cores, 2);
        assert_eq!(profile.memory.bytes(), 16_384_000 * 1024);
    }

    #[test]
    fn parses_disk_free() {
        assert_eq!(
            parse_df_available(DF_MACOS).unwrap().bytes(),
            78_500_996 * 1024
        );
        assert_eq!(
            parse_df_available(DF_LINUX).unwrap().bytes(),
            72_836_000 * 1024
        );
    }

    #[test]
    fn malformed_apple_output_errs() {
        let err = parse_apple_silicon("garbage without fields").unwrap_err();
        assert!(matches!(err, DiscoveryError::FieldNotFound { .. }));
    }

    #[test]
    fn malformed_meminfo_errs() {
        assert!(matches!(
            parse_linux_cpu(CPUINFO, "MemTotal: not_a_number kB").unwrap_err(),
            DiscoveryError::ParseValue { .. }
        ));
    }

    #[test]
    fn discovers_macos_apple_silicon() {
        let probe = FakeProbe::default()
            .command("system_profiler SPHardwareDataType", SP_HARDWARE_M1)
            .command("df -Pk /", DF_MACOS);
        let report = discover_with(&probe, "macos").unwrap();
        assert_eq!(report.os, "macos");
        assert_eq!(report.disk_free.bytes(), 78_500_996 * 1024);
        assert_eq!(
            report.profile.accelerator.unwrap().kind,
            AcceleratorKind::AppleSilicon
        );
    }

    #[test]
    fn discovers_macos_cpu_fallback() {
        // No "Chip:" line -> falls back to sysctl.
        let probe = FakeProbe::default()
            .command(
                "system_profiler SPHardwareDataType",
                "Hardware:\n  Model Name: iMac\n",
            )
            .command(
                "sysctl machdep.cpu.brand_string hw.memsize hw.ncpu",
                SYSCTL_INTEL,
            )
            .command("df -Pk /", DF_MACOS);
        let report = discover_with(&probe, "macos").unwrap();
        assert!(report.profile.accelerator.is_none());
        let cpu = report.profile.cpu.unwrap();
        assert_eq!(cpu.cores, 12);
        assert_eq!(cpu.model, "Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz");
    }

    #[test]
    fn malformed_apple_silicon_does_not_fall_back() {
        // Has a "Chip:" line but a broken Memory line: parse_apple_silicon
        // fails with a non-FieldNotFound{Chip} error, which must propagate
        // rather than silently degrading to CPU-only.
        let probe = FakeProbe::default()
            .command(
                "system_profiler SPHardwareDataType",
                "Hardware:\n  Chip: Apple M3 Max\n  Total Number of Cores: 14\n  Memory: not_a_number GB\n",
            )
            .command("df -Pk /", DF_MACOS);
        let err = discover_with(&probe, "macos").unwrap_err();
        assert!(matches!(err, DiscoveryError::ParseValue { .. }));
    }

    #[test]
    fn discovers_linux() {
        let probe = FakeProbe::default()
            .file("/proc/cpuinfo", CPUINFO)
            .file("/proc/meminfo", MEMINFO)
            .command("df -Pk /", DF_LINUX);
        let report = discover_with(&probe, "linux").unwrap();
        assert_eq!(report.os, "linux");
        assert_eq!(report.disk_free.bytes(), 72_836_000 * 1024);
        assert_eq!(report.profile.memory.bytes(), 16_384_000 * 1024);
    }

    fn linux_probe() -> FakeProbe {
        FakeProbe::default()
            .file("/proc/cpuinfo", CPUINFO)
            .file("/proc/meminfo", MEMINFO)
            .command("df -Pk /", DF_LINUX)
    }

    #[test]
    fn linux_memory_is_capped_by_a_cgroup_v2_limit() {
        let probe = linux_probe().file("/sys/fs/cgroup/memory.max", "4294967296\n");
        let report = discover_with(&probe, "linux").unwrap();
        assert_eq!(report.profile.memory.bytes(), 4 << 30);
    }

    #[test]
    fn linux_memory_is_capped_by_a_cgroup_v1_limit() {
        let probe = linux_probe().file(
            "/sys/fs/cgroup/memory/memory.limit_in_bytes",
            "2147483648\n",
        );
        let report = discover_with(&probe, "linux").unwrap();
        assert_eq!(report.profile.memory.bytes(), 2 << 30);
    }

    #[test]
    fn unlimited_or_larger_cgroup_limits_do_not_change_host_memory() {
        let host = 16_384_000 * 1024;
        for limit in ["max\n", "9223372036854771712\n", "garbage"] {
            let probe = linux_probe().file("/sys/fs/cgroup/memory.max", limit);
            let report = discover_with(&probe, "linux").unwrap();
            assert_eq!(report.profile.memory.bytes(), host, "{limit}");
        }
    }

    const PS_PREFIX: &str = "powershell -NoProfile -NonInteractive -Command ";
    const PS_SYSTEM: &str =
        "Get-CimInstance Win32_ComputerSystem | Select-Object TotalPhysicalMemory | ConvertTo-Json";
    const PS_CPU: &str = "Get-CimInstance Win32_Processor | Select-Object Name,NumberOfLogicalProcessors | ConvertTo-Json";
    const PS_DISK: &str = "Get-CimInstance Win32_LogicalDisk -Filter \"DeviceID='$env:SystemDrive'\" | Select-Object FreeSpace | ConvertTo-Json";

    fn windows_probe(system: &str, cpu: &str, disk: &str) -> FakeProbe {
        FakeProbe::default()
            .command(&format!("{PS_PREFIX}{PS_SYSTEM}"), system)
            .command(&format!("{PS_PREFIX}{PS_CPU}"), cpu)
            .command(&format!("{PS_PREFIX}{PS_DISK}"), disk)
    }

    const WIN_SYSTEM: &str = "{\r\n  \"TotalPhysicalMemory\": 34290544640\r\n}\r\n";
    const WIN_CPU: &str = "{\r\n  \"Name\": \"AMD Ryzen 7 5800X 8-Core Processor      \",\r\n  \"NumberOfLogicalProcessors\": 16\r\n}\r\n";
    const WIN_DISK: &str = "{\r\n  \"FreeSpace\": 250000000000\r\n}\r\n";

    #[test]
    fn discovers_windows() {
        let probe = windows_probe(WIN_SYSTEM, WIN_CPU, WIN_DISK);
        let report = discover_with(&probe, "windows").unwrap();
        assert_eq!(report.os, "windows");
        assert_eq!(report.disk_free.bytes(), 250_000_000_000);
        assert_eq!(report.profile.memory.bytes(), 34_290_544_640);
        assert!(report.profile.accelerator.is_none());
        let cpu = report.profile.cpu.unwrap();
        assert_eq!(cpu.model, "AMD Ryzen 7 5800X 8-Core Processor");
        assert_eq!(cpu.cores, 16);
    }

    #[test]
    fn windows_sums_sockets_and_tolerates_bom_and_string_numbers() {
        let cpu = "\u{feff}[{\"Name\":\"Xeon\",\"NumberOfLogicalProcessors\":24},{\"Name\":\"Xeon\",\"NumberOfLogicalProcessors\":24}]";
        let system = "{\"TotalPhysicalMemory\": \"68719476736\"}";
        let probe = windows_probe(system, cpu, WIN_DISK);
        let report = discover_with(&probe, "windows").unwrap();
        assert_eq!(report.profile.cpu.unwrap().cores, 48);
        assert_eq!(report.profile.memory.bytes(), 68_719_476_736);
    }

    #[test]
    fn windows_missing_fields_err() {
        let probe = windows_probe("{}", WIN_CPU, WIN_DISK);
        assert!(matches!(
            discover_with(&probe, "windows").unwrap_err(),
            DiscoveryError::FieldNotFound { .. }
        ));
        let probe = windows_probe(WIN_SYSTEM, "{\"Name\": \"X\"}", WIN_DISK);
        assert!(matches!(
            discover_with(&probe, "windows").unwrap_err(),
            DiscoveryError::FieldNotFound { .. }
        ));
        let probe = windows_probe(WIN_SYSTEM, WIN_CPU, "{\"FreeSpace\": null}");
        assert!(matches!(
            discover_with(&probe, "windows").unwrap_err(),
            DiscoveryError::FieldNotFound { .. }
        ));
        // CIM returned nothing at all (no such drive).
        let probe = windows_probe(WIN_SYSTEM, WIN_CPU, "");
        assert!(matches!(
            discover_with(&probe, "windows").unwrap_err(),
            DiscoveryError::ParseValue { .. }
        ));
    }

    #[test]
    fn windows_malformed_output_errs() {
        let probe = windows_probe("not json at all", WIN_CPU, WIN_DISK);
        assert!(matches!(
            discover_with(&probe, "windows").unwrap_err(),
            DiscoveryError::ParseValue { .. }
        ));
        let probe = windows_probe(WIN_SYSTEM, WIN_CPU, "{\"FreeSpace\": \"lots\"}");
        assert!(matches!(
            discover_with(&probe, "windows").unwrap_err(),
            DiscoveryError::ParseValue { .. }
        ));
    }

    #[test]
    fn windows_probe_failure_propagates() {
        let probe = FakeProbe::default();
        assert!(matches!(
            discover_with(&probe, "windows").unwrap_err(),
            DiscoveryError::Probe { .. }
        ));
    }

    #[test]
    fn rejects_unsupported_os() {
        let probe = FakeProbe::default();
        assert!(matches!(
            discover_with(&probe, "freebsd").unwrap_err(),
            DiscoveryError::UnsupportedOs(_)
        ));
    }

    #[test]
    #[ignore = "runs real OS probes; host-dependent"]
    fn discover_on_host() {
        let report = discover_report().expect("discovery on host");
        assert!(report.profile.memory.bytes() > 0);
        assert!(report.disk_free.bytes() > 0);
    }
}
