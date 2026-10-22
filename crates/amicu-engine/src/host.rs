//! Host check: compare a local hardware profile with the machine `plan` runs on.
//!
//! Pure on purpose. The caller detects the host (see `amicu-cli`'s `host`
//! module, which shells out) and passes a [`HostFacts`] in, so this logic is
//! testable without touching the real machine.

use amicu_core::ByteSize;
use amicu_core::diagnostic::{Diagnostic, Severity, catalog};
use amicu_core::graph::Graph;
use amicu_core::resource::is_local_provider;

/// What was detected about the machine running the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostFacts {
    /// Total system (or Apple unified) memory.
    pub memory: ByteSize,
    /// Memory of one accelerator when one was detected: unified memory on
    /// Apple Silicon, per-card VRAM for NVIDIA. `None` when no accelerator
    /// was detected, in which case a declared accelerator is not compared.
    pub accelerator_memory: Option<ByteSize>,
}

/// A declared figure is accepted up to the largest of: 5% over the detected
/// one, the detected one plus 256 MiB, and the detected one rounded to the
/// nearest GiB. Linux `MemTotal` is a few percent below installed RAM (a
/// "16GiB" machine reports about 15.6 GiB, an "8GiB" one about 7.6 GiB), and
/// a percentage alone is too tight on small machines.
const TOLERANCE_PERCENT: u64 = 5;
const TOLERANCE_FLOOR: u64 = 256 * 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;

fn exceeds(declared: ByteSize, detected: ByteSize) -> bool {
    let d = detected.bytes();
    let percent = u64::try_from(u128::from(d) * u128::from(100 + TOLERANCE_PERCENT) / 100)
        .unwrap_or(u64::MAX);
    let nearest_gib = d.saturating_add(GIB / 2) / GIB * GIB;
    let allowed = percent
        .max(d.saturating_add(TOLERANCE_FLOOR))
        .max(nearest_gib);
    declared.bytes() > allowed
}

/// `AMICU-MEM-003` warnings, one per local deployment whose hardware profile
/// declares more memory than `host` has. Deployments on remote providers are
/// never checked: their profile describes a machine that is not this one.
///
/// A warning, never an error: `plan` is often run somewhere other than the
/// target (CI, a laptop planning for a workstation), so a mismatch is a
/// prompt to check, not proof of a mistake.
#[must_use]
pub fn host_diagnostics(graph: &Graph, host: &HostFacts) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for d in &graph.deployments {
        let Some(hw) = graph
            .hardware_profiles
            .iter()
            .find(|h| h.id == d.hardware_profile)
        else {
            continue;
        };
        if !is_local_provider(&hw.provider) {
            continue;
        }
        let mut over = Vec::new();
        if exceeds(hw.memory, host.memory) {
            over.push(format!(
                "memory {} but this machine has {}",
                hw.memory.human(),
                host.memory.human()
            ));
        }
        if let (Some(acc), Some(have)) = (&hw.accelerator, host.accelerator_memory)
            && exceeds(acc.memory, have)
        {
            over.push(format!(
                "accelerator memory {} but this machine's accelerator has {}",
                acc.memory.human(),
                have.human()
            ));
        }
        if over.is_empty() {
            continue;
        }
        let mut diag = Diagnostic::new(
            catalog::MEM_PROFILE_EXCEEDS_HOST,
            Severity::Warning,
            format!(
                "hardware profile `{}` declares {}; a model sized for it may fail to load here. \
                 Fix the profile, or ignore this if you are planning for another machine",
                hw.id,
                over.join(" and ")
            ),
        );
        diag.resource = Some(format!("deployment.{}", d.id));
        out.push(diag);
    }
    out
}

#[cfg(test)]
mod tests {
    use amicu_core::resource::{
        Accelerator, AcceleratorKind, Deployment, HardwareProfile, LlamaCppSettings, Runtime,
    };

    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn gib(n: u64) -> ByteSize {
        ByteSize::from_bytes(n * GIB)
    }

    fn graph(provider: &str, memory: ByteSize, accel: Option<ByteSize>) -> Graph {
        let mut g = Graph::new();
        g.hardware_profiles.push(HardwareProfile {
            id: "hw".into(),
            provider: provider.to_owned(),
            region: None,
            accelerator: accel.map(|memory| Accelerator {
                kind: AcceleratorKind::Gpu,
                model: "x".into(),
                count: 1,
                memory,
                memory_bandwidth_gbps: None,
            }),
            cpu: None,
            memory,
            interconnect: None,
            price: None,
            runpod: None,
            gcp: None,
        });
        g.deployments.push(Deployment {
            scaling: None,
            memory_check: amicu_core::resource::MemoryCheck::default(),
            api_key: None,
            id: "dep".into(),
            model: "m".into(),
            hardware_profile: "hw".into(),
            runtime: Runtime::LlamaCpp(LlamaCppSettings::default()),
        });
        g
    }

    fn host(mem: u64, acc: Option<u64>) -> HostFacts {
        HostFacts {
            memory: gib(mem),
            accelerator_memory: acc.map(gib),
        }
    }

    #[test]
    fn declared_memory_above_host_warns() {
        let d = host_diagnostics(&graph("local_apple", gib(48), None), &host(8, None));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, catalog::MEM_PROFILE_EXCEEDS_HOST);
        assert_eq!(d[0].severity, Severity::Warning);
        assert_eq!(d[0].resource.as_deref(), Some("deployment.dep"));
        assert!(d[0].message.contains("48.0 GiB"), "{}", d[0].message);
        assert!(d[0].message.contains("8.0 GiB"), "{}", d[0].message);
    }

    #[test]
    fn matching_or_smaller_profile_is_clean() {
        assert_eq!(
            host_diagnostics(&graph("local", gib(16), None), &host(16, None)).len(),
            0
        );
        assert_eq!(
            host_diagnostics(&graph("local", gib(8), None), &host(16, None)).len(),
            0
        );
    }

    #[test]
    fn linux_memtotal_slack_is_tolerated() {
        // 16 GiB declared on a host reporting 15.6 GiB.
        let h = HostFacts {
            memory: ByteSize::from_bytes(16_750_000_000),
            accelerator_memory: None,
        };
        assert_eq!(
            host_diagnostics(&graph("local_cpu", gib(16), None), &h).len(),
            0
        );
        assert_eq!(
            host_diagnostics(&graph("local_cpu", gib(32), None), &h).len(),
            1
        );
    }

    #[test]
    fn small_machines_round_to_the_nearest_gib() {
        // "8GiB" declared on a host reporting 7.6 GiB (5% alone would warn).
        let h = HostFacts {
            memory: ByteSize::from_bytes(8_150_000_000),
            accelerator_memory: None,
        };
        assert_eq!(host_diagnostics(&graph("local", gib(8), None), &h).len(), 0);
        assert_eq!(
            host_diagnostics(&graph("local", gib(16), None), &h).len(),
            1
        );
    }

    #[test]
    fn accelerator_memory_is_compared_only_when_detected() {
        let g = graph("on_prem", gib(32), Some(gib(24)));
        assert_eq!(host_diagnostics(&g, &host(64, None)).len(), 0);
        let d = host_diagnostics(&g, &host(64, Some(8)));
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("accelerator memory"));
    }

    #[test]
    fn both_figures_over_give_one_diagnostic() {
        let d = host_diagnostics(&graph("local", gib(48), Some(gib(48))), &host(8, Some(8)));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn remote_providers_are_never_checked() {
        assert_eq!(
            host_diagnostics(&graph("modal", gib(640), Some(gib(80))), &host(8, Some(8))).len(),
            0
        );
    }
}
