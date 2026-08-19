//! Amicu CLI: the six core local-workflow commands.
//!
//! Command logic lives here (not in `main.rs`) so it is unit-testable: each
//! command writes to an injected `Write` sink and returns an exit code.

mod catalog;
mod checks;
mod commands;
mod error;
mod planner;
mod project;
mod secret;

use std::io::Write;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

pub use error::CliError;
use project::Project;

/// Machine- vs human-readable output selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum OutputFormat {
    /// A readable summary (default).
    #[default]
    Human,
    /// Stable JSON for automation.
    Json,
}

impl OutputFormat {
    fn is_json(self) -> bool {
        self == OutputFormat::Json
    }
}

/// Amicu: declarative local LLM serving.
#[derive(Debug, Parser)]
#[command(name = "amicu", version, about)]
struct Cli {
    /// Output format for automation-capable commands.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human, global = true)]
    output: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Scaffold a new project in the current directory.
    Init,
    /// Canonically format the project's HCL files.
    Fmt {
        /// Report whether formatting would change files without writing.
        #[arg(long)]
        check: bool,
    },
    /// Validate the configuration and report diagnostics.
    Validate {
        /// Apply safe, deterministic repairs (re-formatting).
        #[arg(long)]
        fix: bool,
    },
    /// Compute the plan of actions for the configuration.
    Plan,
    /// Apply the plan after review and approval.
    Apply {
        /// Skip the interactive approval prompt.
        #[arg(long, visible_alias = "yes")]
        auto_approve: bool,
    },
    /// Run environment and project diagnostics.
    Doctor,
}

/// Parse arguments, run the selected command, and map it to a process exit code.
#[must_use]
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let mut stdout = std::io::stdout();
    match dispatch(&cli, &mut stdout) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: &Cli, out: &mut dyn Write) -> Result<u8, CliError> {
    let project = Project::discover()?;
    match &cli.command {
        Command::Init => commands::init::run(&project, out),
        Command::Fmt { check } => commands::fmt::run(&project, *check, out),
        Command::Validate { fix } => commands::validate::run(&project, *fix, cli.output, out),
        Command::Plan => commands::plan::run(&project, cli.output, out),
        Command::Apply { auto_approve } => {
            commands::apply::run(&project, *auto_approve, cli.output, out)
        }
        Command::Doctor => commands::doctor::run(&project, cli.output, out),
    }
}

/// Write `value` as pretty JSON followed by a newline.
fn write_json<T: serde::Serialize + ?Sized>(
    out: &mut dyn Write,
    value: &T,
) -> Result<(), CliError> {
    serde_json::to_writer_pretty(&mut *out, value)?;
    writeln!(out).map_err(|e| CliError::io("write failed", e))
}

#[cfg(test)]
mod tests;
