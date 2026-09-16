//! Thin binary wrapper: parse config from flags/env and run the server until a
//! shutdown signal, or - given a subcommand - do one job and exit: `migrate`
//! applies pending schema migrations (as the schema-owning role);
//! `outbox-drain` runs one email-outbox drain pass. All logic lives in the
//! library so it is testable over a real socket (see `tests/smoke.rs`) or, for
//! the subcommands, over an explicit `DATABASE_URL` (see
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

/// Sub-commands.
#[derive(Subcommand)]
enum Command {
    /// Apply pending schema migrations, connecting as the schema-OWNING role
    /// via `DATABASE_URL`, then exit. Runs no RLS-bypass guard - the migrator
    /// is expected to own the schema (see `ops/sql/provision_app_role.sql`).
    Migrate,
    /// Run one email-outbox drain pass against `DATABASE_URL` and exit.
    ///
    /// The same work `POST /internal/outbox/drain` does, through the same
    /// `amicu_api::drain_once` - for local development, and for a self-hoster
    /// who would rather run this from cron than call the endpoint. On Cloud Run
    /// the scheduled HTTP endpoint is the real path; this needs no caller
    /// authentication because it IS the operator, with the database credential
    /// in hand.
    OutboxDrain,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Migrate) => amicu_server::migrate().await,
        Some(Command::OutboxDrain) => amicu_server::outbox_drain(&cli.config).await,
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
