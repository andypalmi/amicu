//! `amicu catalog hardware|presets|fits` - read-only, offline views of the
//! data compiled into amicu. Nothing here needs a project, a network or
//! credentials.

use std::io::Write;

use amicu_core::ByteSize;
use amicu_core::diagnostic::{Severity, catalog as codes};
use amicu_core::resource::{AcceleratorKind, GgufVariant, Quantization, Runtime};
use amicu_providers::consumer;
use amicu_recommend::{Preset, WizardAnswers, build_graph};
use amicu_registry::{FitConfig, check_variant_fit};
use serde::Serialize;

use crate::OutputFormat;
use crate::commands::init::TIERS;
use crate::error::CliError;
use crate::host::HostInfo;
use crate::write_json;

fn io(r: std::io::Result<()>) -> Result<(), CliError> {
    r.map_err(|e| CliError::io("write failed", e))
}

const GIB: u64 = 1024 * 1024 * 1024;

// --- hardware ---------------------------------------------------------------

/// One catalogued accelerator.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct HardwareRow {
    pub id: String,
    /// `gpu` or `apple_silicon`.
    pub kind: String,
    /// Memory sizes it is sold with, in GiB, smallest first.
    pub memory_gib: Vec<u64>,
    /// Memory bandwidth of the first size, in GB/s, when catalogued.
    pub bandwidth_gbps: Option<u32>,
}

/// Every catalogued consumer GPU and Apple Silicon chip.
pub(crate) fn hardware_rows() -> Vec<HardwareRow> {
    let mut rows = Vec::new();
    for p in consumer::catalog() {
        let Some(acc) = &p.accelerator else { continue };
        if acc.kind == AcceleratorKind::AppleSilicon {
            continue;
        }
        rows.push(HardwareRow {
            id: p.id.to_string(),
            kind: "gpu".to_owned(),
            memory_gib: vec![acc.memory.bytes() / GIB],
            bandwidth_gbps: acc.memory_bandwidth_gbps,
        });
    }
    for chip in consumer::apple_silicon_chips() {
        rows.push(HardwareRow {
            id: chip.id.to_owned(),
            kind: "apple_silicon".to_owned(),
            memory_gib: chip
                .configs
                .iter()
                .map(|c| u64::from(c.memory_gib))
                .collect(),
            bandwidth_gbps: chip.configs.first().map(|c| c.bandwidth_gbps),
        });
    }
    rows
}

pub(crate) fn hardware(output: OutputFormat, out: &mut dyn Write) -> Result<u8, CliError> {
    let rows = hardware_rows();
    if output.is_json() {
        write_json(out, &rows)?;
        return Ok(0);
    }
    io(writeln!(out, "{:<18} {:<14} MEMORY (GiB)", "ID", "KIND"))?;
    for r in &rows {
        let mem = r
            .memory_gib
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        io(writeln!(out, "{:<18} {:<14} {mem}", r.id, r.kind))?;
    }
    Ok(0)
}

// --- presets ----------------------------------------------------------------

#[derive(Debug, Serialize)]
struct PresetRow<'a> {
    id: &'a str,
    name: &'a str,
    tagline: &'a str,
    fit: &'a str,
}

pub(crate) fn presets(output: OutputFormat, out: &mut dyn Write) -> Result<u8, CliError> {
    let rows: Vec<PresetRow<'_>> = amicu_recommend::catalog()
        .iter()
        .map(|p| PresetRow {
            id: p.id.as_str(),
            name: &p.name,
            tagline: &p.tagline,
            fit: &p.fit,
        })
        .collect();
    if output.is_json() {
        write_json(out, &rows)?;
        return Ok(0);
    }
    io(writeln!(out, "{:<30} {:<30} SUMMARY", "ID", "NAME"))?;
    for r in &rows {
        io(writeln!(out, "{:<30} {:<30} {}", r.id, r.name, r.tagline))?;
    }
    Ok(0)
}

// --- fits -------------------------------------------------------------------

/// Whether something fits the detected host.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Fits,
    TooBig,
    /// The estimate could not be computed.
    Unknown,
}

impl Verdict {
    fn label(self) -> &'static str {
        match self {
            Verdict::Fits => "fits",
            Verdict::TooBig => "too big",
            Verdict::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelFit {
    pub id: String,
    pub reference: String,
    pub file: String,
    pub size_bytes: u64,
    pub verdict: Verdict,
    /// Memory left after the weights and the OS reserve, in bytes.
    pub headroom_bytes: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct PresetFit {
    pub id: String,
    pub name: String,
    pub runtime: String,
    pub verdict: Verdict,
    /// Why it does not fit, or why it is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct HostSummary {
    pub os: String,
    pub memory_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accelerator: Option<String>,
    /// Total memory of the pool the fit engine evaluates: GPU VRAM summed over
    /// all cards when a GPU was detected, otherwise system or unified memory.
    pub pool_bytes: u64,
    /// What weights may use in that pool: after the per-device reserve and,
    /// for several GPUs, the tensor-parallel haircut. Same figure as the
    /// engine's `available`.
    pub available_bytes: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct FitsReport {
    pub version: &'static str,
    pub host: HostSummary,
    pub starter_models: Vec<ModelFit>,
    pub presets: Vec<PresetFit>,
}

/// Compute what fits `host`. Pure given `host`; reads only compiled-in data.
pub(crate) fn fits_report(host: &HostInfo) -> FitsReport {
    let hw = host.fit_profile();
    let cfg = FitConfig::default();

    let starter_models = TIERS
        .iter()
        .map(|t| {
            let variant = GgufVariant {
                quantization: if t.quantization == "q8_0" {
                    Quantization::Q8_0
                } else {
                    Quantization::Q4KM
                },
                file: t.file.to_owned(),
                size: Some(ByteSize::from_bytes(t.size_bytes)),
                sha256: None,
            };
            // The size is declared, so the parameter count is not used.
            let r = check_variant_fit(0, &variant, &hw, &cfg);
            ModelFit {
                id: t.id.to_owned(),
                reference: t.repo.to_owned(),
                file: t.file.to_owned(),
                size_bytes: t.size_bytes,
                verdict: if r.fits {
                    Verdict::Fits
                } else {
                    Verdict::TooBig
                },
                headroom_bytes: r.headroom.bytes(),
            }
        })
        .collect();

    let presets = amicu_recommend::catalog()
        .iter()
        .map(|p| preset_fit(p, host))
        .collect();

    // The pool and what is usable in it, from the same engine the verdicts use.
    let probe = check_variant_fit(
        0,
        &GgufVariant {
            quantization: Quantization::Q4KM,
            file: String::new(),
            size: Some(ByteSize::from_bytes(0)),
            sha256: None,
        },
        &hw,
        &cfg,
    );
    FitsReport {
        version: "v1",
        host: HostSummary {
            os: host.os.clone(),
            memory_bytes: host.profile.memory.bytes(),
            accelerator: host
                .nvidia
                .as_ref()
                .map(|g| g.model.clone())
                .or_else(|| host.profile.accelerator.as_ref().map(|a| a.model.clone())),
            pool_bytes: probe.device_memory.bytes(),
            available_bytes: probe.available.bytes(),
        },
        starter_models,
        presets,
    }
}

/// Build the preset's graph with default answers, put the detected host under
/// every deployment, and ask the engine's own fit checks (`AMICU-MEM-001/002`).
fn preset_fit(preset: &Preset, host: &HostInfo) -> PresetFit {
    let mut row = PresetFit {
        id: preset.id.to_string(),
        name: preset.name.clone(),
        runtime: String::new(),
        verdict: Verdict::Unknown,
        detail: None,
    };
    let answers = WizardAnswers {
        context_length: 8192,
        concurrency: 1,
        eu_residency_required: false,
        budget: None,
        max_latency_ms: None,
        priority: None,
    };
    let mut graph = match build_graph(preset, &answers) {
        Ok(g) => g,
        Err(e) => {
            row.detail = Some(e.to_string());
            return row;
        }
    };
    let mut runtimes: Vec<&str> = graph
        .deployments
        .iter()
        .map(|d| match d.runtime {
            Runtime::LlamaCpp(_) => "llama_cpp",
            Runtime::Vllm(_) => "vllm",
        })
        .collect();
    runtimes.dedup();
    row.runtime = runtimes.join(", ");

    let template = host.fit_profile();
    for hw in &mut graph.hardware_profiles {
        let mut local = template.clone();
        local.id = hw.id.clone();
        "local".clone_into(&mut local.provider);
        *hw = local;
    }
    let report = amicu_engine::report_for(&graph);
    let too_big = report.diagnostics.iter().find(|d| {
        d.severity == Severity::Error
            && (d.code == codes::MEM_VRAM_EXCEEDED || d.code == codes::MEM_ARTIFACT_OVERSIZE)
    });
    if let Some(d) = too_big {
        row.verdict = Verdict::TooBig;
        row.detail = Some(
            d.message
                .split(" (to try anyway")
                .next()
                .unwrap_or(&d.message)
                .to_owned(),
        );
    } else if let Some(d) = report
        .diagnostics
        .iter()
        .find(|d| d.severity == Severity::Error)
    {
        row.detail = Some(d.message.clone());
    } else {
        row.verdict = Verdict::Fits;
    }
    row
}

pub(crate) fn fits(
    host: Result<&HostInfo, &str>,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let host = host.map_err(|e| {
        CliError::Message(format!(
            "cannot detect this machine, so cannot say what fits: {e}"
        ))
    })?;
    let report = fits_report(host);
    if output.is_json() {
        write_json(out, &report)?;
        return Ok(0);
    }
    io(writeln!(out, "Host: {}", host.describe()))?;
    io(writeln!(
        out,
        "Estimates are weights only, after a 2 GiB reserve; context and runtime overhead come on top.\n"
    ))?;
    io(writeln!(out, "Starter models (used by `amicu init`)"))?;
    io(writeln!(
        out,
        "  {:<14} {:>10}  {:<8} HEADROOM",
        "MODEL", "SIZE", "FIT"
    ))?;
    for m in &report.starter_models {
        io(writeln!(
            out,
            "  {:<14} {:>10}  {:<8} {}",
            m.id,
            ByteSize::from_bytes(m.size_bytes).human(),
            m.verdict.label(),
            ByteSize::from_bytes(m.headroom_bytes).human()
        ))?;
    }
    io(writeln!(
        out,
        "\nPresets (default answers, this host as the hardware)"
    ))?;
    io(writeln!(
        out,
        "  {:<30} {:<10} {:<8}",
        "PRESET", "RUNTIME", "FIT"
    ))?;
    for p in &report.presets {
        io(writeln!(
            out,
            "  {:<30} {:<10} {}",
            p.id,
            p.runtime,
            p.verdict.label()
        ))?;
        if let Some(d) = &p.detail {
            io(writeln!(out, "      {d}"))?;
        }
    }
    Ok(0)
}
