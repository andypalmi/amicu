//! Thin binary wrapper: parse config from flags/env and run the server until a
//! shutdown signal. All logic lives in the library so it is testable over a real
//! socket (see `tests/smoke.rs`).

use std::process::ExitCode;

use amicu_server::Config;
use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let config = Config::parse();
    match amicu_server::run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("amicu-server: {e}");
            ExitCode::FAILURE
        }
    }
}
