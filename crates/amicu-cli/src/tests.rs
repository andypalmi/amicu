//! Hermetic tests for the CLI commands, driven against temp-dir fixtures.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_plan::Plan;

use crate::OutputFormat;
use crate::checks::report_for;
use crate::commands;
use crate::planner::build_plan;
use crate::project::{LocalState, Project};

const VALID_CONFIG: &str = r#"# a comment that canonical formatting drops
resource "model" "support_chat" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-7B-Instruct"
  }
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "4GiB"
  }
}

resource "hardware_profile" "dev_mac" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m3_pro"
    count  = 1
    memory = "36GiB"
  }
  memory = "36GiB"
}

resource "deployment" "support_local" {
  model    = model.support_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}
"#;

// A deployment referencing a model that does not exist.
const INVALID_CONFIG: &str = r#"resource "hardware_profile" "dev_mac" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m3_pro"
    count  = 1
    memory = "36GiB"
  }
  memory = "36GiB"
}

resource "deployment" "orphan" {
  model    = model.ghost.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}
"#;

// Redeclares the `support_chat` model id already defined in `VALID_CONFIG`.
const DUP_MODEL: &str = r#"resource "model" "support_chat" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-7B-Instruct"
  }
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "4GiB"
  }
}
"#;

// A second, distinct model used to prove discovery reaches other files.
const SECOND_MODEL: &str = r#"resource "model" "summarizer" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-7B-Instruct"
  }
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "4GiB"
  }
}
"#;

// Shared model + deployment used by the residency fixtures below.
const RESIDENCY_MODEL_AND_DEPLOYMENT: &str = r#"resource "model" "support_chat" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-7B-Instruct"
  }
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "4GiB"
  }
}

resource "deployment" "support_local" {
  model    = model.support_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}
"#;

// Hardware whose region (aws us-east-1) resolves to the US jurisdiction while
// the only allowed residency is the EU.
const REGION_RESIDENCY_VIOLATION: &str = r#"resource "hardware_profile" "dev_mac" {
  provider = "aws"
  region   = "us-east-1"
  accelerator {
    kind   = "apple_silicon"
    model  = "m3_pro"
    count  = 1
    memory = "36GiB"
  }
  memory = "36GiB"
}

resource "data_class" "pii" {
  sensitivity = "confidential"
  residency   = ["eu"]
}
"#;

// Allow rule permits only `aws`; the deployment's provider is `local_apple`.
const PROVIDER_ALLOWLIST_VIOLATION: &str = r#"resource "hardware_profile" "dev_mac" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m3_pro"
    count  = 1
    memory = "36GiB"
  }
  memory = "36GiB"
}

resource "governance_rule" "only_aws" {
  effect            = "allow"
  allowed_providers = ["aws"]
}
"#;

// Region jurisdiction and provider both satisfy the configured allowlists.
const RESIDENCY_COMPLIANT: &str = r#"resource "hardware_profile" "dev_mac" {
  provider = "aws"
  region   = "eu-central-1"
  accelerator {
    kind   = "apple_silicon"
    model  = "m3_pro"
    count  = 1
    memory = "36GiB"
  }
  memory = "36GiB"
}

resource "data_class" "pii" {
  sensitivity = "confidential"
  residency   = ["eu"]
}

resource "governance_rule" "allow_aws" {
  effect            = "allow"
  allowed_providers = ["aws"]
  allowed_regions   = ["eu-central-1"]
}
"#;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("amicu-cli-test-{}-{nanos}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write(root: &Path, name: &str, contents: &str) {
    std::fs::write(root.join(name), contents).unwrap();
}

fn utf8(buf: Vec<u8>) -> String {
    String::from_utf8(buf).unwrap()
}

#[test]
fn init_creates_expected_files_and_is_idempotent() {
    let td = TempDir::new();
    let project = Project::new(&td.path);

    let mut out = Vec::new();
    assert_eq!(commands::init::run(&project, &mut out).unwrap(), 0);

    assert!(td.path.join("amicu.hcl").is_file());
    assert!(td.path.join("variables.amicu.hcl").is_file());
    assert!(td.path.join(".amicu/config.yaml").is_file());
    assert!(td.path.join(".amicu/state").is_dir());

    // The scaffolded config parses and validates.
    let report = report_for(&project.load_graph().unwrap());
    assert!(report.valid, "starter config must be valid");

    // Second run creates nothing and reports so.
    let mut out2 = Vec::new();
    commands::init::run(&project, &mut out2).unwrap();
    assert!(utf8(out2).contains("already initialized"));
}

#[test]
fn fmt_check_detects_and_fixes_drift() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);

    // The leading comment makes the file differ from canonical output.
    let mut out = Vec::new();
    assert_eq!(commands::fmt::run(&project, true, &mut out).unwrap(), 1);
    assert!(utf8(out).contains("would be reformatted"));

    // Writing mode fixes it, and a second check is clean.
    let mut out = Vec::new();
    assert_eq!(commands::fmt::run(&project, false, &mut out).unwrap(), 0);
    let mut out = Vec::new();
    assert_eq!(commands::fmt::run(&project, true, &mut out).unwrap(), 0);
}

#[test]
fn fmt_preserves_variables_file() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let vars = "# just comments\n";
    write(&td.path, "variables.amicu.hcl", vars);

    let mut out = Vec::new();
    commands::fmt::run(&project, false, &mut out).unwrap();
    assert_eq!(
        std::fs::read_to_string(td.path.join("variables.amicu.hcl")).unwrap(),
        vars,
        "comments-only file must be left untouched"
    );
}

#[test]
fn validate_valid_config_passes() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);

    let mut out = Vec::new();
    let code = commands::validate::run(&project, false, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 0);
    let report: amicu_core::diagnostic::DiagnosticReport = serde_json::from_slice(&out).unwrap();
    assert!(report.valid);
    assert_eq!(report.version, "v1");
}

#[test]
fn validate_invalid_config_reports_missing_reference() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", INVALID_CONFIG);

    let mut out = Vec::new();
    let code = commands::validate::run(&project, false, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 1);
    let report: amicu_core::diagnostic::DiagnosticReport = serde_json::from_slice(&out).unwrap();
    assert!(!report.valid);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code.to_string() == "AMICU-REF-001"),
        "expected a missing-reference diagnostic"
    );
}

fn has_code(report: &amicu_core::diagnostic::DiagnosticReport, code: &str) -> bool {
    report
        .diagnostics
        .iter()
        .any(|d| d.code.to_string() == code)
}

#[test]
fn region_residency_violation_blocks_validate_plan_and_apply() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", RESIDENCY_MODEL_AND_DEPLOYMENT);
    write(&td.path, "policy.hcl", REGION_RESIDENCY_VIOLATION);

    // validate
    let mut out = Vec::new();
    let code = commands::validate::run(&project, false, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 1);
    let report: amicu_core::diagnostic::DiagnosticReport = serde_json::from_slice(&out).unwrap();
    assert!(!report.valid);
    assert!(
        has_code(&report, "AMICU-POLICY-002"),
        "expected a region-residency diagnostic"
    );

    // plan
    let mut out = Vec::new();
    let code = commands::plan::run(&project, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 1);
    let report: amicu_core::diagnostic::DiagnosticReport = serde_json::from_slice(&out).unwrap();
    assert!(has_code(&report, "AMICU-POLICY-002"));

    // apply never reaches a side effect
    let mut out = Vec::new();
    let code = commands::apply::run(&project, true, None, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 1);
    let report: amicu_core::diagnostic::DiagnosticReport = serde_json::from_slice(&out).unwrap();
    assert!(has_code(&report, "AMICU-POLICY-002"));
    assert!(
        project.load_state().unwrap().is_none(),
        "a residency-violating stack must never reach apply"
    );
}

#[test]
fn provider_allowlist_violation_reports_policy_003() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", RESIDENCY_MODEL_AND_DEPLOYMENT);
    write(&td.path, "policy.hcl", PROVIDER_ALLOWLIST_VIOLATION);

    let mut out = Vec::new();
    let code = commands::validate::run(&project, false, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 1);
    let report: amicu_core::diagnostic::DiagnosticReport = serde_json::from_slice(&out).unwrap();
    assert!(!report.valid);
    assert!(
        has_code(&report, "AMICU-POLICY-003"),
        "expected a provider-not-allowed diagnostic"
    );
}

#[test]
fn residency_compliant_stack_passes() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", RESIDENCY_MODEL_AND_DEPLOYMENT);
    write(&td.path, "policy.hcl", RESIDENCY_COMPLIANT);

    let mut out = Vec::new();
    let code = commands::validate::run(&project, false, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 0);
    let report: amicu_core::diagnostic::DiagnosticReport = serde_json::from_slice(&out).unwrap();
    assert!(report.valid);
    assert!(!has_code(&report, "AMICU-POLICY-002"));
    assert!(!has_code(&report, "AMICU-POLICY-003"));
}

#[test]
fn plan_builds_expected_action() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let graph = project.load_graph().unwrap();

    let result = build_plan(&graph, None).unwrap();
    assert_eq!(result.plan.actions.len(), 1);
    let action = &result.plan.actions[0];
    assert_eq!(action.address, "deployment.support_local");
    assert_eq!(format!("{:?}", action.kind).to_lowercase(), "create");
    assert!(action.artifact.is_some());
    assert_eq!(action.provider.as_deref(), Some("local_apple"));
}

#[test]
fn plan_json_output_is_valid_contract() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);

    let mut out = Vec::new();
    let code = commands::plan::run(&project, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 0);
    let plan: Plan = serde_json::from_slice(&out).unwrap();
    assert_eq!(plan.version, Plan::VERSION);
    assert_eq!(plan.actions.len(), 1);
}

#[test]
fn plan_diffs_against_prior_state() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let graph = project.load_graph().unwrap();

    // Applying against a prior identical graph yields no actions.
    let unchanged = build_plan(&graph, Some(&graph)).unwrap();
    assert_eq!(unchanged.plan.actions, Vec::new());
}

#[test]
fn plan_detects_referenced_model_change() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let prior = project.load_graph().unwrap();

    // The deployment block is untouched; only the referenced model's variant
    // changes. Change detection must still surface an update.
    let mut changed = prior.clone();
    changed.models[0].variants[0].file = "model-q5.gguf".to_owned();

    let result = build_plan(&changed, Some(&prior)).unwrap();
    assert_eq!(result.plan.actions.len(), 1);
    assert_eq!(
        format!("{:?}", result.plan.actions[0].kind).to_lowercase(),
        "update"
    );
}

#[test]
fn plan_detects_referenced_hardware_change() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let prior = project.load_graph().unwrap();

    let mut changed = prior.clone();
    changed.hardware_profiles[0]
        .accelerator
        .as_mut()
        .unwrap()
        .model = "m4_max".to_owned();

    let result = build_plan(&changed, Some(&prior)).unwrap();
    assert_eq!(result.plan.actions.len(), 1);
    assert_eq!(
        format!("{:?}", result.plan.actions[0].kind).to_lowercase(),
        "update"
    );
}

#[test]
fn duplicate_id_across_files_is_diagnosed() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "a.hcl", VALID_CONFIG);
    // A second file redeclares the same model id.
    write(&td.path, "b.hcl", DUP_MODEL);

    let mut out = Vec::new();
    let code = commands::validate::run(&project, false, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 1);
    let report: amicu_core::diagnostic::DiagnosticReport = serde_json::from_slice(&out).unwrap();
    assert!(!report.valid);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code.to_string() == "AMICU-SCHEMA-002"),
        "expected a duplicate-id diagnostic"
    );

    // And `plan` fails cleanly rather than emitting two same-address items.
    let mut out = Vec::new();
    assert_eq!(
        commands::plan::run(&project, OutputFormat::Human, &mut out).unwrap(),
        1
    );
}

#[test]
fn nested_config_is_discovered_and_merged() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let sub = td.path.join("environments");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("prod.hcl"), SECOND_MODEL).unwrap();

    let graph = project.load_graph().unwrap();
    assert_eq!(graph.models.len(), 2, "nested file must be merged");
}

#[test]
fn hcl_under_state_dir_is_not_discovered() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let dot = td.path.join(".amicu");
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::write(dot.join("stray.hcl"), SECOND_MODEL).unwrap();

    let graph = project.load_graph().unwrap();
    assert_eq!(graph.models.len(), 1, ".amicu/ must be excluded");
}

#[test]
fn apply_json_stdout_is_a_single_json_value() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    // Prior state holds a deployment the new config drops, yielding a destroy
    // plan that starts nothing (no handle held) - so no process is spawned.
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let prior = project.load_graph().unwrap();
    project
        .save_state(&LocalState {
            version: LocalState::VERSION.to_owned(),
            graph: prior,
            endpoints: Vec::new(),
        })
        .unwrap();
    write(&td.path, "amicu.hcl", SECOND_MODEL);

    let mut out = Vec::new();
    let code = commands::apply::run(&project, true, None, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 0);
    // The whole stdout buffer must parse as one JSON value.
    let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(value.is_array());
}

#[test]
fn plan_json_reports_dangling_reference() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", INVALID_CONFIG);

    let mut out = Vec::new();
    let code = commands::plan::run(&project, OutputFormat::Json, &mut out).unwrap();
    assert_eq!(code, 1);
    // A structured diagnostic body must reach stdout, like `validate --output json`.
    let report: amicu_core::diagnostic::DiagnosticReport = serde_json::from_slice(&out).unwrap();
    assert!(!report.valid);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code.to_string() == "AMICU-REF-001"),
        "expected a missing-reference diagnostic"
    );
}

#[test]
fn init_output_passes_fmt_check() {
    let td = TempDir::new();
    let project = Project::new(&td.path);

    let mut out = Vec::new();
    assert_eq!(commands::init::run(&project, &mut out).unwrap(), 0);

    // The scaffolded files must already be canonical under the formatter.
    let mut out = Vec::new();
    assert_eq!(commands::fmt::run(&project, true, &mut out).unwrap(), 0);
    assert!(utf8(out).contains("correctly formatted"));
}

#[test]
fn doctor_emits_structured_json() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);

    let mut out = Vec::new();
    // Exit code depends on host; assert on the report shape, not the value.
    let _ = commands::doctor::run(&project, OutputFormat::Json, &mut out).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(value["version"], "v1");
    assert!(value["checks"].as_array().unwrap().len() >= 3);
    assert!(value["ok"].is_boolean());
}

#[test]
fn state_round_trips() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let graph = project.load_graph().unwrap();

    let state = LocalState {
        version: LocalState::VERSION.to_owned(),
        graph: graph.clone(),
        endpoints: Vec::new(),
    };
    project.save_state(&state).unwrap();
    let loaded = project.load_state().unwrap().unwrap();
    assert_eq!(loaded.graph, graph);
    assert_eq!(loaded.version, "v1");
}

#[test]
#[ignore = "spawns a real llama-server via the host supervisor"]
fn apply_end_to_end() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);

    let mut out = Vec::new();
    let code = commands::apply::run(&project, true, None, OutputFormat::Human, &mut out).unwrap();
    assert_eq!(code, 0);
    assert!(project.load_state().unwrap().is_some());
}

fn assert_frontmatter(text: &str, expected_name: &str) {
    assert!(
        text.starts_with("---\n"),
        "SKILL.md must open with YAML frontmatter"
    );
    let body = &text[4..];
    let end = body
        .find("\n---\n")
        .expect("frontmatter must be closed by ---");
    let front = &body[..end];
    assert!(
        front.contains(&format!("name: {expected_name}")),
        "frontmatter must declare name: {expected_name}"
    );
    assert!(
        front.contains("description:"),
        "frontmatter must carry an activation description"
    );
}

#[test]
fn skill_export_writes_expected_tree() {
    use crate::skills::SKILLS;

    let td = TempDir::new();
    let mut out = Vec::new();
    commands::skill::write_skills(&td.path, &mut out).unwrap();

    assert_eq!(SKILLS.len(), 8, "there must be exactly eight skills");
    for skill in SKILLS {
        let dir = td.path.join(skill.name);
        let skill_md = dir.join("SKILL.md");
        assert!(skill_md.is_file(), "{} SKILL.md missing", skill.name);
        assert_frontmatter(&std::fs::read_to_string(&skill_md).unwrap(), skill.name);
        for file in skill.references {
            assert!(
                dir.join("references").join(file.name).is_file(),
                "{}/references/{} missing",
                skill.name,
                file.name
            );
        }
    }

    // The core skill must carry the verbatim safety rules.
    let core = std::fs::read_to_string(td.path.join("amicu-core/SKILL.md")).unwrap();
    for rule in [
        "Never run `amicu apply` without a reviewed plan",
        "Never run destructive state commands without explicit approval",
        "Never reveal, commit, or transmit secret values",
        "Always validate before plan",
        "Preserve existing configuration and make minimal changes",
    ] {
        assert!(
            core.contains(rule),
            "core skill missing safety rule: {rule}"
        );
    }
}

#[test]
fn skill_export_works_without_a_project() {
    // A temp dir with no amicu config: export must still succeed.
    let td = TempDir::new();
    let mut out = Vec::new();
    let code =
        commands::skill::export(crate::SkillTarget::ClaudeCode, Some(&td.path), &mut out).unwrap();
    assert_eq!(code, 0);
    assert!(td.path.join("amicu-core/SKILL.md").is_file());
    assert!(td.path.join("amicu-presets/SKILL.md").is_file());
}

#[test]
fn skill_export_is_idempotent() {
    let td = TempDir::new();
    let mut out = Vec::new();
    commands::skill::write_skills(&td.path, &mut out).unwrap();
    let mut out2 = Vec::new();
    commands::skill::write_skills(&td.path, &mut out2).unwrap();
    assert_eq!(utf8(out), utf8(out2), "re-export must be deterministic");
}

#[test]
fn approval_parsing() {
    assert!(commands::apply::approved("y"));
    assert!(commands::apply::approved("Y\n"));
    assert!(commands::apply::approved("  yes  "));
    assert!(!commands::apply::approved("n"));
    assert!(!commands::apply::approved(""));
    assert!(!commands::apply::approved("no"));
}
