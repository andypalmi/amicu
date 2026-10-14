//! Thin binary wrapper: parse config from flags/env and run the server until a
//! shutdown signal, or - given a subcommand - do one job and exit: `migrate`
//! applies pending schema migrations (as the schema-owning role);
//! `gen-signing-key` prints a fresh Ed25519 PKCS#8 PEM; `outbox-drain` runs one
//! email-outbox drain pass. All logic lives in the library so it is testable
//! over a real socket (see `tests/smoke.rs`) or, for the database-backed
//! subcommands, over an explicit `DATABASE_URL` (see
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
    /// Generate a fresh Ed25519 PKCS#8 signing key and print ONLY its PEM to
    /// stdout, then exit. Nothing else is
    /// written to stdout, so the output is safe to pipe straight into a
    /// secret manager, e.g.:
    ///
    /// `amicu-server gen-signing-key | gcloud secrets versions add \
    /// AMICU_SIGNING_KEY_PEM --data-file=-`
    ///
    /// See `docs/ops/signing-key-rotation.md` for the full rotation
    /// procedure (generate, set as retiring, promote to active, drop the old
    /// one once it has aged out).
    GenSigningKey,
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
    init_logging();
    let result = match cli.command {
        Some(Command::Migrate) => amicu_server::migrate().await,
        Some(Command::GenSigningKey) => amicu_server::gen_signing_key(),
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

/// Install the `tracing` subscriber: one JSON object per line on stderr, which
/// Cloud Run / Cloud Logging ingest as structured entries. Carries the
/// `amicu::security` events; those hold only
/// pseudonymized identifiers, never plaintext emails, IPs, passwords or tokens.
fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
        .with_writer(std::io::stderr)
        .try_init();
}
