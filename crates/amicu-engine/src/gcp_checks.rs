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
        if let Some(s) = &d.scaling
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
        out.extend(cloud_run_problems(hw));
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
    out
}

fn cloud_run_problems(hw: &HardwareProfile) -> Vec<(DiagnosticCode, String)> {
    let mut out = vec![(
        catalog::CAP_GCP_BACKEND_UNSUPPORTED,
        "`backend = \"cloud_run\"` is not supported by `apply` yet; use `backend = \"compute\"`"
            .to_owned(),
    )];
    if let Some(acc) = &hw.accelerator
        && let Some(model) = amicu_gcp::resolve_model(&acc.model)
        && GcpCatalog::new().cloud_run_gpu(model.name).is_none()
    {
        out.push((
            catalog::CAP_GCP_GPU_UNAVAILABLE,
            format!(
                "`{}` is not offered on Cloud Run (only L4 and RTX PRO 6000 are); use \
                 `backend = \"compute\"`",
                acc.model
            ),
        ));
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

    #[test]
    fn cloud_run_parses_but_is_not_supported_and_h100_has_no_cloud_run() {
        let g = graph(
            "gcp",
            EU,
            "l4",
            "project = \"my-proj\"\n backend = \"cloud_run\"",
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-019"]);
        let g = graph(
            "gcp",
            EU,
            "h100",
            "project = \"my-proj\"\n backend = \"cloud_run\"",
            "",
        );
        assert_eq!(codes(&g), ["AMICU-CAP-019", "AMICU-CAP-017"]);
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
