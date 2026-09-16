//! Thin binary wrapper: parse config from flags/env and run the server until a
//! shutdown signal, or - given the `migrate` subcommand - apply pending
//! schema migrations (as the schema-owning role) and exit. All logic lives in
//! the library so it is testable over a real socket (see `tests/smoke.rs`)
//! or, for `migrate`, over an explicit `DATABASE_URL` (see
//! `tests/rls_guard_live.rs`).

use std::process::ExitCode;

use amicu_server::Config;
use clap::{Parser, Subcommand};

/// Top-level CLI: the default (no subcommand) runs the server from `config`;
/// `migrate` instead applies pending schema migrations and exits. See the
/// `amicu_server` crate docs' "Two-role production model" section.
#[derive(Parser)]
#[command(name = "amicu-server", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    config: Config,
}

/// Sub-commands. `migrate` is the only one today.
#[derive(Subcommand)]
enum Command {
    /// Apply pending schema migrations, connecting as the schema-OWNING role
    /// via `DATABASE_URL`, then exit. Runs no RLS-bypass guard - the migrator
    /// is expected to own the schema (see `ops/sql/provision_app_role.sql`).
    Migrate,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Migrate) => amicu_server::migrate().await,
        None => amicu_server::run(cli.config).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("amicu-server: {e}");
            ExitCode::FAILURE
        }
    }
}
