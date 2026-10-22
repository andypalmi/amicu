//! Compares the GPU catalog with the Compute Engine API of a real project and
//! prints the zone and GPU (or machine type) pairs that are missing from the
//! API or absent from the catalog. Read-only.
//!
//! ```text
//! GCP_PROJECT=my-project cargo run -p amicu-gcp --example verify-catalog
//! ```
//!
//! Credentials are Application Default Credentials (`gcloud auth
//! application-default login`), with `GOOGLE_IMPERSONATE_SERVICE_ACCOUNT`
//! honoured. Exit status: 0 no drift, 1 drift, 2 could not run.

use std::process::ExitCode;
use std::sync::Arc;

use amicu_gcp::catalog::drift::{self, Diff};
use amicu_gcp::{AdcTokenSource, AuthConfig, GcpClient};

fn print(title: &str, diff: &Diff) {
    println!(
        "\n== {title}: expected={} live={} missing={} extra={}",
        diff.expected,
        diff.live,
        diff.missing.len(),
        diff.extra.len()
    );
    for (zone, name) in &diff.missing {
        println!("  MISSING (in catalog, not in API): {zone:<28} {name}");
    }
    for (zone, name) in &diff.extra {
        println!("  EXTRA   (in API, not in catalog): {zone:<28} {name}");
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let Ok(project) = std::env::var("GCP_PROJECT") else {
        eprintln!("set GCP_PROJECT to a project id");
        return ExitCode::from(2);
    };
    let live = async {
        let tokens = AdcTokenSource::new(&AuthConfig::default())?;
        let client = GcpClient::new(Arc::new(tokens));
        drift::fetch_live_state(&client, &project).await
    }
    .await;
    let live = match live {
        Ok(live) => live,
        Err(e) => {
            eprintln!("could not read the project: {e}");
            return ExitCode::from(2);
        }
    };
    let report = drift::compare(&live);
    print("zone x GPU (acceleratorTypes)", &report.zone_gpus);
    print(
        "zone x GPU (inferred from machineTypes)",
        &report.zone_gpus_from_machine_types,
    );
    print(
        "zone x machine type (machineTypes)",
        &report.zone_machine_types,
    );
    println!("\nNote: europe-west4-ai1a is an AI zone and may be hidden from the API.");
    if report.is_empty() {
        println!("\ncatalog matches the live API");
        ExitCode::SUCCESS
    } else {
        println!("\ndifferences found");
        ExitCode::from(1)
    }
}
