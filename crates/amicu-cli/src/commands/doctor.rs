//! `amicu doctor [--output json]` - environment and project diagnostics.

use std::fmt::Write as _;
use std::io::Write;

use amicu_engine::host::host_diagnostics;
use amicu_llamacpp::{BinaryConfig, CommandRunner, HostRunner, detect_binary};
use serde::Serialize;

use crate::OutputFormat;
use crate::error::CliError;
use crate::host::HostInfo;
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
    let host = HostInfo::detect();
    let disk_free = amicu_runtime::discovery::discover_report()
        .ok()
        .map(|r| r.disk_free.human());
    run_with(
        project,
        output,
        out,
        host.as_ref().map_err(String::as_str),
        disk_free,
    )
}

/// [`run`] with the detected host injected.
pub(crate) fn run_with(
    project: &Project,
    output: OutputFormat,
    out: &mut dyn Write,
    host: Result<&HostInfo, &str>,
    disk_free: Option<String>,
) -> Result<u8, CliError> {
    let mut checks = Vec::new();

    match host {
        Ok(host) => {
            let mut detail = host.describe();
            if let Some(free) = disk_free {
                let _ = write!(detail, " · {free} disk free");
            }
            checks.push(check("host", Status::Pass, detail));
        }
        Err(e) => checks.push(check("host", Status::Fail, e)),
    }

    checks.push(binary_check(&HostRunner));

    checks.push(project_config_check(project));
    if let Ok(host) = host {
        checks.extend(profile_check(project, host));
    }
    checks.push(state_dir_check(project));

    let report = DoctorReport::from_checks(checks);

    if output.is_json() {
        write_json(out, &report)?;
    } else {
        print_human(&report, out)?;
    }
    Ok(u8::from(!report.ok))
}

/// The `llama-server` check. An unknown build is a warning, never a failure.
fn binary_check(runner: &dyn CommandRunner) -> Check {
    match detect_binary(runner, &BinaryConfig::default()) {
        Ok(info) => check(
            "llama-server",
            Status::Pass,
            format!("{} at {}", info.version, info.path.display()),
        ),
        Err(amicu_llamacpp::LlamaCppError::UnparseableVersion(_)) => check(
            "llama-server",
            Status::Warn,
            "llama-server found, but its build number is unknown (a Homebrew or \
             source build without version metadata); cannot check it against the \
             minimum build",
        ),
        Err(e) => check("llama-server", Status::Warn, e.to_string()),
    }
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

/// Compare the project's local hardware profile with this machine
/// (`AMICU-MEM-003`). `None` when there is no config or no local deployment,
/// since then there is nothing to compare. A mismatch is a warning: the
/// project may be planned for another machine.
fn profile_check(project: &Project, host: &HostInfo) -> Option<Check> {
    let graph = project.load_graph().ok()?;
    let has_local = graph.deployments.iter().any(|d| {
        graph
            .hardware_profiles
            .iter()
            .find(|h| h.id == d.hardware_profile)
            .is_some_and(|h| amicu_core::resource::is_local_provider(&h.provider))
    });
    if !has_local {
        return None;
    }
    let diags = host_diagnostics(&graph, &host.facts());
    if diags.is_empty() {
        return Some(check(
            "profile",
            Status::Pass,
            "declared hardware does not exceed this machine",
        ));
    }
    let detail = diags
        .iter()
        .map(|d| format!("[{}] {}", d.code, d.message))
        .collect::<Vec<_>>()
        .join("; ");
    Some(check("profile", Status::Warn, detail))
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    struct Fake(String);

    impl CommandRunner for Fake {
        fn which(&self, _name: &str) -> Option<PathBuf> {
            Some(PathBuf::from("/opt/homebrew/bin/llama-server"))
        }
        fn version_output(&self, _path: &Path) -> Result<String, amicu_llamacpp::LlamaCppError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn unknown_build_is_a_warning_with_explanation() {
        let c = binary_check(&Fake("version: 0 (unknown)".to_owned()));
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("build number is unknown"), "{}", c.detail);
    }

    #[test]
    fn known_build_passes() {
        let c = binary_check(&Fake("version: 3901 (abc)".to_owned()));
        assert_eq!(c.status, Status::Pass);
    }
}
