//! `amicu doctor [--output json]` - environment and project diagnostics.

use std::io::Write;

use amicu_llamacpp::{BinaryConfig, HostRunner, detect_binary};
use amicu_runtime::discovery::discover_report;
use serde::Serialize;

use crate::OutputFormat;
use crate::error::CliError;
use crate::project::Project;
use crate::write_json;

/// A pass/warn/fail outcome for one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Pass,
    Warn,
    Fail,
}

/// One diagnostic check.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

/// The full doctor report.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DoctorReport {
    pub version: String,
    pub ok: bool,
    pub checks: Vec<Check>,
}

impl DoctorReport {
    const VERSION: &'static str = "v1";

    fn from_checks(checks: Vec<Check>) -> Self {
        let ok = !checks.iter().any(|c| c.status == Status::Fail);
        Self {
            version: Self::VERSION.to_owned(),
            ok,
            checks,
        }
    }
}

fn check(name: &str, status: Status, detail: impl Into<String>) -> Check {
    Check {
        name: name.to_owned(),
        status,
        detail: detail.into(),
    }
}

/// Gather host, binary, and project checks, then render them.
pub(crate) fn run(
    project: &Project,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let mut checks = Vec::new();

    match discover_report() {
        Ok(report) => checks.push(check(
            "host",
            Status::Pass,
            format!(
                "{} · {} memory · {} disk free",
                report.os, report.profile.memory, report.disk_free
            ),
        )),
        Err(e) => checks.push(check("host", Status::Fail, e.to_string())),
    }

    match detect_binary(&HostRunner, &BinaryConfig::default()) {
        Ok(info) => checks.push(check(
            "llama-server",
            Status::Pass,
            format!("{} at {}", info.version, info.path.display()),
        )),
        Err(e) => checks.push(check("llama-server", Status::Warn, e.to_string())),
    }

    checks.push(project_config_check(project));
    checks.push(state_dir_check(project));

    let report = DoctorReport::from_checks(checks);

    if output.is_json() {
        write_json(out, &report)?;
    } else {
        print_human(&report, out)?;
    }
    Ok(u8::from(!report.ok))
}

fn project_config_check(project: &Project) -> Check {
    match project.load_graph() {
        Ok(graph) => check(
            "config",
            Status::Pass,
            format!(
                "{} model(s), {} deployment(s) parsed",
                graph.models.len(),
                graph.deployments.len()
            ),
        ),
        Err(CliError::NoConfig(_)) => {
            check("config", Status::Warn, "no config found (run `amicu init`)")
        }
        Err(e) => check("config", Status::Fail, e.to_string()),
    }
}

fn state_dir_check(project: &Project) -> Check {
    let dir = project.state_dir();
    match std::fs::create_dir_all(&dir) {
        Ok(()) => check(
            "state",
            Status::Pass,
            format!("{} is writable", dir.display()),
        ),
        Err(e) => check("state", Status::Warn, format!("{}: {e}", dir.display())),
    }
}

fn print_human(report: &DoctorReport, out: &mut dyn Write) -> Result<(), CliError> {
    let io = |r: std::io::Result<()>| r.map_err(|e| CliError::io("write failed", e));
    io(writeln!(out, "Amicu environment check\n"))?;
    for c in &report.checks {
        let mark = match c.status {
            Status::Pass => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "fail",
        };
        io(writeln!(out, "  [{mark}] {:<13} {}", c.name, c.detail))?;
    }
    io(writeln!(
        out,
        "\n{}",
        if report.ok {
            "All required checks passed."
        } else {
            "One or more checks failed."
        }
    ))?;
    Ok(())
}
