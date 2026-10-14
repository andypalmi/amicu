//! `amicu init` - scaffold a new project. Idempotent; never overwrites.

use std::fs;
use std::io::Write;
use std::path::Path;

use amicu_hcl::{format_graph, parse_graph};

use crate::error::CliError;
use crate::project::Project;

const AMICU_HCL: &str = r#"# Amicu starter configuration.
# Edit these resources, then run `amicu validate` and `amicu plan`.

resource "model" "qwen3_8b" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen3-8B-GGUF"
  }
  architecture   = "qwen3"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 100000
  variant {
    quantization = "q4_k_m"
    file         = "Qwen3-8B-Q4_K_M.gguf"
    size         = "5027783488"
  }
}

resource "hardware_profile" "local_dev" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m4_pro"
    count  = 1
    memory = "48GiB"
  }
  memory = "48GiB"
}

resource "deployment" "qwen3_8b_local" {
  model    = model.qwen3_8b.id
  hardware = hardware_profile.local_dev.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 100000
    port           = 8000
    n_gpu_layers   = -1
  }
}
"#;

const VARIABLES_HCL: &str = "# Amicu variables.
# Reusable values will be declarable here in a future release.
";

const CONFIG_YAML: &str = "# Amicu project configuration.
version: 1
state:
  backend: local
";

/// Scaffold the project files, warning on (and skipping) any that exist.
pub(crate) fn run(project: &Project, out: &mut dyn Write) -> Result<u8, CliError> {
    let root = project.root();
    let mut created = 0u32;

    // Emit the starter already in canonical form so `fmt --check` is clean.
    let amicu_hcl = canonical_starter()?;
    created += write_if_absent(out, &root.join("amicu.hcl"), &amicu_hcl)?;
    created += write_if_absent(out, &root.join("variables.amicu.hcl"), VARIABLES_HCL)?;

    ensure_dir(out, &project.dot_dir(), &mut created)?;
    created += write_if_absent(out, &project.dot_dir().join("config.yaml"), CONFIG_YAML)?;
    ensure_dir(out, &project.state_dir(), &mut created)?;
    created += write_if_absent(out, &project.state_dir().join(".gitkeep"), "")?;

    created += ensure_ignore_entry(out, &root.join(".gitignore"))?;

    if created == 0 {
        writeln!(out, "Project already initialized; nothing to do.")
            .map_err(|e| CliError::io("write failed", e))?;
    } else {
        writeln!(
            out,
            "Initialized Amicu project ({created} item(s) created)."
        )
        .map_err(|e| CliError::io("write failed", e))?;
    }
    Ok(0)
}

/// The starter config, rendered through the same formatter as `fmt`.
fn canonical_starter() -> Result<String, CliError> {
    let path = Path::new("amicu.hcl");
    let to_err = |source| CliError::Hcl {
        file: path.to_path_buf(),
        source,
    };
    let graph = parse_graph(AMICU_HCL).map_err(to_err)?;
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
