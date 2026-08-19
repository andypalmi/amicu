//! Thin binary wrapper: parse args and delegate to the library.

use std::process::ExitCode;

fn main() -> ExitCode {
    amicu_cli::run()
}
