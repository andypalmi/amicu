//! `gcp {}` block checks: `AMICU-CAP-016` to `AMICU-CAP-019`. Catalog lookups
//! only; nothing here talks to Google.

use std::time::Duration;

use amicu_core::diagnostic::{Diagnostic, DiagnosticCode, Severity, catalog};
use amicu_core::graph::Graph;
use amicu_core::resource::{
    AcceleratorKind, GcpBackend, GcpProvisioning, GcpSettings, HardwareProfile, Runtime,
};
use amicu_gcp::{CatalogProvisioning, GcpCatalog, PlacementError, candidate_zones, resolve_shape};

const GCP_PROVIDER: &str = "gcp";
const MIN_RUN: Duration = Duration::from_mins(10);
const MIN_IDLE: Duration = Duration::from_mins(5);
const MAX_RUN: Duration = Duration::from_hours(120 * 24);
const MAX_FLEX_RUN: Duration = Duration::from_hours(7 * 24);

/// The catalog's name for a profile's provisioning mode.
#[must_use]
pub fn catalog_provisioning(p: GcpProvisioning) -> CatalogProvisioning {
    match p {
        GcpProvisioning::Spot => CatalogProvisioning::Spot,
        GcpProvisioning::FlexStart => CatalogProvisioning::FlexStart,
        GcpProvisioning::Standard => CatalogProvisioning::OnDemand,
    }
}

fn cidr_ok(c: &str) -> bool {
    let Some((ip, bits)) = c.split_once('/') else {
        return false;
    };
    ip.parse::<std::net::Ipv4Addr>().is_ok() && bits.parse::<u8>().is_ok_and(|b| b <= 32)
}

fn is_ipv6_cidr(c: &str) -> bool {
    c.split_once('/')
        .is_some_and(|(ip, _)| ip.parse::<std::net::Ipv6Addr>().is_ok())
}

pub(crate) fn gcp_diagnostics(graph: &Graph) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for hw in &graph.hardware_profiles {
        let mut push = |code: DiagnosticCode, message: String| {
            let mut d = Diagnostic::new(code, Severity::Error, message);
            d.resource = Some(format!("hardware_profile.{}", hw.id));
            out.push(d);
        };
        if hw.provider != GCP_PROVIDER {
            if hw.gcp.is_some() {
                push(
                    catalog::CAP_GCP_SETTINGS_INVALID,
                    format!(
                        "hardware_profile `{}`: a `gcp {{}}` block needs `provider = \"gcp\"` \
                         (the provider is `{}`); remove the block",
                        hw.id, hw.provider
                    ),
                );
            }
            continue;
        }
        for (code, message) in profile_problems(graph, hw) {
            push(code, format!("hardware_profile `{}`: {message}", hw.id));
        }
    }
    out
}

fn profile_problems(graph: &Graph, hw: &HardwareProfile) -> Vec<(DiagnosticCode, String)> {
    let invalid = catalog::CAP_GCP_SETTINGS_INVALID;
    let mut out = Vec::new();
    let settings = hw.gcp.clone().unwrap_or_default();
    if settings.project.as_deref().is_none_or(str::is_empty) {
        out.push((
            invalid,
            "`gcp {}` needs a `project` (the Google Cloud project id)".to_owned(),
        ));
    } else if let Some(p) = &settings.project
        && amicu_gcp::validate_project_id(p).is_err()
    {
        out.push((invalid, format!("`project` `{p}` is not a project id")));
    }
    for d in graph
        .deployments
        .iter()
        .filter(|d| d.hardware_profile == hw.id)
    {
        if matches!(d.runtime, Runtime::LlamaCpp(_)) {
            out.push((
                invalid,
                format!(
                    "deployment `{}`: the Google Cloud backend runs vLLM only; use \
                     `runtime = \"vllm\"`",
                    d.id
                ),
            ));
        }
        let one = |v: Option<u32>| v.is_none_or(|v| v <= 1);
        if settings.backend == GcpBackend::Compute
            && let Some(s) = &d.scaling
            && !(one(s.min_replicas) && one(s.max_replicas))
        {
            out.push((
                invalid,
                format!(
                    "deployment `{}`: a Compute Engine VM runs exactly one replica, so \
                     `min_replicas` and `max_replicas` can be at most 1",
                    d.id
                ),
            ));
        }
    }
    if settings.backend == GcpBackend::CloudRun {
        out.extend(cloud_run_problems(graph, hw, &settings));
        return out;
    }
    let Some(acc) = hw
        .accelerator
        .as_ref()
        .filter(|a| a.kind == AcceleratorKind::Gpu)
    else {
        out.push((invalid, "a GPU `accelerator` is required".to_owned()));
        return out;
    };
    out.extend(compute_problems(hw, &settings, &acc.model, acc.count));
    if settings.access.is_some() {
        out.push((
            catalog::CAP_GCP_BACKEND_UNSUPPORTED,
            "`access` applies to `backend = \"cloud_run\"` only; a VM is limited by \
             `allowed_source_cidrs` instead"
                .to_owned(),
        ));
    }
    out
}

/// `network` and `subnetwork` of a VM: well-formed, paired, and the
/// subnetwork in the VM's region when its link says which region that is.
fn network_problems(s: &GcpSettings, region: Option<&str>) -> Vec<(DiagnosticCode, String)> {
    let invalid = catalog::CAP_GCP_SETTINGS_INVALID;
    let mut out = Vec::new();
    if let Some(n) = &s.network
        && let Err(e) = amicu_gcp::NetworkRef::parse(n)
    {
        out.push((invalid, format!("`network`: {e}")));
    }
    if let Some(sub) = &s.subnetwork {
        match amicu_gcp::SubnetworkRef::parse(sub) {
            Err(e) => out.push((invalid, format!("`subnetwork`: {e}"))),
            Ok(r) => {
                if let (Some(lr), Some(region)) = (&r.region, region)
                    && lr != region
                {
                    out.push((
                        invalid,
                        format!("`subnetwork` is in `{lr}` but the VM is placed in `{region}`"),
                    ));
                }
            }
        }
        if s.network.is_none() {
            out.push((
                invalid,
                "`subnetwork` needs `network` too: the firewall rule is created on the network"
                    .to_owned(),
            ));
        }
    }
    out
}

/// Most requests Cloud Run lets one instance serve at once, and the most
/// instances one service may scale to.
const CLOUD_RUN_MAX_CONCURRENCY: u32 = 1000;
const CLOUD_RUN_MAX_INSTANCES: u32 = 1000;

/// `scaling {}` of the deployments on a Cloud Run profile.
fn cloud_run_scaling_problems(
    graph: &Graph,
    hw: &HardwareProfile,
) -> Vec<(DiagnosticCode, String)> {
    let invalid = catalog::CAP_GCP_SETTINGS_INVALID;
    let unsupported = catalog::CAP_GCP_BACKEND_UNSUPPORTED;
    let mut out = Vec::new();
    for d in graph
        .deployments
        .iter()
        .filter(|d| d.hardware_profile == hw.id)
    {
        let Some(sc) = &d.scaling else { continue };
        if sc.min_replicas.is_some_and(|m| m > 0) {
            out.push((
                unsupported,
                format!(
                    "deployment `{}`: Cloud Run deployments scale to zero, so `min_replicas` \
                     must be 0 or unset",
                    d.id
                ),
            ));
        }
        if sc.scaledown_window_secs.is_some() {
            out.push((
                unsupported,
                format!(
                    "deployment `{}`: Cloud Run decides when an idle instance stops, so \
                     `scaledown_window` cannot be honoured; remove it",
                    d.id
                ),
            ));
        }
        if sc.max_replicas.is_some_and(|m| m > CLOUD_RUN_MAX_INSTANCES) {
            out.push((
                invalid,
                format!(
                    "deployment `{}`: `max_replicas` can be at most {CLOUD_RUN_MAX_INSTANCES} \
                     on Cloud Run",
                    d.id
                ),
            ));
        }
        if sc
            .target_concurrency
            .is_some_and(|c| c > CLOUD_RUN_MAX_CONCURRENCY)
        {
            out.push((
                invalid,
                format!(
                    "deployment `{}`: `target_concurrency` can be at most \
                     {CLOUD_RUN_MAX_CONCURRENCY} on Cloud Run",
                    d.id
                ),
            ));
        }
    }
    out
}

/// The profile's `memory` and `cpu` against the GPU's Cloud Run minimum.
fn cloud_run_size_problems(
    hw: &HardwareProfile,
    gpu: &amicu_gcp::catalog::CloudRunGpu,
) -> Vec<(DiagnosticCode, String)> {
    const GIB: u64 = 1 << 30;
    let mut out = Vec::new();
    if hw.memory.bytes() < u64::from(gpu.min_memory_gib) * GIB {
        out.push((
            catalog::CAP_GCP_SETTINGS_INVALID,
            format!(
                "`memory` is {}, below Cloud Run's minimum of {} GiB for {}; set at least \
                 \"{}GiB\"",
                hw.memory.human(),
                gpu.min_memory_gib,
                gpu.gpu,
                gpu.min_memory_gib
            ),
        ));
    }
    if let Some(cpu) = hw.cpu.as_ref().filter(|c| c.cores < gpu.min_vcpus) {
        out.push((
            catalog::CAP_GCP_SETTINGS_INVALID,
            format!(
                "`cpu.cores` is {}, below Cloud Run's minimum of {} for {}",
                cpu.cores, gpu.min_vcpus, gpu.gpu
            ),
        ));
    }
    out
}

fn cloud_run_problems(
    graph: &Graph,
    hw: &HardwareProfile,
    s: &GcpSettings,
) -> Vec<(DiagnosticCode, String)> {
    let invalid = catalog::CAP_GCP_SETTINGS_INVALID;
    let unsupported = catalog::CAP_GCP_BACKEND_UNSUPPORTED;
    let mut out = Vec::new();
    let compute_only = [
        ("zone", s.zone.is_some()),
        ("machine_type", s.machine_type.is_some()),
        ("provisioning", s.provisioning != GcpProvisioning::Spot),
        ("idle_shutdown", s.idle_shutdown_secs.is_some()),
        ("max_run_duration", s.max_run_duration_secs.is_some()),
        ("boot_disk_gb", s.boot_disk_gb.is_some()),
        ("allowed_source_cidrs", !s.allowed_source_cidrs.is_empty()),
        ("network", s.network.is_some()),
        ("subnetwork", s.subnetwork.is_some()),
        ("insecure_metadata_secrets", s.insecure_metadata_secrets),
    ];
    for (field, _) in compute_only.iter().filter(|(_, set)| *set) {
        out.push((
            unsupported,
            format!(
                "`{field}` applies to Compute Engine only; Cloud Run cannot honour it, so \
                 remove it (or use `backend = \"compute\"`)"
            ),
        ));
    }
    if s.service_account.as_deref().is_none_or(str::is_empty) {
        out.push((
            invalid,
            "`gcp {}` needs a `service_account`: the Cloud Run service runs as that account \
             and reads its API key and Hugging Face token from Secret Manager with it"
                .to_owned(),
        ));
    }
    out.extend(cloud_run_scaling_problems(graph, hw));
    let Some(acc) = hw
        .accelerator
        .as_ref()
        .filter(|a| a.kind == AcceleratorKind::Gpu)
    else {
        out.push((invalid, "a GPU `accelerator` is required".to_owned()));
        return out;
    };
    let cat = GcpCatalog::new();
    let Some(gpu) = amicu_gcp::resolve_model(&acc.model).and_then(|m| cat.cloud_run_gpu(m.name))
    else {
        out.push((
            catalog::CAP_GCP_GPU_UNAVAILABLE,
            format!(
                "`{}` is not offered on Cloud Run (only L4 and RTX PRO 6000 are); use \
                 `backend = \"compute\"`",
                acc.model
            ),
        ));
        return out;
    };
    out.extend(cloud_run_size_problems(hw, gpu));
    if acc.count != 1 {
        out.push((
            invalid,
            format!(
                "a Cloud Run instance has exactly one GPU, so `accelerator.count` must be 1 \
                 (got {})",
                acc.count
            ),
        ));
    }
    let Some(region) = hw.region.as_ref().map(|r| r.code.as_str()) else {
        out.push((
            invalid,
            format!(
                "set `region` to a region offering {} on Cloud Run ({})",
                gpu.gpu,
                cat.cloud_run_regions(gpu.gpu).join(", ")
            ),
        ));
        return out;
    };
    let offer = cat
        .cloud_run_offers()
        .iter()
        .find(|o| o.gpu == gpu.gpu && o.region == region);
    match offer {
        None => out.push((
            catalog::CAP_GCP_GPU_UNAVAILABLE,
            format!(
                "{} is not offered on Cloud Run in `{region}`; it is in {}",
                gpu.gpu,
                cat.cloud_run_regions(gpu.gpu).join(", ")
            ),
        )),
        Some(o) if o.invitation_only => out.push((
            catalog::CAP_GCP_GPU_UNAVAILABLE,
            format!(
                "{} on Cloud Run in `{region}` is by invitation only; contact the Google \
                 account team or pick another region",
                gpu.gpu
            ),
        )),
        Some(_) => {}
    }
    out
}

fn compute_problems(
    hw: &HardwareProfile,
    s: &GcpSettings,
    gpu: &str,
    count: u32,
) -> Vec<(DiagnosticCode, String)> {
    let invalid = catalog::CAP_GCP_SETTINGS_INVALID;
    let mut out = Vec::new();
    let region = hw.region.as_ref().map(|r| r.code.as_str());
    if region.is_none() && s.zone.is_none() {
        out.push((
            invalid,
            "set `region` (or `gcp { zone }`) so the VM is placed where you expect".to_owned(),
        ));
    }
    if let (Some(z), Some(r)) = (&s.zone, region)
        && amicu_gcp::zone_region(z).is_ok_and(|zr| zr != r)
    {
        out.push((invalid, format!("`zone` `{z}` is not in `region` `{r}`")));
    }
    match resolve_shape(
        gpu,
        count,
        s.machine_type.as_deref(),
        catalog_provisioning(s.provisioning),
    ) {
        Ok(shape) => {
            let fixed = GcpCatalog::new()
                .machine_type(&shape.machine_type)
                .is_some();
            let machine = fixed.then_some(shape.machine_type.as_str());
            if (region.is_some() || s.zone.is_some())
                && let Err(e) = candidate_zones(gpu, machine, region, s.zone.as_deref())
            {
                out.push((catalog::CAP_GCP_GPU_UNAVAILABLE, e.to_string()));
            }
        }
        Err(e) => out.push((placement_code(&e), e.to_string())),
    }
    let flex = s.provisioning == GcpProvisioning::FlexStart;
    let run = Duration::from_secs(u64::from(s.max_run_secs()));
    if run < MIN_RUN || run > if flex { MAX_FLEX_RUN } else { MAX_RUN } {
        let limit = if flex {
            "168h (7 days, the limit for `flex_start`)"
        } else {
            "2880h (120 days; 168h with `provisioning = \"flex_start\"`)"
        };
        out.push((
            invalid,
            format!(
                "`max_run_duration` must be between 10m and {limit}; write it in s, m or h \
                 (days are not accepted)"
            ),
        ));
    }
    if s.boot_disk_gb.is_some_and(|g| g < 50) {
        out.push((invalid, "`boot_disk_gb` must be at least 50".to_owned()));
    }
    if s.service_account.as_deref().is_none_or(str::is_empty) && !s.insecure_metadata_secrets {
        out.push((
            invalid,
            "`gcp {}` needs a `service_account`: the VM reads its API key, Hugging Face token \
             and TLS key from Secret Manager as that account. Create one (for example \
             `gcloud iam service-accounts create amicu-vm`) and set `service_account` to its \
             email, or set `insecure_metadata_secrets = true` to put the secrets in instance \
             metadata instead, where anyone who can read the instance can read them"
                .to_owned(),
        ));
    }
    out.extend(network_problems(
        s,
        region.or_else(|| {
            s.zone
                .as_deref()
                .and_then(|z| amicu_gcp::zone_region(z).ok())
        }),
    ));
    for c in s.allowed_source_cidrs.iter().filter(|c| !cidr_ok(c)) {
        let message = if is_ipv6_cidr(c) {
            format!(
                "`allowed_source_cidrs` entry `{c}` is IPv6; the firewall rule amicu creates \
                 is IPv4 only, so list IPv4 CIDRs like 203.0.113.4/32"
            )
        } else {
            format!("`allowed_source_cidrs` entry `{c}` is not a CIDR like 203.0.113.4/32")
        };
        out.push((invalid, message));
    }
    if let Some(idle) = s.idle_shutdown_secs {
        let idle = Duration::from_secs(u64::from(idle));
        if idle < MIN_IDLE || idle >= run {
            out.push((
                invalid,
                "`idle_shutdown` must be at least 5m and shorter than `max_run_duration`; \
                 write it in s, m or h"
                    .to_owned(),
            ));
        }
    }
    out
}

fn placement_code(e: &PlacementError) -> DiagnosticCode {
    match e {
        PlacementError::UnknownGpu(_) | PlacementError::NoZone { .. } => {
            catalog::CAP_GCP_GPU_UNAVAILABLE
        }
        PlacementError::ProvisioningUnsupported { .. } | PlacementError::NoMachine { .. } => {
            catalog::CAP_GCP_PROVISIONING_UNSUPPORTED
        }
        _ => catalog::CAP_GCP_SETTINGS_INVALID,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile with one vLLM deployment; `gpu` and `gcp` fill the blanks.
    fn graph(provider: &str, region: &str, gpu: &str, gcp: &str, extra: &str) -> Graph {
        // The secrets need a service account unless the test says otherwise.
        let sa = if gcp.contains("service_account") || gcp.contains("insecure_metadata_secrets") {
            ""
        } else {
            "\nservice_account = \"vm@my-proj.iam.gserviceaccount.com\""
        };
        let gcp_block = if gcp.is_empty() {
            String::new()
        } else {
            format!("gcp {{\n{gcp}{sa}\n}}")
        };
        let text = format!(
            r#"
resource "model" "m" {{
  source {{
    registry  = "hugging_face"
    reference = "Qwen/Qwen3-0.6B"
  }}
  architecture = "qwen3"
  modalities   = ["text"]
  artifact {{
    format       = "safetensors"
    quantization = "f16"
  }}
  context_length = 4096
  n_layers       = 28
  n_kv_heads     = 8
  head_dim       = 128
}}
resource "hardware_profile" "g" {{
  provider = "{provider}"
  {region}
  accelerator {{
    kind   = "gpu"
    model  = "{gpu}"
    count  = 1
    memory = "16GiB"
  }}
  memory = "32GiB"
  {gcp_block}
}}
resource "deployment" "d" {{
  model    = model.m.id
  hardware = hardware_profile.g.id
  runtime  = "vllm"
  vllm {{
    context_length = 4096
    port           = 8000
  }}
  api_key = secret("K")
  {extra}
}}
"#
        );
        amicu_hcl::parse_graph(&text).unwrap()
    }

    fn codes(g: &Graph) -> Vec<String> {
        gcp_diagnostics(g)
            .iter()
            .map(|d| d.code.to_string())
            .collect()
    }

    const EU: &str = r#"region = "europe-west4""#;

    #[test]
    fn a_valid_t4_spot_profile_has_no_diagnostics() {
        let g = graph(
            "gcp",
            EU,
            "t4",
            "project = \"my-proj\"\nmachine_type = \"n1-standard-4\"",
            "",
        );
        assert_eq!(codes(&g), Vec::<String>::new());
    }

    #[test]
    fn secrets_need_a_service_account_or_the_explicit_metadata_opt_in() {
        let base = "project = \"my-proj\"\nmachine_type = \"n1-standard-4\"\n";
        let g = graph(
            "gcp",
            EU,
            "t4",
            &format!("{base}zone = \"europe-west4-a\"\nservice_account = \"\""),
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-016"]);
        let d = &gcp_diagnostics(&g)[0];
        assert!(d.message.contains("service_account"), "{}", d.message);
        assert!(
            d.message.contains("insecure_metadata_secrets"),
            "{}",
            d.message
        );
        let g = graph(
            "gcp",
            EU,
            "t4",
            &format!("{base}insecure_metadata_secrets = true"),
            "",
        );
        assert_eq!(codes(&g), Vec::<String>::new());
        let g = graph(
            "gcp",
            EU,
            "t4",
            &format!("{base}service_account = \"vm@my-proj.iam.gserviceaccount.com\""),
            "",
        );
        assert_eq!(codes(&g), Vec::<String>::new());
    }

    #[test]
    fn missing_project_and_wrong_provider_are_reported() {
        let g = graph("gcp", EU, "t4", "zone = \"europe-west4-a\"", "");
        assert_eq!(codes(&g), ["AMICU-CAP-016"]);
        // No block at all: no project and no service account.
        let g = graph("gcp", EU, "t4", "", "");
        assert_eq!(codes(&g), ["AMICU-CAP-016", "AMICU-CAP-016"]);
        let g = graph("runpod", EU, "l4", "project = \"my-proj\"", "");
        assert_eq!(codes(&g), ["AMICU-CAP-016"]);
    }

    #[test]
    fn unknown_gpu_and_gpu_not_offered_in_the_zone_are_reported() {
        let g = graph("gcp", EU, "nope-9000", "project = \"my-proj\"", "");
        assert_eq!(codes(&g), ["AMICU-CAP-017"]);
        let g = graph(
            "gcp",
            r#"region = "europe-west10""#,
            "t4",
            "project = \"my-proj\"\nzone = \"europe-west10-b\"",
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-017"]);
    }

    #[test]
    fn max_run_limits_are_stated_in_hours_and_flex_start_is_seven_days() {
        let base = "project = \"my-proj\"\nmachine_type = \"n1-standard-4\"\n";
        let run = |extra: &str| {
            let g = graph("gcp", EU, "t4", &format!("{base}{extra}"), "");
            gcp_diagnostics(&g)
        };
        // 8 days is fine on-demand or spot, too long for flex_start.
        assert_eq!(run("max_run_duration = \"192h\"").len(), 0);
        let d = run("max_run_duration = \"192h\"\nprovisioning = \"flex_start\"");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("168h (7 days"), "{}", d[0].message);
        let d = run("max_run_duration = \"2881h\"");
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("2880h (120 days"), "{}", d[0].message);
        assert!(!d[0].message.contains("120d"), "{}", d[0].message);
    }

    #[test]
    fn a4x_is_reported_as_not_supported_yet() {
        let g = graph(
            "gcp",
            EU,
            "h100",
            "project = \"my-proj\"\n machine_type = \"a4x-highgpu-4g\"",
            "",
        );
        let d = gcp_diagnostics(&g);
        assert!(
            d.iter()
                .any(|d| d.message.contains("A4X (a4x-*) is not supported yet")),
            "{d:?}"
        );
    }

    #[test]
    fn provisioning_the_machine_does_not_offer_is_reported() {
        let g = graph(
            "gcp",
            EU,
            "h100",
            "project = \"my-proj\"\n machine_type = \"a3-highgpu-1g\"\n provisioning = \"standard\"",
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-018"]);
    }

    const RUN: &str = "project = \"my-proj\"\n backend = \"cloud_run\"";

    #[test]
    fn a_cheap_cloud_run_l4_profile_has_no_diagnostics() {
        let g = graph("gcp", EU, "l4", RUN, "scaling {\n max_replicas = 3\n }");
        assert_eq!(codes(&g), Vec::<String>::new());
        let mut g = graph(
            "gcp",
            r#"region = "us-central1""#,
            "rtx-pro-6000",
            RUN,
            "scaling {\n min_replicas = 0\n target_concurrency = 4\n }",
        );
        assert_eq!(
            codes(&g),
            ["AMICU-CAP-016"],
            "32 GiB is below the 80 GiB minimum"
        );
        assert!(gcp_diagnostics(&g)[0].message.contains("80GiB"));
        g.hardware_profiles[0].memory = amicu_core::ByteSize::from_bytes(80 << 30);
        assert_eq!(codes(&g), Vec::<String>::new());
    }

    #[test]
    fn access_is_cloud_run_only_and_idle_shutdown_is_compute_only() {
        let g = graph("gcp", EU, "l4", &format!("{RUN}\n access = \"iam\""), "");
        assert_eq!(codes(&g), Vec::<String>::new());
        let g = graph(
            "gcp",
            EU,
            "t4",
            "project = \"my-proj\"\n machine_type = \"n1-standard-4\"\n access = \"iam\"",
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-019"]);
        let g = graph(
            "gcp",
            EU,
            "l4",
            &format!("{RUN}\n idle_shutdown = \"30m\""),
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-019"]);
    }

    #[test]
    fn cloud_run_gpus_and_regions_come_from_the_catalog() {
        let g = graph("gcp", EU, "h100", RUN, "");
        assert_eq!(codes(&g), ["AMICU-CAP-017"]);
        let g = graph("gcp", r#"region = "us-west1""#, "l4", RUN, "");
        assert_eq!(codes(&g), ["AMICU-CAP-017"]);
        let d = &gcp_diagnostics(&g)[0];
        assert!(d.message.contains("europe-west4"), "{}", d.message);
        let g = graph("gcp", r#"region = "asia-south1""#, "l4", RUN, "");
        assert!(
            gcp_diagnostics(&g)[0].message.contains("invitation"),
            "{:?}",
            gcp_diagnostics(&g)
        );
        let g = graph("gcp", "", "l4", RUN, "");
        assert_eq!(codes(&g), ["AMICU-CAP-016"]);
    }

    #[test]
    fn cloud_run_refuses_what_it_cannot_honour() {
        let g = graph(
            "gcp",
            EU,
            "l4",
            &format!(
                "{RUN}\n zone = \"europe-west4-a\"\n machine_type = \"g2-standard-4\"\n provisioning = \"standard\"\n allowed_source_cidrs = [\"203.0.113.4/32\"]"
            ),
            "scaling {\n min_replicas = 1\n max_replicas = 2\n scaledown_window = \"5m\"\n }",
        );
        let d = gcp_diagnostics(&g);
        assert_eq!(codes(&g), ["AMICU-CAP-019"; 6], "{d:?}");
        for field in [
            "zone",
            "machine_type",
            "provisioning",
            "allowed_source_cidrs",
            "min_replicas",
            "scaledown_window",
        ] {
            assert!(
                d.iter().any(|d| d.message.contains(field)),
                "{field}: {d:?}"
            );
        }
        let g = graph(
            "gcp",
            EU,
            "l4",
            &format!("{RUN}\n service_account = \"\""),
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-016"]);
    }

    #[test]
    fn network_and_subnetwork_are_checked_and_compute_only() {
        let base = "project = \"my-proj\"\n";
        let g = graph(
            "gcp",
            EU,
            "t4",
            &format!("{base}network = \"vpc-main\"\n subnetwork = \"gpu\""),
            "",
        );
        assert_eq!(codes(&g), Vec::<String>::new());
        let g = graph(
            "gcp",
            EU,
            "t4",
            &format!(
                "{base}network = \"projects/h-project/global/networks/vpc\"\n \
                 subnetwork = \"regions/europe-west4/subnetworks/gpu\""
            ),
            "",
        );
        assert_eq!(codes(&g), Vec::<String>::new());
        let g = graph(
            "gcp",
            EU,
            "t4",
            &format!(
                "{base}network = \"vpc\"\n subnetwork = \"regions/us-central1/subnetworks/gpu\""
            ),
            "",
        );
        let d = gcp_diagnostics(&g);
        assert_eq!(codes(&g), ["AMICU-CAP-016"]);
        assert!(d[0].message.contains("us-central1"), "{}", d[0].message);
        let g = graph("gcp", EU, "t4", &format!("{base}subnetwork = \"gpu\""), "");
        assert_eq!(codes(&g), ["AMICU-CAP-016"]);
        let g = graph(
            "gcp",
            EU,
            "t4",
            &format!("{base}network = \"Bad Name\""),
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-016"]);
        let g = graph(
            "gcp",
            EU,
            "l4",
            &format!("{RUN}\n network = \"vpc\"\n subnetwork = \"gpu\""),
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-019"; 2]);
    }

    #[test]
    fn replicas_beyond_one_and_bad_cidrs_are_reported() {
        let g = graph(
            "gcp",
            EU,
            "t4",
            "project = \"my-proj\"\n allowed_source_cidrs = [\"10.0.0.1\"]",
            "scaling {\n max_replicas = 2\n }",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-016", "AMICU-CAP-016"]);
    }

    #[test]
    fn ipv6_cidrs_are_rejected_because_the_firewall_rule_is_ipv4() {
        let g = graph(
            "gcp",
            EU,
            "t4",
            "project = \"my-proj\"\n allowed_source_cidrs = [\"2001:db8::/32\", \"203.0.113.4/32\"]",
            "",
        );
        let found = gcp_diagnostics(&g);
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("IPv6"), "{}", found[0].message);
    }

    #[test]
    fn idle_shutdown_must_fit_inside_the_run_duration() {
        let ok = graph(
            "gcp",
            EU,
            "t4",
            "project = \"my-proj\"\n idle_shutdown = \"30m\"",
            "",
        );
        assert_eq!(gcp_diagnostics(&ok).len(), 0);
        for bad in ["1m", "9h"] {
            let g = graph(
                "gcp",
                EU,
                "t4",
                &format!("project = \"my-proj\"\n idle_shutdown = \"{bad}\""),
                "",
            );
            assert_eq!(codes(&g), ["AMICU-CAP-016"], "{bad}");
        }
    }

    #[test]
    fn a_region_is_required_without_a_zone() {
        let g = graph("gcp", "", "t4", "project = \"my-proj\"", "");
        assert_eq!(codes(&g), ["AMICU-CAP-016"]);
    }
}
