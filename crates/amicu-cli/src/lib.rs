//! Amicu CLI: the six core local-workflow commands.
//!
//! Command logic lives here (not in `main.rs`) so it is unit-testable: each
//! command writes to an injected `Write` sink and returns an exit code.

mod catalog;
mod checks;
mod commands;
mod error;
mod lock;
mod planner;
mod project;
mod refresh;
mod secret;
mod skills;

use std::io::Write;
use std::path::PathBuf;
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
        /// Wait up to this many seconds for the state lock instead of failing
        /// immediately when another amicu process holds it.
        #[arg(long, value_name = "SECS")]
        lock_timeout: Option<u64>,
    },
    /// Run environment and project diagnostics.
    Doctor,
    /// Export coding-agent skills. Does not require a project.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Modal (modal.com) control-plane commands. Does not require a project.
    Modal {
        #[command(subcommand)]
        command: ModalCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ModalCommand {
    /// Authenticate against Modal's control plane (the `ClientHello` RPC).
    ///
    /// Reads `MODAL_TOKEN_ID` and `MODAL_TOKEN_SECRET` from the
    /// environment; never accepts them as flags, so the secret cannot
    /// appear in shell history or `ps`.
    AuthCheck,
    /// Show a deployed app's state and its Functions' live stats.
    Status {
        /// The app's deployment name.
        app_name: String,
    },
    /// Print an app's logs.
    Logs {
        /// The app's deployment name.
        app_name: String,
        /// Keep tailing after the currently available logs are printed.
        #[arg(long)]
        follow: bool,
        /// Resume just past this log entry id instead of from the start.
        #[arg(long)]
        since: Option<String>,
    },
    /// Show a deployed app's runtime metrics (vLLM or llama.cpp `/metrics`).
    ///
    /// Scrapes through the app's Modal URL; set `MODAL_PROXY_TOKEN_ID` and
    /// `MODAL_PROXY_TOKEN_SECRET` for proxy auth. A scaled-to-zero app is
    /// woken by the request, so the first call can be slow.
    Metrics {
        /// The app's deployment name.
        app_name: String,
        /// Keep scraping and print a new snapshot every `--interval`
        /// seconds, with throughput derived from successive scrapes.
        #[arg(long)]
        watch: bool,
        /// Seconds between snapshots under `--watch`.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,
    },
    /// Stop a deployed app, terminating its containers.
    Stop {
        /// The app's deployment name.
        app_name: String,
    },
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    /// Render the canonical agent skills into a target-specific layout.
    Export {
        /// Coding agent to render skills for.
        #[arg(long, value_enum)]
        target: SkillTarget,
        /// Output directory (overrides the target default; required for generic).
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

/// A coding agent whose skill layout `skill export` renders to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SkillTarget {
    /// Claude Code: `.claude/skills/<skill>/SKILL.md` + `references/`.
    ClaudeCode,
    /// Cursor: `.cursor/rules/<skill>/`.
    Cursor,
    /// Windsurf: `.windsurf/rules/<skill>/`.
    Windsurf,
    /// Plain markdown under `--path` (default `./agent-guidance/`).
    Generic,
}

impl SkillTarget {
    fn default_root(self) -> &'static str {
        match self {
            SkillTarget::ClaudeCode => ".claude/skills",
            SkillTarget::Cursor => ".cursor/rules",
            SkillTarget::Windsurf => ".windsurf/rules",
            SkillTarget::Generic => "agent-guidance",
        }
    }

    fn label(self) -> &'static str {
        match self {
            SkillTarget::ClaudeCode => "claude-code",
            SkillTarget::Cursor => "cursor",
            SkillTarget::Windsurf => "windsurf",
            SkillTarget::Generic => "generic",
        }
    }
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
    // `skill export` and `modal auth-check` are project-independent; route
    // them before discovery.
    if let Command::Skill { command } = &cli.command {
        let SkillCommand::Export { target, path } = command;
        return commands::skill::export(*target, path.as_deref(), out);
    }
    if let Command::Modal { command } = &cli.command {
        return match command {
            ModalCommand::AuthCheck => commands::modal::auth_check(out),
            ModalCommand::Status { app_name } => commands::modal::status(app_name, out),
            ModalCommand::Logs {
                app_name,
                follow,
                since,
            } => commands::modal::logs(app_name, *follow, since.clone(), out),
            ModalCommand::Metrics {
                app_name,
                watch,
                interval,
            } => commands::modal::metrics(app_name, *watch, *interval, out),
            ModalCommand::Stop { app_name } => commands::modal::stop(app_name, out),
        };
    }

    let project = Project::discover()?;
    match &cli.command {
        Command::Init => commands::init::run(&project, out),
        Command::Fmt { check } => commands::fmt::run(&project, *check, out),
        Command::Validate { fix } => commands::validate::run(&project, *fix, cli.output, out),
        Command::Plan => commands::plan::run(&project, cli.output, out),
        Command::Apply {
            auto_approve,
            lock_timeout,
        } => commands::apply::run(
            &project,
            *auto_approve,
            lock_timeout.map(std::time::Duration::from_secs),
            cli.output,
            out,
        ),
        Command::Doctor => commands::doctor::run(&project, cli.output, out),
        Command::Skill { .. } | Command::Modal { .. } => {
            unreachable!("handled before project discovery")
        }
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
