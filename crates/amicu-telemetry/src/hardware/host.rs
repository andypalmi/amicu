//! Container / host memory and CPU from the probe's `cgroup*` and `meminfo`
//! sections. Preference order for memory: cgroup v2, cgroup v1,
//! `/proc/meminfo`.
//!
//! * cgroup v2: `memory.current` (bytes), `memory.max` (bytes or `max` =
//!   unlimited -> `None`), `cpu.stat` `usage_usec`.
//! * cgroup v1: `memory.usage_in_bytes`, `memory.limit_in_bytes` (the kernel
//!   reports ~2^63 for unlimited -> `None`), `cpuacct.usage` (nanoseconds).
//! * `/proc/meminfo`: used = `MemTotal - MemAvailable`, limit = `MemTotal`
//!   (kB). This is the machine, or the sandbox kernel's view under gVisor.

use super::{HostSample, MemorySource, Section};

/// cgroup v1 "no limit" is a page-rounded `i64::MAX`; anything this large is
/// not a real limit.
const V1_UNLIMITED_FLOOR: u64 = 1 << 60;

pub(super) fn parse(sections: &[Section<'_>]) -> HostSample {
    let body = |name: &str| sections.iter().find(|s| s.name == name).map(|s| s.body);
    let number = |name: &str| body(name).and_then(first_u64);
    let mut host = HostSample::default();

    if let Some(current) = number("cgroup2-memory-current") {
        host.mem_used_bytes = Some(current);
        host.mem_limit_bytes = body("cgroup2-memory-max").and_then(first_u64);
        host.memory_source = Some(MemorySource::CgroupV2);
    } else if let Some(usage) = number("cgroup1-memory-usage") {
        host.mem_used_bytes = Some(usage);
        host.mem_limit_bytes = number("cgroup1-memory-limit").filter(|l| *l < V1_UNLIMITED_FLOOR);
        host.memory_source = Some(MemorySource::CgroupV1);
    } else if let Some(text) = body("meminfo") {
        let total = meminfo_kb(text, "MemTotal").and_then(|kb| kb.checked_mul(1024));
        let available = meminfo_kb(text, "MemAvailable").and_then(|kb| kb.checked_mul(1024));
        if total.is_some() {
            host.mem_limit_bytes = total;
            host.mem_used_bytes = total.zip(available).map(|(t, a)| t.saturating_sub(a));
            host.memory_source = Some(MemorySource::ProcMeminfo);
        }
    }

    host.cpu_usage_usec = body("cgroup2-cpu-stat")
        .and_then(|text| {
            text.lines().find_map(|l| {
                let (k, v) = l.split_once(' ')?;
                (k == "usage_usec").then(|| v.trim().parse::<u64>().ok())?
            })
        })
        .or_else(|| number("cgroup1-cpuacct-usage").map(|ns| ns / 1000));
    host
}

fn first_u64(text: &str) -> Option<u64> {
    text.split_whitespace().next()?.parse().ok()
}

fn meminfo_kb(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let rest = l.strip_prefix(key)?.strip_prefix(':')?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{fixtures as fx, split_sections};

    fn host(probe: &str) -> HostSample {
        parse(&split_sections(probe))
    }

    #[test]
    fn cgroup_v2_with_a_limit() {
        let h = host(fx::CGROUP_V2_LIMITED);
        assert_eq!(h.memory_source, Some(MemorySource::CgroupV2));
        assert_eq!(h.mem_used_bytes, Some(13_240_508_416));
        assert_eq!(h.mem_limit_bytes, Some(68_719_476_736));
        assert_eq!(h.cpu_usage_usec, Some(123_456_789));
    }

    #[test]
    fn cgroup_v2_max_means_no_limit() {
        let h = host(&fx::CGROUP_V2_LIMITED.replace("68719476736", "max"));
        assert_eq!(h.mem_used_bytes, Some(13_240_508_416));
        assert_eq!(h.mem_limit_bytes, None);
    }

    #[test]
    fn cgroup_v1_converts_cpuacct_ns_and_drops_the_unlimited_sentinel() {
        let h = host(fx::CGROUP_V1_UNLIMITED);
        assert_eq!(h.memory_source, Some(MemorySource::CgroupV1));
        assert_eq!(h.mem_used_bytes, Some(2_147_483_648));
        assert_eq!(h.mem_limit_bytes, None);
        assert_eq!(h.cpu_usage_usec, Some(5_000_000));
        let limited = host(&fx::CGROUP_V1_UNLIMITED.replace("9223372036854771712", "4294967296"));
        assert_eq!(limited.mem_limit_bytes, Some(4_294_967_296));
    }

    #[test]
    fn proc_meminfo_is_the_last_resort() {
        let h = host(fx::MEMINFO_ONLY);
        assert_eq!(h.memory_source, Some(MemorySource::ProcMeminfo));
        assert_eq!(h.mem_limit_bytes, Some(16_384_000 * 1024));
        assert_eq!(h.mem_used_bytes, Some((16_384_000 - 10_240_000) * 1024));
        assert_eq!(h.cpu_usage_usec, None);
    }

    #[test]
    fn no_sources_means_all_none() {
        assert_eq!(host("@@amicu:probe-v1\n"), HostSample::default());
        // Unparseable content is not a reading.
        assert_eq!(
            host("@@amicu:cgroup2-memory-current\nbanana\n"),
            HostSample::default()
        );
    }
}
