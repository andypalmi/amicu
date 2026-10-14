//! `amicu plan [--output json]`.

use std::io::Write;

use amicu_core::diagnostic::{DiagnosticReport, Severity};
use amicu_core::graph::Graph;
use amicu_modal::ModalSupervisor;
use amicu_modal::supervisor::app_name_for;

use crate::OutputFormat;
use crate::checks::report_for;
use crate::error::CliError;
use crate::planner::{PlanError, PlanItem, PlanResult, build_project_plan};
use crate::project::Project;
use crate::write_json;

/// Build and render the plan; exits non-zero when validation errors block it.
pub(crate) fn run(
    project: &Project,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let graph = project.load_graph()?;

    // Surface structural errors (dangling refs, duplicate ids) through the same
    // diagnostic path as `validate`, before `build_plan` can hard-error on them.
    let report = report_for(&graph);
    if !report.valid {
        emit_report(&report, output, out)?;
        return Ok(1);
    }

    let prior =
        crate::refresh::load_refreshed_state(project, &crate::refresh::ModalLookup, output, out)?
            .map(|s| {
                // A pending deployment (start unconfirmed) is planned as absent.
                let mut g = s.graph;
                g.deployments.retain(|d| !s.pending.contains(&d.id));
                g
            });
    let result = match build_project_plan(project, &graph, prior.as_ref()) {
        Ok(result) => result,
        // A deployment failed to plan (e.g. a CAP-002/CAP-003 extra_args/env
        // conflict); `build_plan` hands back everything it collected instead
        // of a bare adapter error, so render it the same way as any other
        // blocking report (amicu-planning#150 review hardening).
        Err(PlanError(report)) => {
            emit_report(&report, output, out)?;
            return Ok(1);
        }
    };

    if result.has_errors() {
        // Cannot produce a reviewable plan for an invalid config.
        emit_errors(&result, output, out)?;
        return Ok(1);
    }

    if output.is_json() {
        write_json(out, &result.plan)?;
    } else {
        print_human(&result, &graph, out)?;
    }
    Ok(0)
}

fn emit_errors(
    result: &PlanResult,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    emit_report(
        &DiagnosticReport::new(result.diagnostics.clone()),
        output,
        out,
    )
}

fn emit_report(
    report: &DiagnosticReport,
    output: OutputFormat,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    if output.is_json() {
        write_json(out, report)?;
    } else {
        writeln!(out, "Cannot plan: configuration has errors.")
            .map_err(|e| CliError::io("write failed", e))?;
        for d in &report.diagnostics {
            if d.severity == Severity::Error {
                writeln!(out, "  error [{}] {}", d.code, d.message)
                    .map_err(|e| CliError::io("write failed", e))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn print_human(
    result: &PlanResult,
    graph: &Graph,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let io = |r: std::io::Result<()>| r.map_err(|e| CliError::io("write failed", e));

    if result.items.is_empty() {
        io(writeln!(
            out,
            "No changes. Infrastructure matches the configuration."
        ))?;
    } else {
        io(writeln!(out, "Plan: {} action(s).\n", result.items.len()))?;
        for item in &result.items {
            let kind = format!("{:?}", item.action.kind).to_lowercase();
            io(writeln!(out, "  {} {}", symbol(&kind), item.action.address))?;
            io(writeln!(out, "      action:   {kind}"))?;
            if let Some(hw) = &item.action.hardware {
                io(writeln!(out, "      hardware: {hw}"))?;
            }
            // `provider`/`region` were already computed by `action_for`
            // (`amicu_engine::planner`) for every hardware profile, not just
            // Modal's - this just surfaces them (amicu-planning#158 PR 5).
            // The exact Modal `gpu=` string (`"H100:2"`) is not shown here:
            // deriving it needs `amicu-modal`'s mapping, and this print path
            // is shared with the HTTP API's `/plan` display contract, so
            // pulling that crate in here was avoided (see
            // `crates/amicu-modal/src/supervisor.rs`'s packaging note).
            if let Some(provider) = &item.action.provider {
                match &item.action.region {
                    Some(region) => {
                        io(writeln!(
                            out,
                            "      target:   {provider} (region {region})"
                        ))?;
                    }
                    None => io(writeln!(out, "      target:   {provider}"))?,
                }
            }
            if let Some(artifact) = &item.action.artifact {
                io(writeln!(out, "      artifact: {}", artifact.name))?;
            }
            // The endpoint API key: always redacted, and only the secret's name
            // is shown - the value is never read at plan time.
            if let Some(shown) = item.action.runtime.iter().find(|s| s.key == "api_key") {
                let secret = graph
                    .deployments
                    .iter()
                    .find(|d| d.id == item.deployment)
                    .and_then(|d| d.api_key.as_ref());
                match secret {
                    Some(r) => io(writeln!(
                        out,
                        "      api_key:  {} (secret \"{}\")",
                        shown.value, r.name
                    ))?,
                    None => io(writeln!(out, "      api_key:  {}", shown.value))?,
                }
            }
            if let Some(cmd) = &item.rendered {
                // On Modal the supervisor rewrites the argv before launch
                // (`-hf`/`-hff`, `--no-mmproj`, `--host 0.0.0.0`); show what
                // is really sent, via the supervisor's own transformation.
                let cmd = if item.action.provider.as_deref() == Some("modal") {
                    modal_launch_text(graph, item).unwrap_or_else(|| cmd.clone())
                } else {
                    cmd.clone()
                };
                io(writeln!(out, "      launch:   {cmd}"))?;
            }
            if let Some(cp) = &item.action.cost_performance {
                for a in &cp.assumptions {
                    io(writeln!(out, "      note:     {a}"))?;
                }
            }
            io(writeln!(out))?;
        }
    }

    for d in &result.diagnostics {
        let marker = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warn",
            Severity::Info => "info",
        };
        io(writeln!(out, "  {marker}: {}", d.message))?;
    }
    Ok(())
}

/// The launch line `ModalSupervisor` would really send for `item`, or `None`
/// when it cannot be derived (the caller then shows the local rendering;
/// `apply` surfaces the underlying error).
fn modal_launch_text(graph: &Graph, item: &PlanItem) -> Option<String> {
    let launch = item.launch.as_ref()?;
    let mut supervisor = ModalSupervisor::new(app_name_for(item.deployment.as_str()));
    if let Some(model) = graph
        .deployments
        .iter()
        .find(|d| d.id == item.deployment)
        .and_then(|d| graph.models.iter().find(|m| m.id == d.model))
    {
        supervisor = supervisor.with_gguf_model(model);
    }
    supervisor.launch_text(launch).ok()
}

fn symbol(kind: &str) -> &'static str {
    match kind {
        "create" => "+",
        "destroy" => "-",
        "replace" => "±",
        _ => "~",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use amicu_core::ByteSize;
    use amicu_core::resource::{
        Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Deployment, GgufVariant,
        HardwareProfile, LlamaCppSettings, Model, ModelSource, Quantization, Runtime,
        SourceRegistry, VllmSettings,
    };

    use super::*;
    use crate::planner::build_plan;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn graph(provider: &str, runtime: Runtime) -> Graph {
        let mut g = Graph::new();
        g.models.push(Model {
            id: "m".into(),
            source: ModelSource {
                registry: SourceRegistry::HuggingFace,
                reference: "org/tiny-1B".into(),
            },
            architecture: "llama".into(),
            modalities: Vec::new(),
            artifact: Artifact {
                format: ArtifactFormat::Gguf,
                quantization: None,
            },
            context_length: Some(8192),
            variants: vec![GgufVariant {
                quantization: Quantization::Q4KM,
                file: "m.q4_k_m.gguf".into(),
                size: Some(ByteSize::from_bytes(GIB)),
                sha256: None,
            }],
            n_layers: None,
            n_kv_heads: None,
            n_kv_layers: None,
            head_dim: None,
        });
        g.hardware_profiles.push(HardwareProfile {
            id: "hw".into(),
            provider: provider.into(),
            region: None,
            accelerator: Some(Accelerator {
                kind: AcceleratorKind::Gpu,
                model: "l4".into(),
                count: 1,
                memory: ByteSize::from_bytes(24 * GIB),
                memory_bandwidth_gbps: None,
            }),
            cpu: None,
            memory: ByteSize::from_bytes(48 * GIB),
            interconnect: None,
            price: None,
        });
        g.deployments.push(Deployment {
            scaling: None,
            api_key: None,
            id: "dep".into(),
            model: "m".into(),
            hardware_profile: "hw".into(),
            runtime,
        });
        g
    }

    fn llama() -> Runtime {
        Runtime::LlamaCpp(LlamaCppSettings {
            context_length: 8192,
            port: 8000,
            n_gpu_layers: -1,
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            ..Default::default()
        })
    }

    fn vllm() -> Runtime {
        Runtime::Vllm(VllmSettings {
            context_length: 8192,
            port: 8000,
            ..Default::default()
        })
    }

    fn launch_line(g: &Graph) -> String {
        let result = build_plan(g, None).expect("plan");
        let mut out = Vec::new();
        print_human(&result, g, &mut out).expect("print");
        let text = String::from_utf8(out).expect("utf8");
        text.lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("launch:")
                    .map(|c| c.trim().to_owned())
            })
            .unwrap_or_else(|| panic!("no launch line in:\n{text}"))
    }

    #[test]
    fn project_plan_keeps_the_model_path_relative() {
        let g = graph("local", llama());
        let project = Project::new("/proj");
        let result = crate::planner::build_project_plan(&project, &g, None).expect("plan");
        let launch = result.items[0].rendered.clone().expect("rendered");
        assert!(launch.contains("models/m/"), "{launch}");
        assert!(
            !launch.contains("/proj"),
            "no absolute project path: {launch}"
        );
    }

    fn human_plan(g: &Graph) -> String {
        let result = build_plan(g, None).expect("plan");
        let mut out = Vec::new();
        print_human(&result, g, &mut out).expect("print");
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn api_key_shows_redacted_with_the_secret_name_only() {
        let mut g = graph("modal", vllm());
        assert!(!human_plan(&g).contains("api_key"));
        g.deployments[0].api_key = Some(amicu_core::secret::SecretRef::new("CHAT_API_KEY"));
        let text = human_plan(&g);
        let line = text
            .lines()
            .find(|l| l.trim().starts_with("api_key:"))
            .unwrap_or_else(|| panic!("no api_key line in:\n{text}"));
        assert!(line.contains("******"), "{line}");
        assert!(line.contains("CHAT_API_KEY"), "{line}");
        assert_eq!(line.trim(), "api_key:  ****** (secret \"CHAT_API_KEY\")");
        // Planning never resolves: even with the variable set, its value is
        // nowhere in the output.
        assert!(!text.contains("--api-key"), "{text}");
    }

    #[test]
    fn modal_vllm_shows_all_interfaces_bind() {
        let line = launch_line(&graph("modal", vllm()));
        assert!(line.contains("--host 0.0.0.0"), "{line}");
        assert!(!line.contains("127.0.0.1"), "{line}");
    }

    #[test]
    fn modal_llamacpp_shows_the_argv_modal_really_runs() {
        let line = launch_line(&graph("modal", llama()));
        assert!(
            line.contains("-hf org/tiny-1B -hff m.q4_k_m.gguf"),
            "{line}"
        );
        assert!(!line.contains(" -m "), "{line}");
        assert!(line.contains("--host 0.0.0.0"), "{line}");
        assert!(line.contains("--no-mmproj"), "{line}");
    }

    #[test]
    fn host_targets_keep_the_local_rendering() {
        for runtime in [llama(), vllm()] {
            let line = launch_line(&graph("local", runtime));
            assert!(!line.contains("0.0.0.0"), "{line}");
            assert!(!line.contains("--no-mmproj"), "{line}");
            assert!(!line.contains("-hf "), "{line}");
        }
    }
}
