//! Test fixtures for the hardware parsers, with provenance.
//!
//! **Real captures** are marked CAPTURED. Everything else is CONSTRUCTED: no
//! GPU of that vendor was available, so the text was written to match the
//! format the vendor's own source or documentation specifies (cited per
//! fixture) with plausible values. Constructed fixtures prove the parser
//! agrees with the documented shape; they do not prove a live tool's
//! output byte for byte. The ignored live test in `amicu-modal` and any run
//! on real hardware are the check for that.

/// CONSTRUCTED. `nvidia-smi --query-gpu=index,name,utilization.gpu,
/// memory.used,memory.total,temperature.gpu,power.draw
/// --format=csv,noheader,nounits` (driver 550-series layout: `, ` separated,
/// MiB / C / W without units, unsupported fields as `[N/A]`). Format per the
/// `nvidia-smi` man page ("--query-gpu", "--format"). GPU 1 models an idle
/// card whose sensors are unsupported.
pub(super) const NVIDIA_CSV: &str = "\
0, NVIDIA H100 80GB HBM3, 37, 21412, 81559, 45, 289.45
1, NVIDIA H100 80GB HBM3, 0, 4, 81559, [N/A], [N/A]";

/// CONSTRUCTED. `amd-smi metric --json`, ROCm 6.x array form. Structure and
/// key names from ROCm/amdsmi `amdsmi_cli/amdsmi_commands.py` (`usage`
/// -> `gfx_activity`, `power` -> `socket_power`, `temperature` -> `edge` /
/// `hotspot` / `mem`, `mem_usage` -> `total_vram` / `used_vram`, sizes in MB
/// = MiB) and the `{"value": .., "unit": ..}` convention from its
/// CHANGELOG ("Added units of measure to JSON output"). Unavailable readings
/// are the string `"N/A"`. The ROCm 7.x `{"gpu_data": [...]}` wrapper
/// (`amdsmi_logger.py: combine_arrays_to_json`) is covered by wrapping this.
pub(super) const AMD_SMI_JSON: &str = r#"[
    {
        "gpu": 0,
        "usage": {
            "gfx_activity": {"value": 41, "unit": "%"},
            "umc_activity": {"value": 12, "unit": "%"}
        },
        "power": {
            "socket_power": {"value": 142, "unit": "W"},
            "power_management": "ENABLED"
        },
        "temperature": {
            "edge": {"value": 38, "unit": "C"},
            "hotspot": {"value": 44, "unit": "C"},
            "mem": {"value": 40, "unit": "C"}
        },
        "mem_usage": {
            "total_vram": {"value": 262128, "unit": "MB"},
            "used_vram": {"value": 283, "unit": "MB"},
            "free_vram": {"value": 261845, "unit": "MB"}
        }
    },
    {
        "gpu": 1,
        "usage": {"gfx_activity": {"value": 0, "unit": "%"}},
        "power": {"socket_power": "N/A"},
        "temperature": {
            "edge": "N/A",
            "hotspot": {"value": 51, "unit": "C"},
            "mem": "N/A"
        },
        "mem_usage": {
            "total_vram": {"value": 262128, "unit": "MB"},
            "used_vram": "N/A"
        }
    }
]"#;

/// CONSTRUCTED. `rocm-smi --showuse --showmeminfo vram --showtemp --showpower
/// --json`. Keys from ROCm/rocm_smi_lib `python_smi_tools/rocm_smi.py`
/// (v4.0.0): `GPU use (%)`, `<MEM> Total Memory (B)`, `<MEM> Total Used
/// Memory (B)`, `Temperature (Sensor <name>) (C)`, and
/// `<Average|Current Socket> Graphics Package Power (W)`; JSON mode prints
/// every value as a string and adds a `system` object. Card 1 has only a
/// junction sensor and reports socket power.
pub(super) const ROCM_SMI_JSON: &str = r#"{
  "card0": {
    "GPU use (%)": "12",
    "VRAM Total Memory (B)": "17163091968",
    "VRAM Total Used Memory (B)": "13840384",
    "Temperature (Sensor edge) (C)": "37.0",
    "Temperature (Sensor junction) (C)": "39.0",
    "Average Graphics Package Power (W)": "7.0"
  },
  "card1": {
    "GPU use (%)": "88",
    "VRAM Total Memory (B)": "17163091968",
    "VRAM Total Used Memory (B)": "9663676416",
    "Temperature (Sensor junction) (C)": "55.0",
    "Current Socket Graphics Package Power (W)": "95.5"
  },
  "system": {"Driver version": "6.8.5"}
}"#;

/// CONSTRUCTED. The `key=value` body the probe writes for
/// `/sys/class/drm/card0/device` of an amdgpu card: `gpu_busy_percent` (%),
/// `mem_info_vram_used` / `_total` (bytes), `temp1_input` (milli-C) and
/// `power1_average` (micro-W) from hwmon, per the kernel's amdgpu
/// documentation (Documentation/gpu/amdgpu/driver-misc.rst, hwmon ABI).
pub(super) const AMD_SYSFS_CARD0: &str = "\
vendor=0x1002
gpu_busy_percent=23
mem_info_vram_used=1073741824
mem_info_vram_total=17163091968
temp1_input=44000
power1_average=31500000";

/// CONSTRUCTED. `xpu-smi discovery -j`. Shape (`device_list`, `device_id`,
/// `device_name`) from intel/xpumanager `ial/cmn/cmd_discovery.cpp`
/// (main @ 2e14cf9c).
pub(super) const XPU_DISCOVERY_JSON: &str = r#"{
    "device_list": [
        {"device_id": 0, "device_name": "Intel(R) Data Center GPU Max 1550", "vendor_name": "Intel(R) Corporation", "uuid": "00000000-0000-0000-0000-000000000001"},
        {"device_id": 1, "device_name": "Intel(R) Data Center GPU Max 1100", "vendor_name": "Intel(R) Corporation", "uuid": "00000000-0000-0000-0000-000000000002"}
    ]
}"#;

/// CONSTRUCTED. `xpu-smi discovery -d 0 -j`: the per-device form adds the
/// integer `memory_physical_size_byte` (`cmd_discovery.cpp` promotes it from
/// text to an integer).
pub(super) const XPU_DEVICE0_JSON: &str = r#"{
    "device_id": 0,
    "device_name": "Intel(R) Data Center GPU Max 1550",
    "vendor_name": "Intel(R) Corporation",
    "memory_physical_size_byte": 137438953472
}"#;

/// CONSTRUCTED. As [`XPU_DEVICE0_JSON`] for device 1.
pub(super) const XPU_DEVICE1_JSON: &str = r#"{
    "device_id": 1,
    "device_name": "Intel(R) Data Center GPU Max 1100",
    "memory_physical_size_byte": 51539607552
}"#;

/// CONSTRUCTED. `xpu-smi stats -j` for two devices (an array). Layout from
/// `ial/cmn/cmd_stats.cpp`: `device_index`, then per-tile blocks:
/// `utilization.gpu_percent.tile_N` = number, and
/// `power.gpu_power_w` / `temperature.gpu_core_celsius` / `memory.used_mib`
/// `.tile_N` = `{avg,min,max,current}`. Device 0 is a two-tile part; device
/// 1 reports no power and no memory block.
pub(super) const XPU_STATS_JSON: &str = r#"[
    {
        "device_index": 0,
        "pci_bdf": "0000:18:00.0",
        "device_type": "discrete",
        "utilization": {"gpu_percent": {"tile_0": 40.0, "tile_1": 20.0}},
        "power": {"gpu_power_w": {
            "tile_0": {"avg": 205.0, "min": 200.0, "max": 211.0, "current": 210.5},
            "tile_1": {"avg": 190.0, "min": 188.0, "max": 192.0, "current": 190.25}
        }},
        "temperature": {"gpu_core_celsius": {
            "tile_0": {"avg": 57.0, "min": 56.0, "max": 58.0, "current": 58.0},
            "tile_1": {"avg": 60.0, "min": 59.0, "max": 61.0, "current": 61.0}
        }},
        "memory": {"used_mib": {
            "tile_0": {"avg": 1024.0, "min": 1024.0, "max": 1024.0, "current": 1024.0},
            "tile_1": {"avg": 2048.0, "min": 2048.0, "max": 2048.0, "current": 2048.0}
        }}
    },
    {
        "device_index": 1,
        "pci_bdf": "0000:3b:00.0",
        "device_type": "discrete",
        "utilization": {"gpu_percent": {"tile_0": 0.0}},
        "temperature": {"gpu_core_celsius": {
            "tile_0": {"avg": 40.0, "min": 40.0, "max": 40.0, "current": 40.0}
        }}
    }
]"#;

/// CONSTRUCTED. `xpu-smi stats -d 0 -j` for a single device: a bare object
/// (`cmd_stats.cpp` only wraps in an array when several devices are queried).
pub(super) const XPU_STATS_SINGLE_JSON: &str = r#"{
    "device_index": 0,
    "utilization": {"gpu_percent": {"tile_0": 7.5}}
}"#;

/// CAPTURED on an Apple M1 (macOS 15, Darwin 24.6), via the probe's own
/// filter of `ioreg -r -d 1 -w 0 -c IOAccelerator`. Trimmed to the lines the
/// probe keeps; `PerformanceStatistics` is verbatim except for the lines'
/// leading ioreg indentation. Note the decoy `"In use system memory
/// (driver)"` key.
pub(super) const IOREG_M1: &str = r#"+-o AGXAcceleratorG13G_B0  <class AGXAcceleratorG13G_B0, id 0x10000079d, registered, matched, active, busy 0 (911 ms), retain 46>
      "PerformanceStatistics" = {"In use system memory (driver)"=0,"Alloc system memory"=2286698496,"Tiler Utilization %"=6,"recoveryCount"=0,"lastRecoveryTime"=0,"Renderer Utilization %"=5,"TiledSceneBytes"=1507328,"Device Utilization %"=6,"SplitSceneCount"=0,"Allocated PB Size"=43515904,"In use system memory"=210010112}
      "model" = "Apple M1""#;

/// CONSTRUCTED probe sections for a cgroup v2 container with a 64 GiB limit.
/// File formats per the kernel's cgroup-v2 documentation: `memory.current`
/// and `memory.max` are one number (or `max`); `cpu.stat` is `key value`
/// lines including `usage_usec`.
pub(super) const CGROUP_V2_LIMITED: &str = "\
@@amicu:cgroup2-memory-current
13240508416
@@amicu:cgroup2-memory-max
68719476736
@@amicu:cgroup2-cpu-stat
usage_usec 123456789
user_usec 100000000
system_usec 23456789
nr_periods 0
nr_throttled 0
";

/// CONSTRUCTED probe sections for cgroup v1 with no memory limit: the kernel
/// reports `9223372036854771712` (`i64::MAX` rounded down to a page) and
/// `cpuacct.usage` is cumulative nanoseconds (kernel cgroup-v1 docs).
pub(super) const CGROUP_V1_UNLIMITED: &str = "\
@@amicu:cgroup1-memory-usage
2147483648
@@amicu:cgroup1-memory-limit
9223372036854771712
@@amicu:cgroup1-cpuacct-usage
5000000000
";

/// CONSTRUCTED. The `MemTotal` / `MemAvailable` lines of `/proc/meminfo`
/// (kB), which is all the probe keeps.
pub(super) const MEMINFO_ONLY: &str = "\
@@amicu:meminfo
MemTotal:       16384000 kB
MemAvailable:   10240000 kB
";
