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
//!
//! Zones are listed one by one, because the aggregated calls are lossy. The
//! API lists only what the project can see: zones gated to specific projects
//! show up as MISSING even when the catalog is right, so run it from a project
//! known to have access. Entries in AI zones (`europe-west4-ai1a`), which the
//! API may hide, are printed as HIDDEN and never affect the exit status.

use std::process::ExitCode;
use std::sync::Arc;

use amicu_gcp::catalog::drift::{self, Diff};
use amicu_gcp::{AdcTokenSource, AuthConfig, GcpClient};

fn print(title: &str, diff: &Diff) {
    println!(
        "\n== {title}: expected={} live={} missing={} hidden={} extra={}",
        diff.expected,
        diff.live,
        diff.missing.len(),
        diff.hidden.len(),
        diff.extra.len()
    );
    for (zone, name) in &diff.missing {
        println!("  MISSING (in catalog, not in API): {zone:<28} {name}");
    }
    for (zone, name) in &diff.hidden {
        println!("  HIDDEN  (AI zone, may be hidden from the API; not drift): {zone:<28} {name}");
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
    for zone in &live.not_visible {
        println!("zone not visible: {zone}");
    }
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
    println!(
        "\nNote: AI zones (europe-west4-ai1a) may be hidden from the API and are reported \
         as HIDDEN, not drift. Zones gated to specific projects show as MISSING."
    );
    if report.is_empty() {
        println!("\ncatalog matches the live API");
        ExitCode::SUCCESS
    } else {
        println!("\ndifferences found");
        ExitCode::from(1)
    }
}
