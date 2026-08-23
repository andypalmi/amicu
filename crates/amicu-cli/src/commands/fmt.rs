//! `amicu fmt [--check]` - canonically reformat the project's HCL.

use std::fs;
use std::io::Write;
use std::path::Path;

use amicu_hcl::{format_graph, parse_graph};

use crate::error::CliError;
use crate::project::{Project, is_empty_graph};

/// Format every config file; `--check` reports drift without writing.
pub(crate) fn run(project: &Project, check: bool, out: &mut dyn Write) -> Result<u8, CliError> {
    let files = project.config_files()?;
    if files.is_empty() {
        return Err(CliError::NoConfig(project.root().to_path_buf()));
    }

    let mut changed = Vec::new();
    for path in &files {
        if reformat_file(path, check, out)? {
            changed.push(path.clone());
        }
    }

    if check {
        if changed.is_empty() {
            writeln!(out, "All files are correctly formatted.")
                .map_err(|e| CliError::io("write failed", e))?;
            Ok(0)
        } else {
            writeln!(out, "{} file(s) would be reformatted.", changed.len())
                .map_err(|e| CliError::io("write failed", e))?;
            Ok(1)
        }
    } else {
        if changed.is_empty() {
            writeln!(out, "All files already formatted.")
                .map_err(|e| CliError::io("write failed", e))?;
        } else {
            writeln!(out, "Formatted {} file(s).", changed.len())
                .map_err(|e| CliError::io("write failed", e))?;
        }
        Ok(0)
    }
}

/// Returns whether `path` is (or would be) changed by formatting.
fn reformat_file(path: &Path, check: bool, out: &mut dyn Write) -> Result<bool, CliError> {
    let original = fs::read_to_string(path)
        .map_err(|e| CliError::io(format!("cannot read {}", path.display()), e))?;
    let graph = parse_graph(&original).map_err(|source| CliError::Hcl {
        file: path.to_path_buf(),
        source,
    })?;

    // Comments-only / variables files carry no resources; formatting would
    // erase them, so leave them untouched.
    if is_empty_graph(&graph) {
        return Ok(false);
    }

    let formatted = format_graph(&graph).map_err(|source| CliError::Hcl {
        file: path.to_path_buf(),
        source,
    })?;
    if formatted == original {
        return Ok(false);
    }

    let verb = if check {
        "would reformat"
    } else {
        "reformatted"
    };
    writeln!(out, "  {verb} {}", path.display()).map_err(|e| CliError::io("write failed", e))?;
    if !check {
        fs::write(path, formatted)
            .map_err(|e| CliError::io(format!("cannot write {}", path.display()), e))?;
    }
    Ok(true)
}
