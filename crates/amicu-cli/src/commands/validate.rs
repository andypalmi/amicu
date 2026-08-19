//! `amicu validate [--fix] [--output json]`.

use std::io::Write;

use amicu_core::diagnostic::{DiagnosticReport, Severity};

use crate::OutputFormat;
use crate::checks::report_for;
use crate::error::CliError;
use crate::project::Project;
use crate::write_json;

/// Validate the merged graph, optionally applying safe reformatting first.
pub(crate) fn run(
    project: &Project,
    fix: bool,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    if fix {
        // The only safe, deterministic repair is canonical re-formatting.
        let mut sink = Vec::new();
        crate::commands::fmt::run(project, false, &mut sink)?;
    }

    let graph = project.load_graph()?;
    let report = report_for(&graph);

    if output.is_json() {
        write_json(out, &report)?;
    } else {
        print_human(&report, out)?;
    }

    Ok(u8::from(!report.valid))
}

fn print_human(report: &DiagnosticReport, out: &mut dyn Write) -> Result<(), CliError> {
    let w = |out: &mut dyn Write, s: &str| {
        writeln!(out, "{s}").map_err(|e| CliError::io("write failed", e))
    };
    if report.diagnostics.is_empty() {
        w(out, "Configuration is valid. No diagnostics.")?;
        return Ok(());
    }
    for d in &report.diagnostics {
        let marker = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warn",
            Severity::Info => "info",
        };
        let resource = d.resource.as_deref().unwrap_or("-");
        writeln!(out, "  {marker:<5} [{}] {resource}: {}", d.code, d.message)
            .map_err(|e| CliError::io("write failed", e))?;
    }
    if report.valid {
        w(out, "Configuration is valid (warnings only).")?;
    } else {
        w(out, "Configuration is invalid.")?;
    }
    Ok(())
}
