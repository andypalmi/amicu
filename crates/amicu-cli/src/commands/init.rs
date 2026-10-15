//! `amicu init [--example [mac|linux-cuda|linux-cpu|modal]]` - scaffold a new
//! project. Idempotent; never overwrites.
//!
//! Plain `init` writes an `amicu.hcl` with only a comment header. `--example`
//! writes a working starter instead. For the local examples its hardware
//! profile is the detected host and the model is the largest of a small Qwen3
//! ladder that fits that memory; `modal` mirrors the Modal docs example. The
//! same starters live under `examples/` in the repo, generated from
//! [`starter_hcl`] on a stated reference machine (see the tests).
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::Path;

use amicu_hcl::{format_graph, parse_graph};
use clap::ValueEnum;

use crate::error::CliError;
use crate::host::HostInfo;
use crate::project::Project;

/// Which example `init --example` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InitTarget {
    /// Apple Silicon Mac: llama.cpp with Metal offload. On an Intel Mac there
    /// is no GPU to offload to, so it writes a `local_cpu` profile with
    /// `n_gpu_layers = 0` instead.
    Mac,
    /// Linux with an NVIDIA GPU: llama.cpp with all layers on the GPU.
    LinuxCuda,
    /// Linux without a GPU: llama.cpp on the CPU.
    LinuxCpu,
    /// Modal serverless GPU (vLLM). Needs no local hardware.
    Modal,
}

impl InitTarget {
    fn label(self) -> &'static str {
        match self {
            InitTarget::Mac => "mac",
            InitTarget::LinuxCuda => "linux-cuda",
            InitTarget::LinuxCpu => "linux-cpu",
            InitTarget::Modal => "modal",
        }
    }

    /// The target for the detected host: macOS is `mac`; Linux with an NVIDIA
    /// GPU is `linux-cuda`; any other Linux is `linux-cpu`.
    pub(crate) fn detect(host: &HostInfo) -> Self {
        if host.os == "macos" {
            InitTarget::Mac
        } else if host.nvidia.is_some() {
            InitTarget::LinuxCuda
        } else {
            InitTarget::LinuxCpu
        }
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

/// One rung of the starter model ladder (all Qwen3 GGUF from Hugging Face).
pub(crate) struct Tier {
    pub id: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub quantization: &'static str,
    /// Exact file size, as served by Hugging Face.
    pub size_bytes: u64,
    /// Smallest memory (GiB, of the pool the model loads into) it is picked at.
    pub min_gib: u64,
}

/// Largest first.
pub(crate) const TIERS: [Tier; 4] = [
    Tier {
        id: "qwen3_8b",
        repo: "Qwen/Qwen3-8B-GGUF",
        file: "Qwen3-8B-Q4_K_M.gguf",
        quantization: "q4_k_m",
        size_bytes: 5_027_783_488,
        min_gib: 16,
    },
    Tier {
        id: "qwen3_4b",
        repo: "Qwen/Qwen3-4B-GGUF",
        file: "Qwen3-4B-Q4_K_M.gguf",
        quantization: "q4_k_m",
        size_bytes: 2_497_280_256,
        min_gib: 8,
    },
    Tier {
        id: "qwen3_1_7b",
        repo: "Qwen/Qwen3-1.7B-GGUF",
        file: "Qwen3-1.7B-Q8_0.gguf",
        quantization: "q8_0",
        size_bytes: 1_834_426_016,
        min_gib: 6,
    },
    Tier {
        id: "qwen3_0_6b",
        repo: "Qwen/Qwen3-0.6B-GGUF",
        file: "Qwen3-0.6B-Q8_0.gguf",
        quantization: "q8_0",
        size_bytes: 639_446_688,
        min_gib: 0,
    },
];

/// Whether `memory_bytes` reaches `gib` GiB, allowing 5% slack: a "16 GB" GPU
/// reports 16376 MiB, and Linux `MemTotal` is below installed RAM.
pub(crate) fn reaches_gib(memory_bytes: u64, gib: u64) -> bool {
    u128::from(memory_bytes) * 100 >= u128::from(gib) * u128::from(GIB) * 95
}

/// The largest tier whose `min_gib` is within `memory_bytes` (with 5% slack). On CPU the 8B
/// tier is skipped: generation is slow enough there that a small model is the
/// better first experience.
pub(crate) fn pick_tier(memory_bytes: u64, cpu_only: bool) -> &'static Tier {
    TIERS
        .iter()
        .skip(usize::from(cpu_only))
        .find(|t| reaches_gib(memory_bytes, t.min_gib))
        .unwrap_or(&TIERS[TIERS.len() - 1])
}

/// Runtime context for a tier on `memory_bytes`: modest by default (the KV
/// cache grows with it), more on big machines.
pub(crate) fn context_for(tier: &Tier, memory_bytes: u64) -> u32 {
    match tier.id {
        "qwen3_8b" | "qwen3_4b" if reaches_gib(memory_bytes, 32) => 32768,
        "qwen3_8b" | "qwen3_4b" => 8192,
        "qwen3_1_7b" => 4096,
        _ => 2048,
    }
}

fn model_block(tier: &Tier) -> String {
    format!(
        r#"resource "model" "{id}" {{
  source {{
    registry  = "hugging_face"
    reference = "{repo}"
  }}
  architecture   = "qwen3"
  modalities     = ["text"]
  artifact {{
    format       = "gguf"
    quantization = "{q}"
  }}
  context_length = 32768
  variant {{
    quantization = "{q}"
    file         = "{file}"
    size         = "{size}"
  }}
}}
"#,
        id = tier.id,
        repo = tier.repo,
        q = tier.quantization,
        file = tier.file,
        size = tier.size_bytes,
    )
}

/// A CPU model name as a slug (`AMD Ryzen 9 7950X` -> `amd_ryzen_9_7950x`).
fn cpu_slug(name: &str) -> String {
    let s = crate::host::slug(name);
    if s == "gpu" { "cpu".to_owned() } else { s }
}

/// The starter HCL for `target` on `host`. Pure: no I/O.
///
/// # Errors
/// Fails when a local target needs a host and none was detected.
pub(crate) fn starter_hcl(target: InitTarget, host: Option<&HostInfo>) -> Result<String, CliError> {
    if target == InitTarget::Modal {
        return Ok(MODAL_HCL.to_owned());
    }
    let host = host.ok_or_else(|| {
        CliError::Message(format!(
            "could not detect this machine, so the `{}` starter cannot match it; \
             use `--example modal`, or write the hardware profile by hand",
            target.label()
        ))
    })?;
    let mem = host.profile.memory;
    let mem_str = mem.to_string();
    let mut hcl = String::from(
        "# Amicu starter configuration.\n# Edit these resources, then run `amicu validate` and `amicu plan`.\n\n",
    );
    match target {
        InitTarget::Mac => {
            let tier = pick_tier(mem.bytes(), host.profile.accelerator.is_none());
            hcl.push_str(&model_block(tier));
            match &host.profile.accelerator {
                Some(acc) => {
                    let _ = write!(
                        hcl,
                        "\nresource \"hardware_profile\" \"local_dev\" {{\n  provider = \"local_apple\"\n  accelerator {{\n    kind   = \"apple_silicon\"\n    model  = \"{}\"\n    count  = 1\n    memory = \"{}\"\n  }}\n  memory = \"{mem_str}\"\n}}\n",
                        acc.model, acc.memory
                    );
                }
                None => hcl.push_str(&cpu_profile(host, &mem_str)),
            }
            hcl.push_str(&deployment_block(
                tier,
                "local_dev",
                context_for(tier, mem.bytes()),
                host.profile.accelerator.is_some(),
            ));
        }
        InitTarget::LinuxCuda => {
            let gpu = host.nvidia.as_ref().ok_or_else(|| {
                CliError::Message(
                    "no NVIDIA GPU found (`nvidia-smi` is missing or lists no card); \
                     use `--example linux-cpu`"
                        .into(),
                )
            })?;
            let tier = pick_tier(gpu.memory.bytes(), false);
            hcl.push_str(&model_block(tier));
            let _ = write!(
                hcl,
                "\nresource \"hardware_profile\" \"local_gpu\" {{\n  provider = \"on_prem\"\n  accelerator {{\n    kind   = \"gpu\"\n    model  = \"{}\"\n    count  = 1\n    memory = \"{}\"\n  }}\n  memory = \"{mem_str}\"\n}}\n",
                gpu.model, gpu.memory
            );
            hcl.push_str(&deployment_block(
                tier,
                "local_gpu",
                context_for(tier, gpu.memory.bytes()),
                true,
            ));
        }
        InitTarget::LinuxCpu => {
            let tier = pick_tier(mem.bytes(), true);
            hcl.push_str(&model_block(tier));
            hcl.push_str(&cpu_profile(host, &mem_str));
            hcl.push_str(&deployment_block(
                tier,
                "local_dev",
                context_for(tier, mem.bytes()),
                false,
            ));
        }
        InitTarget::Modal => unreachable!("returned above"),
    }
    Ok(hcl)
}

fn cpu_profile(host: &HostInfo, mem_str: &str) -> String {
    let (model, cores) = host
        .profile
        .cpu
        .as_ref()
        .map_or_else(|| ("cpu".to_owned(), 1), |c| (cpu_slug(&c.model), c.cores));
    format!(
        "\nresource \"hardware_profile\" \"local_dev\" {{\n  provider = \"local_cpu\"\n  cpu {{\n    model = \"{model}\"\n    cores = {cores}\n  }}\n  memory = \"{mem_str}\"\n}}\n"
    )
}

fn deployment_block(tier: &Tier, hw_id: &str, context: u32, gpu: bool) -> String {
    format!(
        r#"
resource "deployment" "{id}_local" {{
  model    = model.{id}.id
  hardware = hardware_profile.{hw_id}.id
  runtime  = "llama_cpp"
  llama_cpp {{
    context_length = {context}
    port           = 8000
    n_gpu_layers   = {layers}
  }}
}}
"#,
        id = tier.id,
        layers = if gpu { -1 } else { 0 },
    )
}

/// The `modal` starter: the Modal docs example (vLLM on one L4). Needs the
/// four Modal tokens and the deployment's own `CHAT_API_KEY`.
const MODAL_HCL: &str = r#"# Amicu starter configuration for Modal.
# Export MODAL_TOKEN_ID, MODAL_TOKEN_SECRET, MODAL_PROXY_TOKEN_ID,
# MODAL_PROXY_TOKEN_SECRET and CHAT_API_KEY, then run `amicu validate` and
# `amicu plan`.

resource "model" "qwen" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen3-8B"
  }
  architecture   = "qwen3"
  modalities     = ["text"]
  artifact {
    format = "safetensors"
  }
  context_length = 32768
}

resource "hardware_profile" "gpu_l4" {
  provider = "modal"
  region   = "eu-west"
  accelerator {
    kind   = "gpu"
    model  = "l4"
    count  = 1
    memory = "24GiB"
  }
  memory = "32GiB"
}

resource "deployment" "chat" {
  model    = model.qwen.id
  hardware = hardware_profile.gpu_l4.id
  runtime  = "vllm"
  api_key  = secret("CHAT_API_KEY")
  vllm {
    context_length = 8192
    port           = 8000
  }
}
"#;

/// What plain `init` writes: no resources, just where to go next.
const EMPTY_HCL: &str = "# Amicu configuration.
#
# Declare models, hardware profiles and deployments here.
# To start from a working example sized for this machine, run
#   amicu init --example
# (or pick one: amicu init --example mac|linux-cuda|linux-cpu|modal).
# That replaces this file while it is still unchanged.
#
# See what fits this machine: amicu catalog fits
# Docs: https://amicu.dev/docs
";

const VARIABLES_HCL: &str = "# Amicu variables.
# Reusable values will be declarable here in a future release.
";

const CONFIG_YAML: &str = "# Amicu project configuration.
version: 1
state:
  backend: local
";

/// Scaffold the project files, warning on (and skipping) any that exist.
///
/// `example` is `None` for plain `init` (header-only config), `Some(None)` for
/// a bare `--example` (auto-detect from `host`), or `Some(Some(target))`.
/// `host` is `None` when detection failed or was not needed.
#[allow(
    clippy::option_option,
    reason = "absent, bare --example, --example <target>"
)]
pub(crate) fn run(
    project: &Project,
    example: Option<Option<InitTarget>>,
    host: Option<&HostInfo>,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let io = |r: std::io::Result<()>| r.map_err(|e| CliError::io("write failed", e));
    let target = match (example, host) {
        (None, _) => None,
        (Some(Some(t)), _) => Some(t),
        (Some(None), Some(h)) => Some(InitTarget::detect(h)),
        (Some(None), None) => {
            return Err(CliError::Message(
                "could not detect this machine; pass `--example mac|linux-cuda|linux-cpu|modal`"
                    .into(),
            ));
        }
    };
    if let Some(t) = target {
        check_host_matches(t, host)?;
    }
    let root = project.root();
    let mut created = 0u32;

    // Emit the starter already in canonical form so `fmt --check` is clean.
    let amicu_hcl = match target {
        Some(t) => canonical(&starter_hcl(t, host)?)?,
        None => EMPTY_HCL.to_owned(),
    };
    let hcl_path = root.join("amicu.hcl");
    // A file `init` left untouched is safe to upgrade to an example; anything
    // the user wrote is never overwritten.
    if target.is_some() && fs::read_to_string(&hcl_path).is_ok_and(|t| t == EMPTY_HCL) {
        fs::remove_file(&hcl_path)
            .map_err(|e| CliError::io(format!("cannot replace {}", hcl_path.display()), e))?;
    }
    let hcl_existed = hcl_path.exists();
    created += write_if_absent(out, &hcl_path, &amicu_hcl)?;
    created += write_if_absent(out, &root.join("variables.amicu.hcl"), VARIABLES_HCL)?;

    ensure_dir(out, &project.dot_dir(), &mut created)?;
    created += write_if_absent(out, &project.dot_dir().join("config.yaml"), CONFIG_YAML)?;
    ensure_dir(out, &project.state_dir(), &mut created)?;
    created += write_if_absent(out, &project.state_dir().join(".gitkeep"), "")?;

    created += ensure_ignore_entry(out, &root.join(".gitignore"))?;

    if created == 0 {
        io(writeln!(out, "Project already initialized; nothing to do."))?;
    } else {
        io(writeln!(
            out,
            "Initialized Amicu project ({created} item(s) created)."
        ))?;
    }
    if !hcl_existed && let Some(target) = target {
        io(writeln!(out, "Example: {}.", target.label()))?;
        if target == InitTarget::Modal {
            io(writeln!(
                out,
                "Export MODAL_TOKEN_ID, MODAL_TOKEN_SECRET, MODAL_PROXY_TOKEN_ID, \
                 MODAL_PROXY_TOKEN_SECRET and CHAT_API_KEY before `amicu plan`."
            ))?;
        }
        if let Some(h) = host.filter(|_| target != InitTarget::Modal) {
            io(writeln!(out, "Detected: {}.", h.describe()))?;
        }
    }
    Ok(0)
}

/// A local target must match the machine it is written on: its profile is the
/// detected host. `--example mac` on Linux (or the reverse) would write a
/// profile of hardware that is not there.
fn check_host_matches(target: InitTarget, host: Option<&HostInfo>) -> Result<(), CliError> {
    let Some(host) = host else { return Ok(()) };
    let mismatch = |wanted: &str| {
        Err(CliError::Message(format!(
            "`--example {}` needs {wanted}, but this machine is {}; \
             pick another target (the default detects one)",
            target.label(),
            host.os
        )))
    };
    match target {
        InitTarget::Mac if host.os != "macos" => mismatch("a Mac"),
        InitTarget::LinuxCuda | InitTarget::LinuxCpu if host.os != "linux" => {
            mismatch("a Linux machine")
        }
        _ => Ok(()),
    }
}

/// The starter for `target` on `host`, in canonical form: exactly what
/// `init --example` writes. Also used to generate `examples/`.
#[cfg(test)]
pub(crate) fn canonical_starter(
    target: InitTarget,
    host: Option<&HostInfo>,
) -> Result<String, CliError> {
    canonical(&starter_hcl(target, host)?)
}

/// The starter config, rendered through the same formatter as `fmt`.
fn canonical(hcl: &str) -> Result<String, CliError> {
    let path = Path::new("amicu.hcl");
    let to_err = |source| CliError::Hcl {
        file: path.to_path_buf(),
        source,
    };
    let graph = parse_graph(hcl).map_err(to_err)?;
    format_graph(&graph).map_err(to_err)
}

fn write_if_absent(out: &mut dyn Write, path: &Path, contents: &str) -> Result<u32, CliError> {
    if path.exists() {
        writeln!(out, "  skip  {} (exists)", path.display())
            .map_err(|e| CliError::io("write failed", e))?;
        return Ok(0);
    }
    fs::write(path, contents)
        .map_err(|e| CliError::io(format!("cannot write {}", path.display()), e))?;
    writeln!(out, "  create {}", path.display()).map_err(|e| CliError::io("write failed", e))?;
    Ok(1)
}

fn ensure_dir(out: &mut dyn Write, path: &Path, created: &mut u32) -> Result<(), CliError> {
    if path.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(path)
        .map_err(|e| CliError::io(format!("cannot create {}", path.display()), e))?;
    writeln!(out, "  create {}/", path.display()).map_err(|e| CliError::io("write failed", e))?;
    *created += 1;
    Ok(())
}

/// Make sure the ignore file covers `.amicu/`: local state is per-machine
/// and must not be shared via version control. Appends
/// when the file exists without the entry; creates it otherwise; idempotent.
fn ensure_ignore_entry(out: &mut dyn Write, path: &Path) -> Result<u32, CliError> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(CliError::io(format!("cannot read {}", path.display()), e)),
    };
    let Some(mut text) = existing else {
        fs::write(path, ".amicu/\n")
            .map_err(|e| CliError::io(format!("cannot write {}", path.display()), e))?;
        writeln!(out, "  create {}", path.display())
            .map_err(|e| CliError::io("write failed", e))?;
        return Ok(1);
    };
    let covered = text.lines().map(str::trim).any(|l| {
        matches!(
            l,
            ".amicu" | ".amicu/" | "/.amicu" | "/.amicu/" | ".amicu/*"
        )
    });
    if covered {
        return Ok(0);
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(".amicu/\n");
    fs::write(path, text)
        .map_err(|e| CliError::io(format!("cannot write {}", path.display()), e))?;
    writeln!(out, "  update {} (added .amicu/)", path.display())
        .map_err(|e| CliError::io("write failed", e))?;
    Ok(1)
}
