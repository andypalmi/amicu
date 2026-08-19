//! `amicu plan [--output json]`.

use std::io::Write;

use amicu_core::diagnostic::{DiagnosticReport, Severity};

use crate::OutputFormat;
use crate::checks::report_for;
use crate::error::CliError;
use crate::planner::{PlanResult, build_plan};
use crate::project::Project;
use crate::write_json;

/// Build and render the plan; exits non-zero when validation errors block it.
pub(crate) fn run(
    project: &Project,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let graph = project.load_graph()?;

    // Surface structural errors (dangling refs, duplicate ids) through the same
    // diagnostic path as `validate`, before `build_plan` can hard-error on them.
    let report = report_for(&graph);
    if !report.valid {
        emit_report(&report, output, out)?;
        return Ok(1);
    }

    let prior = project.load_state()?.map(|s| s.graph);
    let result = build_plan(&graph, prior.as_ref())?;

    if result.has_errors() {
        // Cannot produce a reviewable plan for an invalid config.
        emit_errors(&result, output, out)?;
        return Ok(1);
    }

    if output.is_json() {
        write_json(out, &result.plan)?;
    } else {
        print_human(&result, out)?;
    }
    Ok(0)
}

fn emit_errors(
    result: &PlanResult,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    emit_report(
        &DiagnosticReport::new(result.diagnostics.clone()),
        output,
        out,
    )
}

fn emit_report(
    report: &DiagnosticReport,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    if output.is_json() {
        write_json(out, report)?;
    } else {
        writeln!(out, "Cannot plan: configuration has errors.")
            .map_err(|e| CliError::io("write failed", e))?;
        for d in &report.diagnostics {
            if d.severity == Severity::Error {
                writeln!(out, "  error [{}] {}", d.code, d.message)
                    .map_err(|e| CliError::io("write failed", e))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn print_human(result: &PlanResult, out: &mut dyn Write) -> Result<(), CliError> {
    let io = |r: std::io::Result<()>| r.map_err(|e| CliError::io("write failed", e));

    if result.items.is_empty() {
        io(writeln!(
            out,
            "No changes. Infrastructure matches the configuration."
        ))?;
    } else {
        io(writeln!(out, "Plan: {} action(s).\n", result.items.len()))?;
        for item in &result.items {
            let kind = format!("{:?}", item.action.kind).to_lowercase();
            io(writeln!(out, "  {} {}", symbol(&kind), item.action.address))?;
            io(writeln!(out, "      action:   {kind}"))?;
            if let Some(hw) = &item.action.hardware {
                io(writeln!(out, "      hardware: {hw}"))?;
            }
            if let Some(artifact) = &item.action.artifact {
                io(writeln!(out, "      artifact: {}", artifact.name))?;
            }
            if let Some(cmd) = &item.rendered {
                io(writeln!(out, "      launch:   {cmd}"))?;
            }
            if let Some(cp) = &item.action.cost_performance {
                for a in &cp.assumptions {
                    io(writeln!(out, "      note:     {a}"))?;
                }
            }
            io(writeln!(out))?;
        }
    }

    for d in &result.diagnostics {
        let marker = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warn",
            Severity::Info => "info",
        };
        io(writeln!(out, "  {marker}: {}", d.message))?;
    }
    Ok(())
}

fn symbol(kind: &str) -> &'static str {
    match kind {
        "create" => "+",
        "destroy" => "-",
        "replace" => "±",
        _ => "~",
    }
}
