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
    reference = "unsloth/Qwen3.5-9B-GGUF"
  }
  architecture   = "qwen35"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "5680522464B"
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
    reference = "unsloth/Qwen3.5-9B-GGUF"
  }
  architecture   = "qwen35"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "5680522464B"
  }
}
"#;

// A second, distinct model used to prove discovery reaches other files.
const SECOND_MODEL: &str = r#"resource "model" "summarizer" {
  source {
    registry  = "hugging_face"
    reference = "unsloth/Qwen3.5-9B-GGUF"
  }
  architecture   = "qwen35"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "5680522464B"
  }
}
"#;

// Shared model + deployment used by the residency fixtures below.
const RESIDENCY_MODEL_AND_DEPLOYMENT: &str = r#"resource "model" "support_chat" {
  source {
    registry  = "hugging_face"
    reference = "unsloth/Qwen3.5-9B-GGUF"
  }
  architecture   = "qwen35"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "model.gguf"
    size         = "5680522464B"
  }
}

resource "deployment" "support_local" {
  model    = model.support_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  api_key  = secret("SUPPORT_LOCAL_API_KEY")
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
    assert_eq!(
        commands::init::run(
            &project,
            Some(Some(commands::init::InitTarget::Mac)),
            Some(&crate::host::fake::mac("m4_pro", 48)),
            &mut out
        )
        .unwrap(),
        0
    );

    assert!(td.path.join("amicu.hcl").is_file());
    assert!(td.path.join("variables.amicu.hcl").is_file());
    assert!(td.path.join(".amicu/config.yaml").is_file());
    assert!(td.path.join(".amicu/state").is_dir());

    // The scaffolded config parses and validates.
    let report = report_for(&project.load_graph().unwrap());
    assert!(report.valid, "starter config must be valid");

    // Second run creates nothing and reports so.
    let mut out2 = Vec::new();
    commands::init::run(
        &project,
        Some(Some(commands::init::InitTarget::Mac)),
        Some(&crate::host::fake::mac("m4_pro", 48)),
        &mut out2,
    )
    .unwrap();
    assert!(utf8(out2).contains("already initialized"));
}

#[test]
fn init_ignores_dot_amicu_idempotently() {
    let read = |td: &TempDir| std::fs::read_to_string(td.path.join(".gitignore")).unwrap();
    // No ignore file: created.
    let td = TempDir::new();
    let project = Project::new(&td.path);
    commands::init::run(
        &project,
        Some(Some(commands::init::InitTarget::Mac)),
        Some(&crate::host::fake::mac("m4_pro", 48)),
        &mut Vec::new(),
    )
    .unwrap();
    commands::init::run(
        &project,
        Some(Some(commands::init::InitTarget::Mac)),
        Some(&crate::host::fake::mac("m4_pro", 48)),
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(read(&td), ".amicu/\n");

    // Existing file without trailing newline: appended, other entries kept.
    let td = TempDir::new();
    write(&td.path, ".gitignore", "target");
    let project = Project::new(&td.path);
    commands::init::run(
        &project,
        Some(Some(commands::init::InitTarget::Mac)),
        Some(&crate::host::fake::mac("m4_pro", 48)),
        &mut Vec::new(),
    )
    .unwrap();
    commands::init::run(
        &project,
        Some(Some(commands::init::InitTarget::Mac)),
        Some(&crate::host::fake::mac("m4_pro", 48)),
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(read(&td), "target\n.amicu/\n");

    // Already covered (alternate spelling): untouched.
    let td = TempDir::new();
    write(&td.path, ".gitignore", "/.amicu\n");
    commands::init::run(
        &Project::new(&td.path),
        Some(Some(commands::init::InitTarget::Mac)),
        Some(&crate::host::fake::mac("m4_pro", 48)),
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(read(&td), "/.amicu\n");
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
    let code = run_apply(&project, OutputFormat::Json, &mut out).unwrap();
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
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
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
            project_id: None,
            pending: Vec::new(),
            stops: Vec::new(),
            fingerprints: std::collections::BTreeMap::default(),
            serial: 0,
            purge_pending: Vec::new(),
            extra: std::collections::BTreeMap::default(),
            graph: prior,
            endpoints: Vec::new(),
        })
        .unwrap();
    write(&td.path, "amicu.hcl", SECOND_MODEL);

    let mut out = Vec::new();
    let code = run_apply(&project, OutputFormat::Json, &mut out).unwrap();
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
    assert_eq!(
        commands::init::run(
            &project,
            Some(Some(commands::init::InitTarget::Mac)),
            Some(&crate::host::fake::mac("m4_pro", 48)),
            &mut out
        )
        .unwrap(),
        0
    );

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
        project_id: None,
        pending: Vec::new(),
        stops: Vec::new(),
        fingerprints: std::collections::BTreeMap::default(),
        serial: 0,
        purge_pending: Vec::new(),
        extra: std::collections::BTreeMap::default(),
        graph: graph.clone(),
        endpoints: Vec::new(),
    };
    project.save_state(&state).unwrap();
    let loaded = project.load_state().unwrap().unwrap();
    assert_eq!(loaded.graph, graph);
    assert_eq!(loaded.version, LocalState::VERSION);
}

#[test]
fn apply_fails_fast_when_state_is_locked() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);

    let _held = crate::lock::StateLock::acquire(&project.state_dir(), "apply", None).unwrap();
    let mut out = Vec::new();
    let err = run_apply(&project, OutputFormat::Human, &mut out)
        .expect_err("a locked state must fail apply");
    assert!(
        matches!(err, crate::error::CliError::StateLocked(_)),
        "{err}"
    );
    // Failed before any plan output or side effect, and wrote no state.
    assert_eq!(out.len(), 0);
    assert!(project.load_state().unwrap().is_none());
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
    let code = commands::skill::export(
        Some(crate::SkillTarget::ClaudeCode),
        Some(&td.path),
        &mut out,
    )
    .unwrap();
    assert_eq!(code, 0);
    assert!(td.path.join("amicu-core/SKILL.md").is_file());
    assert!(td.path.join("amicu-presets/SKILL.md").is_file());
    let out = utf8(out);
    assert!(out.starts_with("Exporting 8 skills for claude-code to "));
    assert!(
        !out.contains("for cursor"),
        "single target must not export others"
    );
}

#[test]
fn skill_export_without_target_writes_every_target() {
    use crate::SkillTarget;

    let td = TempDir::new();
    let plan: Vec<(SkillTarget, std::path::PathBuf)> = SkillTarget::ALL
        .iter()
        .map(|&t| (t, td.path.join(t.label())))
        .collect();
    let mut out = Vec::new();
    commands::skill::export_plan(&plan, &mut out).unwrap();

    let out = utf8(out);
    let mut last = 0;
    for label in ["claude-code", "cursor", "windsurf", "generic"] {
        let header = format!("Exporting 8 skills for {label} to ");
        let at = out
            .find(&header)
            .unwrap_or_else(|| panic!("no header for {label}"));
        assert!(at >= last, "targets must be listed in fixed order");
        last = at;
        for name in ["amicu-core", "amicu-presets"] {
            let written = td.path.join(label).join(name).join("SKILL.md");
            assert!(written.is_file(), "{label}: {} missing", written.display());
            assert!(out.contains(&written.display().to_string()));
        }
    }
    assert_eq!(out.matches("Exporting ").count(), 4);
}

#[test]
fn skill_export_without_target_rejects_path() {
    let td = TempDir::new();
    let mut out = Vec::new();
    let err = commands::skill::export(None, Some(&td.path), &mut out).unwrap_err();
    assert!(err.to_string().contains("--path needs --target"));
    assert!(out.is_empty(), "nothing may be written before the refusal");
    assert!(std::fs::read_dir(&td.path).unwrap().next().is_none());
}

#[test]
fn skill_export_default_roots_are_disjoint() {
    use crate::SkillTarget;

    let plan: Vec<(SkillTarget, std::path::PathBuf)> = SkillTarget::ALL
        .iter()
        .map(|&t| {
            (
                t,
                std::path::PathBuf::from("/project").join(t.default_root()),
            )
        })
        .collect();
    commands::skill::check_disjoint(&plan).expect("default layouts must not collide");

    let shared = vec![
        (SkillTarget::Cursor, std::path::PathBuf::from("/out")),
        (SkillTarget::Generic, std::path::PathBuf::from("/out")),
    ];
    let err = commands::skill::check_disjoint(&shared).unwrap_err();
    assert!(err.to_string().contains("overlapping directories"));
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

/// `VALID_CONFIG` plus a second deployment, `support_two`, on another port.
fn two_deployments() -> String {
    let second = r#"
resource "deployment" "support_two" {
  model    = model.support_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8001
    n_gpu_layers   = -1
  }
}
"#;
    format!("{VALID_CONFIG}{second}")
}

fn two_deployment_graph() -> amicu_core::graph::Graph {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", &two_deployments());
    project.load_graph().unwrap()
}

fn destroyed_ids(result: &crate::planner::PlanResult) -> Vec<String> {
    result
        .items
        .iter()
        .map(|i| i.deployment.as_str().to_owned())
        .collect()
}

#[test]
fn destroy_plans_every_deployment_in_state() {
    let prior = two_deployment_graph();
    let result = commands::destroy::plan_destroy(&prior, &[], &[]).unwrap();
    assert_eq!(result.plan.actions.len(), 2);
    assert!(
        result
            .plan
            .actions
            .iter()
            .all(|a| a.kind == amicu_plan::contract::ActionKind::Destroy)
    );
    assert!(result.items.iter().all(|i| i.launch.is_none()));
    assert_eq!(destroyed_ids(&result), ["support_local", "support_two"]);
}

#[test]
fn destroy_target_limits_the_plan_to_the_named_deployments() {
    let prior = two_deployment_graph();
    let result = commands::destroy::plan_destroy(&prior, &[], &["support_two".to_owned()]).unwrap();
    assert_eq!(destroyed_ids(&result), ["support_two"]);
    assert_eq!(result.plan.actions.len(), 1);
    assert_eq!(result.plan.actions[0].address, "deployment.support_two");
}

#[test]
fn destroy_unknown_target_is_an_error_and_plans_nothing() {
    let prior = two_deployment_graph();
    let err = commands::destroy::plan_destroy(
        &prior,
        &[],
        &["support_two".to_owned(), "ghost".to_owned()],
    )
    .err()
    .expect("unknown target must fail");
    let commands::destroy::DestroyPlanError::UnknownTarget(msg) = err else {
        panic!("expected UnknownTarget");
    };
    assert!(msg.contains("ghost"), "{msg}");
    assert!(
        msg.starts_with("unknown --target deployment(s): ghost;"),
        "only the unknown name is reported: {msg}"
    );
    assert!(msg.contains("support_local"), "lists known names: {msg}");
}

// ---- apply / destroy end to end, over fake host and Modal -------------------
//
// None of these touches the real launchctl/systemctl or the network: the host
// supervisor and Modal are injected, and `TempDir::new` arms a guard that
// panics if a real service command is ever spawned.

use std::sync::{Arc, Mutex};

use crate::commands::apply::Env;
use crate::modal_ops::{ModalOps, Workspace};
use crate::refresh::AppLookup;
use crate::stops::{Backend, StopRecord};

type Hook = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct StopLog {
    stopped: Mutex<Vec<String>>,
    started: Mutex<Vec<String>>,
    /// Every start and stop, host and Modal, in the order they happened.
    events: Mutex<Vec<String>>,
    /// One-shot hooks fired by name (`ctx`, `start`, `stop`) from the fake host,
    /// to simulate another amicu writing state at that moment.
    hooks: Mutex<std::collections::HashMap<&'static str, Hook>>,
}

impl StopLog {
    fn fire(&self, name: &str) {
        let hook = self.hooks.lock().unwrap().remove(name);
        if let Some(hook) = hook {
            hook();
        }
    }
}

struct FakeHost {
    log: Arc<StopLog>,
    fail: bool,
    /// `start` fails, as a bad model path would.
    start_fails: bool,
    ctx: Option<amicu_llamacpp::HostContext>,
    owner: Option<u32>,
}

#[async_trait::async_trait]
impl amicu_llamacpp::ProcessSupervisor for FakeHost {
    async fn stop_all_sidecars(&self) -> Result<Vec<String>, amicu_llamacpp::LlamaCppError> {
        self.log
            .events
            .lock()
            .unwrap()
            .push("stop telemetry agents".to_owned());
        Ok(vec!["io.amicu.telemetry.a1b2c3d4.support_local".to_owned()])
    }
    async fn start(
        &self,
        plan: &amicu_runtime::LaunchPlan,
        _: &[amicu_llamacpp::ResolvedSecret],
    ) -> Result<amicu_runtime::RuntimeHandle, amicu_llamacpp::LlamaCppError> {
        if self.start_fails {
            return Err(amicu_llamacpp::LlamaCppError::Supervisor(
                "model file not found".to_owned(),
            ));
        }
        let id = amicu_llamacpp::host_label(plan.deployment.as_str());
        self.log.started.lock().unwrap().push(id.clone());
        self.log.fire("start");
        self.log
            .events
            .lock()
            .unwrap()
            .push(format!("start host {id}"));
        Ok(amicu_runtime::RuntimeHandle {
            id,
            deployment: plan.deployment.clone(),
            endpoint: "http://127.0.0.1:8000".to_owned(),
            exposure: amicu_runtime::EndpointExposure::Loopback,
        })
    }
    async fn status(
        &self,
        _: &amicu_runtime::RuntimeHandle,
    ) -> Result<amicu_runtime::RuntimeStatus, amicu_llamacpp::LlamaCppError> {
        unreachable!()
    }
    async fn metrics(
        &self,
        _: &amicu_runtime::RuntimeHandle,
    ) -> Result<amicu_runtime::RuntimeMetrics, amicu_llamacpp::LlamaCppError> {
        unreachable!()
    }
    async fn logs(
        &self,
        _: &amicu_runtime::RuntimeHandle,
        _: amicu_runtime::LogOptions,
    ) -> Result<amicu_runtime::LogStream, amicu_llamacpp::LlamaCppError> {
        unreachable!()
    }
    async fn stop(
        &self,
        h: &amicu_runtime::RuntimeHandle,
    ) -> Result<(), amicu_llamacpp::LlamaCppError> {
        if self.fail {
            self.log
                .events
                .lock()
                .unwrap()
                .push(format!("stop host failed {}", h.id));
            return Err(amicu_llamacpp::LlamaCppError::Supervisor(
                "stop could not be confirmed".to_owned(),
            ));
        }
        self.log.stopped.lock().unwrap().push(h.id.clone());
        self.log.fire("stop");
        self.log
            .events
            .lock()
            .unwrap()
            .push(format!("stop host {}", h.id));
        Ok(())
    }
    async fn stop_recorded(
        &self,
        h: &amicu_runtime::RuntimeHandle,
        definition: Option<&std::path::Path>,
    ) -> Result<(), amicu_llamacpp::LlamaCppError> {
        self.stop(h).await?;
        // Like the real managers: delete the definition at the recorded path.
        if let Some(path) = definition {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }
    fn host_context(&self) -> Option<amicu_llamacpp::HostContext> {
        self.log.fire("ctx");
        self.ctx.clone()
    }
    fn instance_owner(&self, _label: &str) -> Option<u32> {
        self.owner
    }
    fn boot_hint(&self) -> Option<String> {
        Some("note: fake lingering hint".to_owned())
    }
}

/// A scripted Modal: which workspace the token is for, which apps exist by
/// name, and which app ids Modal reports as unknown.
struct FakeModal {
    workspace: Workspace,
    apps: Mutex<Vec<(String, String)>>,
    unknown_ids: Vec<String>,
    stopped: Mutex<Vec<String>>,
    live: Option<Vec<String>>,
    /// App ids the token's workspace can see (besides `apps`).
    visible_ids: Vec<String>,
    /// Deployments whose host service the fake service manager reports down.
    host_down: Vec<String>,
    /// The fake service manager cannot answer (bus error, unknown state).
    host_unanswered: bool,
    /// `stop_app` errors, as an unreachable control plane would.
    stop_fails: bool,
    /// Shared with the host fake so cross-provider ordering is observable.
    log: Arc<StopLog>,
}

impl Default for FakeModal {
    fn default() -> Self {
        Self {
            workspace: ws("acme"),
            apps: Mutex::new(Vec::new()),
            unknown_ids: Vec::new(),
            stopped: Mutex::new(Vec::new()),
            live: None,
            visible_ids: Vec::new(),
            host_down: Vec::new(),
            host_unanswered: false,
            stop_fails: false,
            log: Arc::default(),
        }
    }
}

fn ws(name: &str) -> Workspace {
    Workspace {
        name: name.to_owned(),
        environment: "default".to_owned(),
    }
}

#[async_trait::async_trait]
impl ModalOps for FakeModal {
    async fn workspace(&self) -> Result<Workspace, crate::error::CliError> {
        Ok(self.workspace.clone())
    }
    async fn list_app_ids(&self) -> Result<Vec<String>, crate::error::CliError> {
        Ok(self.visible())
    }
    async fn find_app(&self, name: &str) -> Result<Option<String>, crate::error::CliError> {
        Ok(self
            .apps
            .lock()
            .unwrap()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| id.clone()))
    }
    async fn latest_app_version(&self, _: &str) -> Result<Option<u32>, crate::error::CliError> {
        Ok(None)
    }
    async fn rollback_app(&self, _: &str, _: u32) -> Result<(), crate::error::CliError> {
        Ok(())
    }
    async fn stop_app(&self, app_id: &str) -> Result<bool, crate::error::CliError> {
        if self.stop_fails {
            return Err(crate::error::CliError::Message(
                "modal stop could not be confirmed".to_owned(),
            ));
        }
        if self.unknown_ids.iter().any(|i| i == app_id) {
            return Ok(false);
        }
        self.stopped.lock().unwrap().push(app_id.to_owned());
        self.log
            .events
            .lock()
            .unwrap()
            .push(format!("stop modal {app_id}"));
        Ok(true)
    }
}

impl FakeModal {
    fn visible(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .apps
            .lock()
            .unwrap()
            .iter()
            .map(|(_, id)| id.clone())
            .collect();
        v.extend(self.visible_ids.clone());
        v
    }
}

impl AppLookup for FakeModal {
    fn app_ids(&self) -> Result<Option<std::collections::HashSet<String>>, String> {
        Ok(Some(self.visible().into_iter().collect()))
    }
    fn live_apps(&self, app_names: &[String]) -> Result<std::collections::HashSet<String>, String> {
        Ok(self
            .live
            .clone()
            .unwrap_or_else(|| app_names.to_vec())
            .into_iter()
            .collect())
    }
    fn active_workspace(&self) -> Result<Option<Workspace>, String> {
        Ok(Some(self.workspace.clone()))
    }
    fn host_unit(&self, record: &StopRecord) -> Result<crate::refresh::HostUnit, String> {
        use crate::refresh::HostUnit;
        if self.host_unanswered {
            return Err("Failed to connect to bus".to_owned());
        }
        Ok(
            if self
                .host_down
                .iter()
                .any(|d| d == record.deployment.as_str())
            {
                HostUnit::Down
            } else {
                HostUnit::Running
            },
        )
    }
}

/// Telemetry hooks with consent decided by an injected environment, and a
/// record of what was cleared and forgotten.
#[derive(Default)]
struct FakeTelemetry {
    env: Vec<(&'static str, &'static str)>,
    stored: Option<bool>,
    cleared: std::sync::atomic::AtomicBool,
    forgotten: Mutex<Vec<String>>,
}

impl crate::commands::telemetry_agent::TelemetryHooks for FakeTelemetry {
    fn consent_on(&self) -> bool {
        let env = |k: &str| {
            self.env
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        };
        crate::commands::telemetry::effective(&env, self.stored).enabled
    }
    fn planner(
        &self,
        _: &amicu_core::graph::Graph,
        _: &dyn amicu_llamacpp::Catalog,
        _: &crate::planner::PlanResult,
    ) -> Option<Arc<dyn amicu_llamacpp::SidecarPlanner>> {
        None
    }
    fn clear_queues(&self) {
        self.cleared
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn forget(&self, main_label: &str) {
        self.forgotten.lock().unwrap().push(main_label.to_owned());
    }
}

struct Rig {
    log: Arc<StopLog>,
    telemetry: FakeTelemetry,
    host: Box<crate::commands::apply::HostSupervisorFactory>,
    modal: FakeModal,
    runpod: runpod::FakeRunPod,
    resolver: Box<dyn amicu_core::secret::SecretResolver>,
    forget: bool,
    purge: bool,
}

impl Rig {
    fn new() -> Self {
        Self::build(false, Some(platform_ctx()), None)
    }
    fn failing() -> Self {
        Self::build(true, Some(platform_ctx()), None)
    }
    fn with_ctx(ctx: amicu_llamacpp::HostContext) -> Self {
        Self::build(false, Some(ctx), None)
    }
    fn no_ctx() -> Self {
        Self::build(false, None, None)
    }
    fn with_owner(owner: u32) -> Self {
        Self::build(false, Some(platform_ctx()), Some(owner))
    }
    fn failing_start() -> Self {
        let mut rig = Self::new();
        let log = rig.log.clone();
        rig.host = Box::new(move |_| {
            Box::new(FakeHost {
                log: log.clone(),
                fail: false,
                start_fails: true,
                ctx: Some(platform_ctx()),
                owner: None,
            })
        });
        rig
    }
    fn build(fail: bool, ctx: Option<amicu_llamacpp::HostContext>, owner: Option<u32>) -> Self {
        let log = Arc::new(StopLog::default());
        let l = log.clone();
        let modal = FakeModal {
            log: log.clone(),
            ..FakeModal::default()
        };
        Self {
            log,
            host: Box::new(move |_| {
                Box::new(FakeHost {
                    log: l.clone(),
                    fail,
                    start_fails: false,
                    ctx: ctx.clone(),
                    owner,
                })
            }),
            modal,
            runpod: runpod::FakeRunPod::default(),
            resolver: Box::new(crate::secret::EnvSecretResolver),
            forget: false,
            purge: false,
            // Opted in: the opt-out sweep stays out of unrelated tests.
            telemetry: FakeTelemetry {
                stored: Some(true),
                ..FakeTelemetry::default()
            },
        }
    }
    fn env(&self) -> Env<'_> {
        Env {
            host: &*self.host,
            modal: &self.modal,
            runpod: &self.runpod,
            resolver: &*self.resolver,
            forget_unconfirmed: self.forget,
            purge: self.purge,
            proxy_tokens: amicu_modal::proxy_auth::TokenConfig::from_env,
            interactive: true,
            secret_is_set: |_| true,
            telemetry: &self.telemetry,
            progress: |_| {},
        }
    }
    fn stopped(&self) -> Vec<String> {
        let mut v = self.log.stopped.lock().unwrap().clone();
        v.sort();
        v
    }
}

/// The identity of the machine the tests run on, as a user 501 would see it.
fn platform_ctx() -> amicu_llamacpp::HostContext {
    if cfg!(target_os = "macos") {
        amicu_llamacpp::HostContext {
            backend: amicu_llamacpp::HostBackend::Launchd,
            uid: Some(501),
            sid: None,
            domain: Some("gui/501".to_owned()),
        }
    } else if cfg!(windows) {
        amicu_llamacpp::HostContext {
            backend: amicu_llamacpp::HostBackend::TaskScheduler,
            uid: None,
            sid: Some("S-1-5-21-1-1001".to_owned()),
            domain: None,
        }
    } else {
        amicu_llamacpp::HostContext {
            backend: amicu_llamacpp::HostBackend::Systemd,
            uid: Some(501),
            sid: None,
            domain: None,
        }
    }
}

fn run_apply(
    project: &Project,
    output: OutputFormat,
    out: &mut Vec<u8>,
) -> Result<u8, crate::error::CliError> {
    let rig = Rig::new();
    commands::apply::run_with(
        project,
        true,
        None,
        output,
        out,
        &mut std::io::Cursor::new(Vec::new()),
        &rig.env(),
    )
}

fn state_with(graph: amicu_core::graph::Graph) -> LocalState {
    LocalState {
        version: LocalState::VERSION.to_owned(),
        project_id: None,
        graph,
        endpoints: Vec::new(),
        pending: Vec::new(),
        stops: Vec::new(),
        fingerprints: std::collections::BTreeMap::default(),
        serial: 0,
        purge_pending: Vec::new(),
        extra: std::collections::BTreeMap::default(),
    }
}

fn run_destroy(
    project: &Project,
    targets: &[String],
    auto_approve: bool,
    output: OutputFormat,
    answer: &str,
    rig: &Rig,
) -> (Result<u8, crate::error::CliError>, String) {
    let mut out = Vec::new();
    let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());
    let result = commands::destroy::run_with(
        project,
        targets,
        auto_approve,
        None,
        output,
        &mut out,
        &mut input,
        &rig.env(),
    );
    (result, utf8(out))
}

fn label(name: &str) -> String {
    amicu_llamacpp::host_label(name)
}

fn host_rec(name: &str) -> StopRecord {
    StopRecord::host(&name.into(), None)
}

fn modal_rec(name: &str, app_id: Option<&str>, workspace: Option<&str>) -> StopRecord {
    StopRecord::modal(
        &name.into(),
        app_id.map(str::to_owned),
        workspace.map(ws).as_ref(),
    )
}

fn stops_for(project: &Project, name: &str) -> Vec<StopRecord> {
    project
        .load_state()
        .unwrap()
        .unwrap()
        .stops
        .into_iter()
        .filter(|r| r.deployment.as_str() == name)
        .collect()
}

fn saved(td: &TempDir, state: &LocalState) -> Project {
    let project = Project::new(&td.path);
    project.save_state(state).unwrap();
    project
}

#[test]
fn applying_a_removal_stops_via_the_fake_host_not_the_real_one() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let prior = project.load_graph().unwrap();
    project.save_state(&state_with(prior)).unwrap();
    write(&td.path, "amicu.hcl", SECOND_MODEL);

    let rig = Rig::new();
    let mut out = Vec::new();
    let code = commands::apply::run_with(
        &project,
        true,
        None,
        OutputFormat::Json,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &rig.env(),
    )
    .unwrap();
    assert_eq!(code, 0);
    assert_eq!(rig.stopped(), [label("support_local")]);
    let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(value.is_array());
}

#[test]
#[should_panic(expected = "inject a fake service manager")]
fn the_real_host_exec_refuses_service_commands_under_test() {
    let _ = amicu_llamacpp::CommandExec::exec(&amicu_llamacpp::HostExec, "launchctl", &["list"]);
}

#[test]
fn destroy_unknown_target_leaves_state_untouched() {
    let td = TempDir::new();
    let state = state_with(two_deployment_graph());
    let project = saved(&td, &state);
    let rig = Rig::new();
    let (result, out) = run_destroy(
        &project,
        &["ghost".to_owned()],
        true,
        OutputFormat::Human,
        "",
        &rig,
    );
    let err = result.expect_err("unknown target");
    assert!(err.to_string().contains("ghost"), "{err}");
    assert_eq!(project.load_state().unwrap().unwrap().graph, state.graph);
    assert_eq!(out, "");
    assert_eq!(rig.stopped().len(), 0);
}

#[test]
fn destroy_with_empty_state_prints_nothing_to_destroy() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    let (result, out) = run_destroy(&project, &[], true, OutputFormat::Human, "", &Rig::new());
    assert_eq!(result.unwrap(), 0);
    assert_eq!(out, "Nothing to destroy.\n");
    assert!(project.load_state().unwrap().is_none());
}

#[test]
fn destroy_nothing_in_json_mode_is_an_empty_array() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    let (result, out) = run_destroy(&project, &[], true, OutputFormat::Json, "", &Rig::new());
    result.unwrap();
    assert_eq!(out.trim(), "[]");
}

#[test]
fn destroy_cancel_or_eof_destroys_nothing_and_json_prints_an_empty_array() {
    for answer in ["", "n\n", "no\n", "\n"] {
        for output in [OutputFormat::Human, OutputFormat::Json] {
            let td = TempDir::new();
            let state = state_with(two_deployment_graph());
            let project = saved(&td, &state);
            let rig = Rig::new();
            let (result, out) = run_destroy(&project, &[], false, output, answer, &rig);
            assert_eq!(result.unwrap(), 0);
            if output.is_json() {
                assert_eq!(out.trim(), "[]", "{out}");
            } else {
                assert!(out.contains("Destroy these deployments? [y/N]"), "{out}");
                assert!(out.contains("Destroy cancelled."), "{out}");
            }
            assert_eq!(project.load_state().unwrap().unwrap().graph, state.graph);
            assert_eq!(rig.stopped().len(), 0);
        }
    }
}

#[test]
fn destroy_confirmed_stops_via_derived_labels_and_empties_state() {
    let td = TempDir::new();
    // v1-style state: no endpoints, no stop records. Identity is derived.
    let project = saved(&td, &state_with(two_deployment_graph()));
    let rig = Rig::new();
    let (result, out) = run_destroy(&project, &[], false, OutputFormat::Human, "y\n", &rig);
    assert_eq!(result.unwrap(), 0);
    assert!(out.contains("Destroyed 2 deployment(s):"), "{out}");
    assert_eq!(
        rig.stopped(),
        [label("support_local"), label("support_two")]
    );
    let after = project.load_state().unwrap().unwrap();
    assert_eq!(after.graph.deployments.len(), 0);
    assert_eq!(after.stops.len(), 0);
}

#[test]
fn targeted_destroy_preserves_other_deployments_endpoints_and_stop_records() {
    let td = TempDir::new();
    let kept = handle("support_local", 8000);
    let mut state = state_with(two_deployment_graph());
    state.endpoints = vec![kept.clone()];
    let project = saved(&td, &state);
    let rig = Rig::new();
    let (result, _) = run_destroy(
        &project,
        &["support_two".to_owned()],
        true,
        OutputFormat::Human,
        "",
        &rig,
    );
    result.unwrap();
    assert_eq!(rig.stopped(), [label("support_two")]);
    let after = project.load_state().unwrap().unwrap();
    assert_eq!(after.endpoints, vec![kept]);
    let ids: Vec<&str> = after
        .graph
        .deployments
        .iter()
        .map(|d| d.id.as_str())
        .collect();
    assert_eq!(ids, ["support_local"]);
    assert!(
        after
            .stops
            .iter()
            .all(|r| r.deployment.as_str() == "support_local")
    );
}

#[test]
fn a_failed_stop_keeps_the_deployment_and_its_record_and_errors_with_a_hint() {
    let td = TempDir::new();
    let mut state = state_with(two_deployment_graph());
    state.stops = vec![host_rec("support_local"), host_rec("support_two")];
    let project = saved(&td, &state);
    let rig = Rig::failing();
    let (result, out) = run_destroy(&project, &[], true, OutputFormat::Human, "", &rig);
    let err = result.expect_err("an unconfirmed stop must fail the command");
    let msg = err.to_string();
    assert!(msg.contains("could not be confirmed"), "{msg}");
    // First-stop failure: nothing changed, and the hint names the command.
    assert!(msg.contains("amicu destroy"), "{msg}");
    assert!(!out.contains("Destroyed"), "never claim success: {out}");
    let after = project.load_state().unwrap().unwrap();
    assert_eq!(after.graph.deployments.len(), 2);
    assert_eq!(after.stops, state.stops);
}

/// A crash between the provider start and the final save leaves only the
/// write-ahead record: a pending deployment with its stop record and no
/// endpoint. Destroy stops it from that record alone.
#[test]
fn destroy_stops_a_pending_deployment_from_its_record_alone() {
    let td = TempDir::new();
    let mut state = state_with(two_deployment_graph());
    state.pending = vec!["support_two".into()];
    state.stops = vec![host_rec("support_two")];
    let project = saved(&td, &state);
    let rig = Rig::new();
    let (result, _) = run_destroy(
        &project,
        &["support_two".to_owned()],
        true,
        OutputFormat::Human,
        "",
        &rig,
    );
    result.unwrap();
    assert_eq!(rig.stopped(), [label("support_two")]);
    let after = project.load_state().unwrap().unwrap();
    assert_eq!(after.pending.len(), 0);
    assert!(
        after
            .stops
            .iter()
            .all(|r| r.deployment.as_str() == "support_local"),
        "{:?}",
        after.stops
    );
}

#[test]
fn destroy_stops_every_recorded_identity_of_a_deployment() {
    let td = TempDir::new();
    let mut state = state_with(two_deployment_graph());
    let mut old = host_rec("support_two");
    old.identity = "io.amicu.llama.old_name".to_owned();
    state.stops = vec![old, host_rec("support_two")];
    let project = saved(&td, &state);
    let rig = Rig::new();
    let (result, _) = run_destroy(
        &project,
        &["support_two".to_owned()],
        true,
        OutputFormat::Human,
        "",
        &rig,
    );
    result.unwrap();
    assert_eq!(
        rig.stopped(),
        ["io.amicu.llama.old_name".to_owned(), label("support_two")]
    );
    // Each confirmed stop also drops that instance's agent id and queue.
    assert_eq!(
        *rig.telemetry.forgotten.lock().unwrap(),
        ["io.amicu.llama.old_name".to_owned(), label("support_two")]
    );
}

/// Stop records whose deployment is no longer in the graph are still
/// instances that may be running: destroy plans and stops them too, and they
/// are valid `--target`s.
#[test]
fn destroy_acts_on_orphan_stop_records() {
    let td = TempDir::new();
    let mut state = state_with(two_deployment_graph());
    state.stops = vec![host_rec("ghost")];
    let project = saved(&td, &state);

    let rig = Rig::new();
    let (result, _) = run_destroy(
        &project,
        &["ghost".to_owned()],
        true,
        OutputFormat::Human,
        "",
        &rig,
    );
    result.unwrap();
    assert_eq!(rig.stopped(), [label("ghost")]);
    let after = project.load_state().unwrap().unwrap();
    assert_eq!(after.graph.deployments.len(), 2, "others untouched");
    assert!(after.stops.iter().all(|r| r.deployment.as_str() != "ghost"));
}

// ---- host identity: backend, uid, launchd domain ----------------------------

fn ctx(
    backend: amicu_llamacpp::HostBackend,
    uid: u32,
    domain: Option<&str>,
) -> amicu_llamacpp::HostContext {
    amicu_llamacpp::HostContext {
        backend,
        uid: Some(uid),
        sid: None,
        domain: domain.map(str::to_owned),
    }
}

fn destroy_one_host_record(
    rec: StopRecord,
    rig: &Rig,
) -> (Result<u8, crate::error::CliError>, Project, TempDir) {
    let td = TempDir::new();
    let mut state = state_with(two_deployment_graph());
    state.stops = vec![rec];
    let project = saved(&td, &state);
    let (result, _) = run_destroy(
        &project,
        &["support_two".to_owned()],
        true,
        OutputFormat::Human,
        "",
        rig,
    );
    (result, project, td)
}

#[test]
fn host_records_remember_uid_and_launchd_domain() {
    let c = ctx(amicu_llamacpp::HostBackend::Launchd, 501, Some("gui/501"));
    let rec = StopRecord::host(&"support_two".into(), Some(&c));
    assert_eq!(rec.backend, Backend::HostLaunchd);
    assert_eq!(rec.uid, Some(501));
    assert_eq!(rec.domain.as_deref(), Some("gui/501"));
    let back: StopRecord = serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
    assert_eq!(back, rec);
}

#[test]
fn a_matching_uid_and_domain_stops_and_an_old_record_without_them_is_accepted() {
    let c = ctx(amicu_llamacpp::HostBackend::Launchd, 501, Some("gui/501"));
    let rec = StopRecord::host(&"support_two".into(), Some(&c));
    let (result, _, _td) = destroy_one_host_record(rec, &Rig::with_ctx(c.clone()));
    result.unwrap();

    // Old record: backend only, no uid/domain.
    let mut old = host_rec("support_two");
    old.backend = Backend::HostLaunchd;
    let (result, _, _td) = destroy_one_host_record(old, &Rig::with_ctx(c));
    result.unwrap();
}

#[test]
fn a_different_uid_domain_or_backend_is_refused_and_nothing_is_stopped() {
    let started = ctx(amicu_llamacpp::HostBackend::Launchd, 501, Some("gui/501"));
    let cases = [
        (
            "uid",
            ctx(amicu_llamacpp::HostBackend::Launchd, 502, Some("gui/501")),
        ),
        (
            "domain",
            ctx(amicu_llamacpp::HostBackend::Launchd, 501, Some("user/501")),
        ),
        (
            "backend",
            ctx(amicu_llamacpp::HostBackend::Systemd, 501, None),
        ),
    ];
    for (what, now) in cases {
        let rec = StopRecord::host(&"support_two".into(), Some(&started));
        let rig = Rig::with_ctx(now);
        let (result, project, _td) = destroy_one_host_record(rec.clone(), &rig);
        let err = result.expect_err(what);
        assert!(
            err.to_string().contains("Nothing was changed"),
            "{what}: {err}"
        );
        assert_eq!(rig.stopped().len(), 0, "{what}");
        let after = project.load_state().unwrap().unwrap();
        assert_eq!(after.stops, vec![rec], "{what}: record kept");
    }
}

fn task_ctx(sid: &str) -> amicu_llamacpp::HostContext {
    amicu_llamacpp::HostContext {
        backend: amicu_llamacpp::HostBackend::TaskScheduler,
        uid: None,
        sid: Some(sid.to_owned()),
        domain: None,
    }
}

#[test]
fn task_scheduler_records_round_trip_and_older_records_still_load() {
    assert_eq!(Backend::HostTaskScheduler.name(), "task-scheduler");
    assert_eq!(
        serde_json::to_string(&Backend::HostTaskScheduler).unwrap(),
        "\"host_task_scheduler\""
    );
    let rec = StopRecord::host(&"support_two".into(), Some(&task_ctx("S-1-5-21-1-1001")));
    assert_eq!(rec.backend, Backend::HostTaskScheduler);
    assert_eq!(rec.uid, None);
    assert_eq!(rec.sid.as_deref(), Some("S-1-5-21-1-1001"));
    let json = serde_json::to_string(&rec).unwrap();
    assert!(!json.contains("\"uid\""), "{json}");
    let back: StopRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, rec);
    // Unix records never had a sid: they load unchanged and save without one.
    let unix = StopRecord::host(
        &"support_two".into(),
        Some(&ctx(amicu_llamacpp::HostBackend::Systemd, 501, None)),
    );
    let json = serde_json::to_string(&unix).unwrap();
    assert!(!json.contains("sid"), "{json}");
    let back: StopRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back.uid, Some(501));
    assert_eq!(back.sid, None);
}

#[test]
fn task_scheduler_recorded_files_are_validated() {
    let mut rec = StopRecord::host(&"support_two".into(), Some(&task_ctx("S-1-5-21-1-1001")));
    let id = rec.identity.clone();
    let base = "/Users/me/AppData/Local/amicu";
    rec.definition = Some(format!("{base}/tasks/{id}.xml"));
    rec.env_file = Some(format!("{base}/env/{id}.env"));
    rec.logs = vec![format!("{base}/logs/{id}.out.log")];
    if std::env::var_os("LOCALAPPDATA").is_none() {
        // Without LOCALAPPDATA the anchor is unknown: everything is refused.
        assert!(rec.definition_path().is_err());
        assert!(rec.env_path().is_err());
        assert!(rec.log_paths().is_err());
    }

    let mut bad = rec.clone();
    bad.definition = Some(format!("/Users/me/Documents/{id}.xml"));
    assert!(bad.definition_path().is_err());
    let mut bad = rec.clone();
    bad.env_file = Some(format!("{base}/tasks/{id}.env"));
    assert!(bad.env_path().is_err());
    let mut bad = rec;
    bad.logs = vec![format!("{base}/logs/other.out.log")];
    assert!(bad.log_paths().is_err());
}

#[test]
fn task_scheduler_files_are_anchored_to_the_managers_directory() {
    use crate::stops::check_anchored;
    use std::path::Path;
    let base = Path::new(r"C:\Users\me\AppData\Local\amicu");
    let ok = |p: &str, sub: &str| check_anchored(Path::new(p), Some(base), sub).is_ok();
    assert!(ok(r"C:\Users\me\AppData\Local\amicu\tasks\x.xml", "tasks"));
    // Case and separators do not matter on Windows.
    assert!(ok(r"c:/users/ME/appdata/local/AMICU/tasks/x.xml", "tasks"));
    // The same suffix under another profile, drive or share is refused.
    assert!(!ok(
        r"C:\Users\other\AppData\Local\amicu\tasks\x.xml",
        "tasks"
    ));
    assert!(!ok(r"D:\Users\me\AppData\Local\amicu\tasks\x.xml", "tasks"));
    assert!(!ok(
        r"\\server\share\AppData\Local\amicu\tasks\x.xml",
        "tasks"
    ));
    assert!(!ok(r"C:\Users\me\AppData\Local\amicu\env\x.xml", "tasks"));
    assert!(check_anchored(Path::new("/x/amicu/tasks/x.xml"), None, "tasks").is_err());
}

#[test]
fn a_task_scheduler_record_of_another_windows_user_is_refused() {
    let rec = StopRecord::host(&"support_two".into(), Some(&task_ctx("S-1-5-21-1-1001")));
    let rig = Rig::with_ctx(task_ctx("S-1-5-21-1-1002"));
    let (result, project, _td) = destroy_one_host_record(rec.clone(), &rig);
    let err = result.expect_err("other user").to_string();
    assert!(err.contains("Windows user S-1-5-21-1-1001"), "{err}");
    assert_eq!(rig.stopped().len(), 0);
    let after = project.load_state().unwrap().unwrap();
    assert_eq!(after.stops, vec![rec]);

    // The same user stops it.
    let rec = StopRecord::host(&"support_two".into(), Some(&task_ctx("S-1-5-21-1-1001")));
    let rig = Rig::with_ctx(task_ctx("S-1-5-21-1-1001"));
    let (result, _, _td) = destroy_one_host_record(rec, &rig);
    result.unwrap();
    assert_eq!(rig.stopped().len(), 1);
}

#[test]
fn launchd_and_systemd_records_are_not_the_same_target() {
    let mut launchd = host_rec("support_two");
    launchd.backend = Backend::HostLaunchd;
    let mut systemd = host_rec("support_two");
    systemd.backend = Backend::HostSystemd;
    assert!(!launchd.same_target(&systemd));
    assert!(launchd.same_target(&launchd.clone()));
}

// ---- Modal: workspace, app ids, unconfirmed names ---------------------------

fn modal_graph() -> amicu_core::graph::Graph {
    amicu_hcl::parse_graph(&two_deployments().replace("local_apple", "modal")).unwrap()
}

fn destroy_modal(
    rec: StopRecord,
    rig: &Rig,
) -> (Result<u8, crate::error::CliError>, Project, TempDir) {
    let td = TempDir::new();
    let mut state = state_with(modal_graph());
    state.stops = vec![rec];
    let project = saved(&td, &state);
    let (result, _) = run_destroy(
        &project,
        &["support_two".to_owned()],
        true,
        OutputFormat::Human,
        "",
        rig,
    );
    (result, project, td)
}

#[test]
fn modal_destroy_stops_by_recorded_app_id_in_the_matching_workspace() {
    let rig = Rig::new();
    let (result, project, _td) =
        destroy_modal(modal_rec("support_two", Some("ap-2"), Some("acme")), &rig);
    result.unwrap();
    assert_eq!(*rig.modal.stopped.lock().unwrap(), ["ap-2"]);
    assert_eq!(stops_for(&project, "support_two").len(), 0);
}

#[test]
fn modal_not_found_for_a_recorded_id_is_confirmed_only_in_the_matching_workspace() {
    let mut rig = Rig::new();
    rig.modal.unknown_ids = vec!["ap-2".to_owned()];
    // Same workspace: Modal says it is gone, so it is confirmed.
    let (result, project, _td) =
        destroy_modal(modal_rec("support_two", Some("ap-2"), Some("acme")), &rig);
    result.unwrap();
    assert_eq!(stops_for(&project, "support_two").len(), 0);

    // Another workspace: refused before asking Modal, and the record is kept.
    let rec = modal_rec("support_two", Some("ap-2"), Some("other-co"));
    let (result, project, _td) = destroy_modal(rec.clone(), &rig);
    let msg = result.expect_err("workspace mismatch").to_string();
    assert!(msg.contains("other-co") && msg.contains("acme"), "{msg}");
    assert!(msg.contains("MODAL_TOKEN_ID"), "{msg}");
    assert_eq!(stops_for(&project, "support_two"), vec![rec]);
}

#[test]
fn modal_name_only_record_that_cannot_be_found_is_kept_unless_forgotten() {
    let rig = Rig::new();
    let rec = modal_rec("support_two", None, Some("acme"));
    let (result, project, _td) = destroy_modal(rec.clone(), &rig);
    let msg = result.expect_err("unconfirmed").to_string();
    assert!(msg.contains("--forget-unconfirmed"), "{msg}");
    assert!(!msg.contains("state.json"), "no hand-editing advice: {msg}");
    assert_eq!(
        project.load_state().unwrap().unwrap().stops,
        vec![rec.clone()]
    );

    // Found by name: stopped by the id Modal reports.
    rig.modal
        .apps
        .lock()
        .unwrap()
        .push(("amicu-support-two".to_owned(), "ap-found".to_owned()));
    let (result, _, _td) = destroy_modal(rec.clone(), &rig);
    result.unwrap();
    assert_eq!(*rig.modal.stopped.lock().unwrap(), ["ap-found"]);

    // The user checked the dashboard and opts in.
    let mut rig = Rig::new();
    rig.forget = true;
    let (result, project, _td) = destroy_modal(rec, &rig);
    result.unwrap();
    assert_eq!(stops_for(&project, "support_two").len(), 0);
}

#[test]
fn a_workspace_less_modal_record_is_stopped_only_once_its_app_is_proven_visible() {
    // Proven: the active workspace lists the app id, so it is stopped and
    // the record (stamped with the workspace) is then removed.
    let mut rig = Rig::new();
    rig.modal.visible_ids = vec!["ap-2".to_owned()];
    let (result, project, _td) = destroy_modal(modal_rec("support_two", Some("ap-2"), None), &rig);
    result.unwrap();
    assert_eq!(*rig.modal.stopped.lock().unwrap(), ["ap-2"]);
    assert_eq!(stops_for(&project, "support_two").len(), 0);

    // Not visible: ownership is unproven, so nothing is stopped or dropped,
    // even when Modal would say "not found".
    let mut rig = Rig::new();
    rig.modal.unknown_ids = vec!["ap-2".to_owned()];
    let rec = modal_rec("support_two", Some("ap-2"), None);
    let (result, project, _td) = destroy_modal(rec.clone(), &rig);
    let msg = result.expect_err("unproven").to_string();
    assert!(msg.contains("cannot see that app"), "{msg}");
    assert!(msg.contains("MODAL_TOKEN_ID"), "{msg}");
    assert!(!msg.contains("MODAL_ENVIRONMENT"), "{msg}");
    assert_eq!(rig.modal.stopped.lock().unwrap().len(), 0);
    assert_eq!(stops_for(&project, "support_two"), vec![rec]);
}

/// A v1 state (Modal handle, no stop records) under a token for another
/// workspace: both apply and destroy refuse before doing anything, so there is
/// no duplicate deploy and nothing is dropped.
#[test]
fn v1_modal_state_under_another_workspaces_token_blocks_apply_and_destroy() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let mut state = state_with(modal_graph());
    state.endpoints = vec![handle("support_local", 8000), handle("support_two", 8001)];
    for h in &mut state.endpoints {
        h.id = format!("ap-{}", h.deployment);
        h.exposure = amicu_runtime::EndpointExposure::Loopback;
    }
    project.save_state(&state).unwrap();
    let before = std::fs::read_to_string(project.state_file()).unwrap();

    let mut rig = Rig::new();
    rig.modal.workspace = ws("other-co");
    // The other workspace cannot see those app ids.
    let mut out = Vec::new();
    let err = commands::apply::run_with(
        &project,
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &rig.env(),
    )
    .expect_err("apply must refuse");
    assert!(err.to_string().contains("cannot see that app"), "{err}");

    let (result, _) = run_destroy(&project, &[], true, OutputFormat::Human, "", &rig);
    let err = result.expect_err("destroy must refuse");
    assert!(err.to_string().contains("cannot see that app"), "{err}");

    assert_eq!(rig.modal.stopped.lock().unwrap().len(), 0);
    assert_eq!(rig.stopped().len(), 0);
    assert_eq!(
        std::fs::read_to_string(project.state_file()).unwrap(),
        before,
        "state untouched: nothing dropped"
    );
}

#[test]
fn forget_unconfirmed_needs_explicit_targets() {
    let td = TempDir::new();
    let mut state = state_with(modal_graph());
    state.stops = vec![modal_rec("support_two", None, Some("acme"))];
    let project = saved(&td, &state);
    let mut rig = Rig::new();
    rig.forget = true;
    let (result, _) = run_destroy(&project, &[], true, OutputFormat::Human, "", &rig);
    let msg = result.expect_err("no targets").to_string();
    assert!(msg.contains("--target"), "{msg}");
    assert_eq!(stops_for(&project, "support_two").len(), 1);
}

#[test]
fn forget_unconfirmed_is_refused_for_a_record_without_a_known_workspace() {
    let mut rig = Rig::new();
    rig.forget = true;
    let rec = modal_rec("support_two", None, None);
    let (result, project, _td) = destroy_modal(rec.clone(), &rig);
    let msg = result.expect_err("unknown workspace").to_string();
    // Dead end: it points at the targeted recovery, not at the flag that is
    // refused here.
    assert!(msg.contains("recorded with no workspace"), "{msg}");
    assert!(msg.contains("modal app list"), "{msg}");
    assert!(msg.contains("amicu modal stop amicu-support-two"), "{msg}");
    assert!(!msg.contains("--forget-unconfirmed"), "{msg}");
    assert_eq!(stops_for(&project, "support_two"), vec![rec]);
}

#[test]
fn forget_unconfirmed_lists_what_it_will_forget_and_needs_confirmation() {
    let td = TempDir::new();
    let mut state = state_with(modal_graph());
    state.stops = vec![modal_rec("support_two", None, Some("acme"))];
    let project = saved(&td, &state);
    let mut rig = Rig::new();
    rig.forget = true;
    // Interactive and declined: nothing is forgotten.
    let (result, out) = run_destroy(
        &project,
        &["support_two".to_owned()],
        false,
        OutputFormat::Human,
        "n\n",
        &rig,
    );
    assert_eq!(result.unwrap(), 0);
    assert!(out.contains("WITHOUT confirming"), "{out}");
    assert!(
        out.contains("support_two") && out.contains("amicu-support-two"),
        "{out}"
    );
    assert_eq!(stops_for(&project, "support_two").len(), 1);
    // Confirmed: forgotten.
    let (result, _) = run_destroy(
        &project,
        &["support_two".to_owned()],
        false,
        OutputFormat::Human,
        "y\n",
        &rig,
    );
    result.unwrap();
    assert_eq!(stops_for(&project, "support_two").len(), 0);
}

// ---- host identity edge cases ------------------------------------------------

#[test]
fn an_unknown_host_identity_refuses_every_stop() {
    let (result, project, _td) = destroy_one_host_record(host_rec("support_two"), &Rig::no_ctx());
    let msg = result.expect_err("unknown context").to_string();
    assert!(msg.contains("cannot determine which user"), "{msg}");
    assert_eq!(stops_for(&project, "support_two").len(), 1);
}

#[cfg(unix)]
#[test]
fn a_record_without_an_owner_is_refused_for_root_or_a_foreign_plist_owner() {
    let mut root = platform_ctx();
    root.uid = Some(0);
    let (result, _, _td) = destroy_one_host_record(host_rec("support_two"), &Rig::with_ctx(root));
    assert!(result.expect_err("root").to_string().contains("root"));

    let (result, _, _td) = destroy_one_host_record(host_rec("support_two"), &Rig::with_owner(999));
    assert!(
        result
            .expect_err("owner")
            .to_string()
            .contains("belongs to uid 999")
    );

    // The plist is ours: fine.
    let (result, _, _td) = destroy_one_host_record(host_rec("support_two"), &Rig::with_owner(501));
    result.unwrap();
}

#[test]
fn a_gui_domain_record_after_logout_gets_a_targeted_recovery_command() {
    let started = ctx(amicu_llamacpp::HostBackend::Launchd, 501, Some("gui/501"));
    let rec = StopRecord::host(&"support_two".into(), Some(&started));
    let now = ctx(amicu_llamacpp::HostBackend::Launchd, 501, Some("user/501"));
    let (result, _, _td) = destroy_one_host_record(rec, &Rig::with_ctx(now));
    let msg = result.expect_err("domain mismatch").to_string();
    assert!(
        msg.contains("launchctl print gui/501/io.amicu.llama.support_two"),
        "{msg}"
    );
    assert!(msg.contains("amicu destroy --target support_two"), "{msg}");
}

#[test]
fn state_round_trips_and_refuses_newer_versions() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    let mut state = state_with(two_deployment_graph());
    state.pending = vec!["support_two".into()];
    state.stops = vec![host_rec("support_two")];
    project.save_state(&state).unwrap();
    let loaded = project.load_state().unwrap().unwrap();
    assert_eq!(loaded.version, "v5");
    assert_eq!(loaded.pending, state.pending);
    assert_eq!(loaded.stops, state.stops);

    // v1 state (no pending/stops) still loads.
    let mut v1: serde_json::Value = serde_json::to_value(&state).unwrap();
    v1["version"] = "v1".into();
    v1.as_object_mut().unwrap().remove("pending");
    v1.as_object_mut().unwrap().remove("stops");
    std::fs::write(project.state_file(), v1.to_string()).unwrap();
    let old = project.load_state().unwrap().unwrap();
    assert_eq!(old.version, "v5", "migrated on load");
    assert_eq!(old.stops.len(), 0);

    // A newer version is refused rather than silently downgraded.
    let mut newer = v1;
    newer["version"] = "v9".into();
    std::fs::write(project.state_file(), newer.to_string()).unwrap();
    let err = project.load_state().expect_err("newer state");
    assert!(err.to_string().contains("v9"), "{err}");
}

fn handle(id: &str, port: u16) -> amicu_runtime::RuntimeHandle {
    amicu_runtime::RuntimeHandle {
        id: id.to_owned(),
        deployment: id.into(),
        endpoint: format!("http://127.0.0.1:{port}"),
        exposure: amicu_runtime::EndpointExposure::Loopback,
    }
}

// ---- project-scoped labels, recorded files, purge (#212, #214) ---------------

/// The directories amicu writes service files to, under `home`.
fn service_dirs(home: &std::path::Path) -> (PathBuf, PathBuf) {
    if cfg!(target_os = "macos") {
        (
            home.join("Library/LaunchAgents"),
            home.join("Library/Logs/amicu"),
        )
    } else {
        (home.join("amicu"), home.join("unused"))
    }
}

fn scoped_rec(project_id: &str, name: &str, home: &std::path::Path) -> StopRecord {
    let label = amicu_llamacpp::project_host_label(project_id, name);
    let (defs, logs) = service_dirs(home);
    let ext = if cfg!(target_os = "macos") {
        "plist"
    } else {
        "env"
    };
    StopRecord::host_in(
        project_id,
        &name.into(),
        Some(&platform_ctx()),
        Some(amicu_llamacpp::ServicePaths {
            definition: defs.join(format!("{label}.{ext}")),
            logs: if cfg!(target_os = "macos") {
                vec![logs.join(format!("{label}.out.log"))]
            } else {
                Vec::new()
            },
            env: None,
        }),
    )
}

#[cfg(unix)]
fn launchd_rec(project_id: &str, name: &str) -> StopRecord {
    let mut rec = StopRecord::host_in(
        project_id,
        &name.into(),
        Some(&amicu_llamacpp::HostContext {
            backend: amicu_llamacpp::HostBackend::Launchd,
            uid: Some(501),
            sid: None,
            domain: Some("gui/501".to_owned()),
        }),
        None,
    );
    let label = rec.identity.clone();
    rec.definition = Some(format!("/Users/me/Library/LaunchAgents/{label}.plist"));
    rec.logs = vec![format!("/Users/me/Library/Logs/amicu/{label}.out.log")];
    rec
}

#[cfg(unix)]
#[test]
fn recorded_definition_and_log_paths_are_validated_against_the_record() {
    let rec = launchd_rec("aaaa1111", "chat");
    assert!(rec.definition_path().unwrap().is_some());
    assert_eq!(rec.log_paths().unwrap().len(), 1);

    let label = rec.identity.clone();
    let with_def = |d: String| {
        let mut r = rec.clone();
        r.definition = Some(d);
        r.definition_path()
    };
    // `..` components.
    assert!(with_def(format!("/Users/me/Library/LaunchAgents/../x/{label}.plist")).is_err());
    assert!(
        with_def(format!(
            "/Users/me/Library/LaunchAgents/../LaunchAgents/{label}.plist"
        ))
        .is_err()
    );
    // Another project's label, or another deployment's.
    assert!(
        with_def("/Users/me/Library/LaunchAgents/io.amicu.llama.bbbb2222.chat.plist".into())
            .is_err()
    );
    // The wrong directory.
    assert!(with_def(format!("/etc/{label}.plist")).is_err());
    assert!(with_def(format!("/Users/me/Documents/{label}.plist")).is_err());
    // An arbitrary file with the right extension, a relative path, a wrong extension.
    assert!(with_def("/Users/me/Library/LaunchAgents/other.plist".into()).is_err());
    assert!(
        with_def(format!("Library/LaunchAgents/{label}.plist").replace("Library", "./Library"))
            .is_err()
    );
    assert!(with_def(format!("/Users/me/Library/LaunchAgents/{label}.env")).is_err());

    let with_log = |l: String| {
        let mut r = rec.clone();
        r.logs = vec![l];
        r.log_paths()
    };
    assert!(
        with_log(format!(
            "/Users/me/Library/Logs/amicu/../../x/{label}.out.log"
        ))
        .is_err()
    );
    assert!(
        with_log("/Users/me/Library/Logs/amicu/io.amicu.llama.bbbb2222.chat.out.log".into())
            .is_err()
    );
    assert!(with_log(format!("/var/log/{label}.out.log")).is_err());
    assert!(with_log("/Users/me/Library/Logs/amicu/notes.txt".into()).is_err());
    assert!(with_log(format!("/Users/me/Library/Logs/amicu/{label}.err.log")).is_ok());
}

#[cfg(unix)]
#[test]
fn systemd_env_files_are_validated_in_the_env_dir_only() {
    let mut rec = StopRecord::host_in(
        "aaaa1111",
        &"chat".into(),
        Some(&amicu_llamacpp::HostContext {
            backend: amicu_llamacpp::HostBackend::Systemd,
            uid: Some(501),
            sid: None,
            domain: None,
        }),
        None,
    );
    let label = rec.identity.clone();
    let mut check = |d: String| {
        rec.definition = Some(d);
        rec.definition_path()
    };
    assert!(check(format!("/run/user/501/amicu/{label}.env")).is_ok());
    assert!(check(format!("/home/me/.local/state/amicu/env/{label}.env")).is_ok());
    assert!(check(format!("/home/me/{label}.env")).is_err());
    assert!(check("/run/user/501/amicu/secrets.env".into()).is_err());
    assert!(check(format!("/run/user/501/amicu/../{label}.env")).is_err());
    // Persistent user units: the unit file in the systemd user dir only.
    assert!(check(format!("/home/me/.config/systemd/user/{label}.service")).is_ok());
    assert!(check(format!("/home/me/.config/systemd/user/{label}.env")).is_err());
    assert!(check(format!("/home/me/{label}.service")).is_err());
    assert!(check("/home/me/.config/systemd/user/sshd.service".into()).is_err());
    assert!(check(format!("/etc/systemd/system/{label}.service")).is_err());
    assert!(check(format!("/home/me/.config/systemd/user/../{label}.service")).is_err());
    // Not just any `systemd/user` directory.
    assert!(check(format!("/tmp/systemd/user/{label}.service")).is_err());
}

#[cfg(unix)]
#[test]
fn the_recorded_systemd_secrets_file_is_validated_before_anything_is_deleted() {
    let mut rec = StopRecord::host_in(
        "aaaa1111",
        &"chat".into(),
        Some(&amicu_llamacpp::HostContext {
            backend: amicu_llamacpp::HostBackend::Systemd,
            uid: Some(501),
            sid: None,
            domain: None,
        }),
        Some(amicu_llamacpp::ServicePaths {
            definition: "/home/me/.config/systemd/user/x.service".into(),
            logs: Vec::new(),
            env: Some("/home/me/.local/state/amicu/env/x.env".into()),
        }),
    );
    // The env path is written ahead with the unit path.
    assert_eq!(
        rec.env_file.as_deref(),
        Some("/home/me/.local/state/amicu/env/x.env")
    );
    let label = rec.identity.clone();
    let mut check = |d: String| {
        rec.env_file = Some(d);
        rec.env_path()
    };
    assert!(check(format!("/home/me/.local/state/amicu/env/{label}.env")).is_ok());
    assert!(check("/home/me/.local/state/amicu/env/other.env".to_owned()).is_err());
    assert!(check(format!("/home/me/.ssh/{label}.env")).is_err());
    assert!(check(format!("/home/me/.local/state/amicu/env/../{label}.env")).is_err());
    assert!(check(format!("relative/{label}.env")).is_err());
    rec.env_file = None;
    assert_eq!(rec.env_path(), Ok(None));
}

#[test]
fn two_projects_with_the_same_deployment_name_get_distinct_labels() {
    let td = TempDir::new();
    let a = scoped_rec("aaaa1111", "chat", &td.path);
    let b = scoped_rec("bbbb2222", "chat", &td.path);
    assert_ne!(a.identity, b.identity);
    assert_eq!(a.identity, "io.amicu.llama.aaaa1111.chat");
    assert!(!a.same_target(&b), "one project must never stop the other");
    assert_ne!(a.definition, b.definition);
    // The legacy, unscoped label is a different target again.
    assert!(!a.same_target(&StopRecord::host(&"chat".into(), None)));
}

#[test]
fn minted_project_ids_differ_per_project() {
    let a = Project::new("/tmp/proj-a").mint_id();
    let b = Project::new("/tmp/proj-b").mint_id();
    assert_eq!(a.len(), 8);
    assert_ne!(a, b);
}

#[test]
fn v2_state_without_project_id_or_files_still_loads() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    let state = state_with(two_deployment_graph());
    project.save_state(&state).unwrap();
    let mut v: serde_json::Value = serde_json::to_value(&state).unwrap();
    v["version"] = "v2".into();
    v.as_object_mut().unwrap().remove("project_id");
    std::fs::write(project.state_file(), v.to_string()).unwrap();
    let loaded = project.load_state().unwrap().unwrap();
    assert!(loaded.project_id.is_none());
}

fn destroy_scoped(
    rec: StopRecord,
    rig: &Rig,
    purge: bool,
) -> (Result<u8, crate::error::CliError>, Project, TempDir, String) {
    let td = TempDir::new();
    let mut state = state_with(two_deployment_graph());
    state.stops = vec![rec];
    let project = saved(&td, &state);
    let mut rig_env = rig.env();
    rig_env.purge = purge;
    let mut out = Vec::new();
    let result = commands::destroy::run_with(
        &project,
        &["support_two".to_owned()],
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &rig_env,
    );
    (result, project, td, utf8(out))
}

fn write_files(rec: &StopRecord) {
    let def = std::path::PathBuf::from(rec.definition.clone().unwrap());
    std::fs::create_dir_all(def.parent().unwrap()).unwrap();
    std::fs::write(&def, "secret").unwrap();
    for l in &rec.logs {
        let l = std::path::PathBuf::from(l);
        std::fs::create_dir_all(l.parent().unwrap()).unwrap();
        std::fs::write(l, "log").unwrap();
    }
}

#[cfg(unix)]
#[test]
fn destroy_removes_the_recorded_definition_and_leaves_logs_unless_purged() {
    let home = TempDir::new();
    let rec = scoped_rec("aaaa1111", "support_two", &home.path);
    write_files(&rec);
    let def = std::path::PathBuf::from(rec.definition.clone().unwrap());
    let rig = Rig::new();
    let (result, project, _td, out) = destroy_scoped(rec.clone(), &rig, false);
    result.unwrap();
    assert!(!def.exists(), "secret-bearing file deleted with the stop");
    for l in &rec.logs {
        assert!(
            std::path::Path::new(l).exists(),
            "logs stay without --purge"
        );
        assert!(
            out.contains("amicu destroy --purge") && out.contains(l),
            "{out}"
        );
    }
    assert_eq!(stops_for(&project, "support_two").len(), 0);
    assert_eq!(rig.stopped(), ["io.amicu.llama.aaaa1111.support_two"]);
}

#[cfg(target_os = "macos")]
#[test]
fn destroy_purge_deletes_logs() {
    let home = TempDir::new();
    let rec = scoped_rec("aaaa1111", "support_two", &home.path);
    write_files(&rec);
    let log = std::path::PathBuf::from(rec.logs[0].clone());
    let (result, _p, _td, _out) = destroy_scoped(rec, &Rig::new(), true);
    result.unwrap();
    assert!(!log.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn purge_refuses_a_log_path_that_is_not_the_records_own_before_changing_anything() {
    let home = TempDir::new();
    let victim = home.path.join("precious.log");
    std::fs::write(&victim, "keep").unwrap();
    let mut rec = scoped_rec("aaaa1111", "support_two", &home.path);
    rec.logs = vec![victim.to_string_lossy().into_owned()];
    let rig = Rig::new();
    let (result, project, _td, _) = destroy_scoped(rec.clone(), &rig, true);
    result.unwrap_err();
    assert!(victim.exists());
    assert_eq!(rig.stopped().len(), 0, "nothing was stopped");
    assert_eq!(stops_for(&project, "support_two"), vec![rec]);
}

#[test]
fn a_failed_stop_keeps_the_record_with_its_recorded_files() {
    let home = TempDir::new();
    let rec = scoped_rec("aaaa1111", "support_two", &home.path);
    write_files(&rec);
    let (result, project, _td, _) = destroy_scoped(rec.clone(), &Rig::failing(), true);
    result.unwrap_err();
    assert_eq!(stops_for(&project, "support_two"), vec![rec]);
}

#[test]
fn a_recorded_definition_that_is_not_the_records_own_file_is_never_deleted_or_stopped() {
    let home = TempDir::new();
    let victim = home.path.join("precious.plist");
    std::fs::write(&victim, "keep").unwrap();
    let mut rec = scoped_rec("aaaa1111", "support_two", &home.path);
    rec.definition = Some(victim.to_string_lossy().into_owned());
    let rig = Rig::new();
    let (result, project, _td, _) = destroy_scoped(rec, &rig, false);
    result.unwrap_err();
    assert!(victim.exists());
    assert_eq!(rig.stopped().len(), 0);
    assert_eq!(stops_for(&project, "support_two").len(), 1, "record kept");
}

#[test]
fn modal_volumes_are_never_deleted_even_with_purge_and_are_listed_with_the_manual_command() {
    let rig = Rig::new();
    let td = TempDir::new();
    let mut state = state_with(modal_graph());
    state.stops = vec![
        modal_rec("support_two", Some("ap-2"), Some("acme"))
            .with_volumes(&["amicu-hf-cache", "amicu-vllm-cache"]),
        modal_rec("support_local", Some("ap-1"), Some("acme"))
            .with_volumes(&["amicu-hf-cache", "amicu-vllm-cache"]),
    ];
    let project = saved(&td, &state);
    for purge in [false, true] {
        project.save_state(&state).unwrap();
        let mut env = rig.env();
        env.purge = purge;
        let mut out = Vec::new();
        commands::destroy::run_with(
            &project,
            &[],
            true,
            None,
            OutputFormat::Human,
            &mut out,
            &mut std::io::Cursor::new(Vec::new()),
            &env,
        )
        .unwrap();
        let out = utf8(out);
        assert!(out.contains("modal volume delete amicu-hf-cache"), "{out}");
        assert!(out.contains("other projects or apps"), "{out}");
    }
}

// ---- host refresh and failed updates -------------

fn single_graph() -> amicu_core::graph::Graph {
    amicu_hcl::parse_graph(VALID_CONFIG).unwrap()
}

fn apply_with(project: &Project, rig: &Rig) -> Result<u8, crate::error::CliError> {
    commands::apply::run_with(
        project,
        true,
        None,
        OutputFormat::Human,
        &mut Vec::new(),
        &mut std::io::Cursor::new(Vec::new()),
        &rig.env(),
    )
}

fn plan_text(project: &Project, rig: &Rig) -> String {
    let mut out = Vec::new();
    let state = crate::refresh::load_refreshed_state(
        project,
        &rig.modal,
        &rig.runpod,
        OutputFormat::Human,
        &mut out,
    )
    .unwrap()
    .unwrap();
    format!("{}|pending={:?}", utf8(out), state.pending)
}

fn running_host_state() -> LocalState {
    let mut state = state_with(single_graph());
    state.stops = vec![StopRecord::host(
        &"support_local".into(),
        Some(&platform_ctx()),
    )];
    state
}

#[test]
fn a_dead_host_service_is_drift_kept_in_state_and_planned_as_a_create() {
    let td = TempDir::new();
    let project = saved(&td, &running_host_state());
    let mut rig = Rig::new();
    rig.modal.host_down = vec!["support_local".to_owned()];
    let text = plan_text(&project, &rig);
    assert!(text.contains("drift: deployment support_local"), "{text}");
    assert!(text.contains("is not running"), "{text}");
    assert!(text.contains("|pending=["), "{text}");
    assert!(!text.contains("pending=[]"), "{text}");
    // Read-only: the state on disk is untouched, and nothing was stopped.
    let on_disk = project.load_state().unwrap().unwrap();
    assert_eq!(on_disk.pending.len(), 0);
    assert_eq!(on_disk.stops.len(), 1);
    assert_eq!(rig.stopped().len(), 0);
}

#[test]
fn a_running_host_service_is_not_drift() {
    let td = TempDir::new();
    let project = saved(&td, &running_host_state());
    let text = plan_text(&project, &Rig::new());
    assert!(!text.contains("drift"), "{text}");
}

#[test]
fn apply_restarts_a_dead_host_service_and_clears_pending() {
    let td = TempDir::new();
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let project = saved(&td, &running_host_state());
    let mut rig = Rig::new();
    rig.modal.host_down = vec!["support_local".to_owned()];
    assert_eq!(apply_with(&project, &rig).unwrap(), 0);
    assert_eq!(*rig.log.started.lock().unwrap(), [label("support_local")]);
    let after = project.load_state().unwrap().unwrap();
    assert_eq!(after.pending.len(), 0);
    assert_eq!(after.stops.len(), 1, "stop identity kept");
}

#[test]
fn apply_prints_the_boot_hint_for_a_host_deployment_in_human_output_only() {
    for (output, shown) in [(OutputFormat::Human, true), (OutputFormat::Json, false)] {
        let td = TempDir::new();
        write(&td.path, "amicu.hcl", VALID_CONFIG);
        let project = saved(&td, &running_host_state());
        let mut rig = Rig::new();
        rig.modal.host_down = vec!["support_local".to_owned()];
        let mut out = Vec::new();
        let code = commands::apply::run_with(
            &project,
            true,
            None,
            output,
            &mut out,
            &mut std::io::Cursor::new(Vec::new()),
            &rig.env(),
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(utf8(out).contains("fake lingering hint"), shown);
    }
}

#[test]
fn destroy_confirms_the_stop_of_a_dead_host_service_before_forgetting_it() {
    let td = TempDir::new();
    let project = saved(&td, &running_host_state());
    let mut rig = Rig::new();
    rig.modal.host_down = vec!["support_local".to_owned()];
    let (result, _) = run_destroy(&project, &[], true, OutputFormat::Human, "", &rig);
    result.unwrap();
    assert_eq!(rig.stopped(), [label("support_local")]);
    assert_eq!(project.load_state().unwrap().unwrap().stops.len(), 0);
}

#[test]
fn a_failed_host_update_leaves_the_deployment_pending_with_its_stop_record() {
    let td = TempDir::new();
    // State holds the old definition (port 8001); the config says 8000.
    let old = amicu_hcl::parse_graph(&VALID_CONFIG.replace("8000", "8001")).unwrap();
    let mut state = state_with(old);
    state.stops = vec![StopRecord::host(
        &"support_local".into(),
        Some(&platform_ctx()),
    )];
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let project = saved(&td, &state);
    let rig = Rig::failing_start();
    let err = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(err.contains("model file not found"), "{err}");
    assert!(err.contains("recorded as pending"), "{err}");
    assert!(
        err.contains("amicu destroy --target support_local"),
        "{err}"
    );
    let after = project.load_state().unwrap().unwrap();
    assert_eq!(after.pending, ["support_local".into()]);
    // The legacy unscoped instance was stopped and confirmed (migration); the
    // project-scoped one written ahead of the failed start stays stoppable.
    assert_eq!(after.stops.len(), 1, "{:?}", after.stops);
    assert!(
        after.stops[0].identity.starts_with("io.amicu.llama.")
            && after.stops[0].identity.ends_with(".support_local")
            && after.stops[0].identity != label("support_local"),
        "{:?}",
        after.stops
    );
    assert_eq!(
        rig.stopped(),
        [label("support_local")],
        "legacy job stopped first"
    );
    // The next plan no longer says "No changes": it plans a create.
    let plan = crate::refresh::load_refreshed_state(
        &project,
        &rig.modal,
        &rig.runpod,
        OutputFormat::Human,
        &mut Vec::new(),
    )
    .unwrap()
    .unwrap();
    assert!(plan.pending.iter().any(|p| p.as_str() == "support_local"));
    // And destroy still stops it from state alone.
    let (result, _) = run_destroy(&project, &[], true, OutputFormat::Human, "", &Rig::new());
    result.unwrap();
}

#[test]
fn a_no_op_apply_with_do_not_track_stops_the_telemetry_agents() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let graph = project.load_graph().unwrap();
    project.save_state(&state_with(graph)).unwrap();
    let mut rig = Rig::new();
    // Opted in in the file, DO_NOT_TRACK=1 in this shell.
    rig.telemetry.env = vec![("DO_NOT_TRACK", "1")];
    let mut out = Vec::new();
    let code = commands::apply::run_with(
        &project,
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &rig.env(),
    )
    .unwrap();
    assert_eq!(code, 0);
    assert!(utf8(out).contains("No changes to apply."));
    assert_eq!(*rig.log.events.lock().unwrap(), ["stop telemetry agents"]);
    assert!(
        rig.telemetry
            .cleared
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    // Opted in everywhere: nothing is stopped.
    let rig = Rig::new();
    let mut out = Vec::new();
    commands::apply::run_with(
        &project,
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &rig.env(),
    )
    .unwrap();
    assert!(rig.log.events.lock().unwrap().is_empty());
}

#[test]
fn apply_with_no_changes_records_a_fingerprint_baseline_and_changes_nothing_else() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let graph = project.load_graph().unwrap();
    project.save_state(&state_with(graph.clone())).unwrap();

    let mut out = Vec::new();
    let code = run_apply(&project, OutputFormat::Human, &mut out).unwrap();
    assert_eq!(code, 0);
    assert!(utf8(out).contains("No changes to apply."));
    let state = project.load_state().unwrap().unwrap();
    assert_eq!(state.graph, graph);
    assert!(state.pending.is_empty() && state.stops.is_empty());
    assert_eq!(state.fingerprints.len(), graph.deployments.len());
    // Only digests are stored.
    let text = std::fs::read_to_string(project.state_dir().join("state.json")).unwrap_or_default();
    assert!(!text.contains("llama-server"));
}

struct P;
impl amicu_engine::fingerprint::EnvProbe for P {
    fn image(
        &self,
        _: &amicu_core::resource::Deployment,
        _: Option<&amicu_core::resource::HardwareProfile>,
    ) -> amicu_engine::fingerprint::Fact {
        amicu_engine::fingerprint::Fact::NotApplicable
    }
    fn binary(
        &self,
        _: &amicu_core::resource::Deployment,
        _: Option<&amicu_core::resource::HardwareProfile>,
    ) -> amicu_engine::fingerprint::Fact {
        amicu_engine::fingerprint::Fact::Known("/opt/llama-server".to_owned())
    }
}

#[test]
fn plan_shows_why_an_unchanged_config_is_an_update() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let graph = project.load_graph().unwrap();
    let d = &graph.deployments[0];
    let mut recorded = std::collections::BTreeMap::new();
    recorded.insert(
        d.id.clone(),
        amicu_engine::fingerprint::Fingerprint {
            binary: Some("0".repeat(64)),
            ..Default::default()
        },
    );
    let result =
        crate::planner::build_project_plan_with(&project, &graph, Some(&graph), &recorded, &P)
            .unwrap();
    let mut out = Vec::new();
    commands::plan::print_human(&result, &graph, &mut out).unwrap();
    let text = utf8(out);
    assert!(text.contains("action:   update"), "{text}");
    assert!(
        text.contains("reason:   binary path changed since it was last started"),
        "{text}"
    );
}

#[test]
fn a_host_service_that_cannot_be_probed_is_a_warning_never_drift() {
    let td = TempDir::new();
    let project = saved(&td, &running_host_state());
    let mut rig = Rig::new();
    rig.modal.host_down = vec!["support_local".to_owned()];
    rig.modal.host_unanswered = true;
    let mut state = project.load_state().unwrap().unwrap();
    let refresh = crate::refresh::refresh_state(&mut state, &rig.modal);
    assert_eq!(refresh.notes.len(), 0);
    assert_eq!(state.pending.len(), 0, "unknown is never down");
    let warning = refresh.warning.unwrap();
    assert!(
        warning.contains("could not check host services"),
        "{warning}"
    );
    assert!(warning.contains("Failed to connect to bus"), "{warning}");
}

// Provider switches: the old instance is stopped and
// confirmed before the new one starts, and is never dropped from state
// otherwise.

/// `support_local` on Modal (a Hugging Face GGUF, llama.cpp).
fn modal_config() -> String {
    let model = VALID_CONFIG
        .split("resource \"hardware_profile\"")
        .next()
        .unwrap();
    format!(
        r#"{model}
resource "hardware_profile" "modal_l4" {{
  provider = "modal"
  region   = "eu-west"
  accelerator {{
    kind   = "gpu"
    model  = "L4"
    count  = 1
    memory = "24GiB"
  }}
  memory = "32GiB"
}}

resource "deployment" "support_local" {{
  model    = model.support_chat.id
  hardware = hardware_profile.modal_l4.id
  runtime  = "llama_cpp"
  api_key  = secret("AMICU_TEST_UNSET_208_KEY")
  llama_cpp {{
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }}
}}
"#
    )
}

fn modal_support_graph() -> amicu_core::graph::Graph {
    amicu_hcl::parse_graph(&modal_config()).unwrap()
}

fn running_modal_state() -> LocalState {
    let mut state = state_with(modal_support_graph());
    state.stops = vec![modal_rec("support_local", Some("ap-old"), Some("acme"))];
    state
}

fn plan_text_for(config: &str, state: &LocalState) -> (String, amicu_plan::Plan) {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", config);
    let graph = project.load_graph().unwrap();
    let result = crate::planner::build_project_plan_with(
        &project,
        &graph,
        Some(&state.graph),
        &state.fingerprints,
        &P,
    )
    .unwrap();
    let switches = commands::plan::provider_switches(
        &result,
        &commands::apply::effective_stops(Some(state)),
        state.project_id.as_deref(),
    );
    let mut out = Vec::new();
    commands::plan::print_human_with(&result, &graph, &switches, &mut out).unwrap();
    // The JSON plan carries the same steps, so the counts agree.
    let mut plan = result.plan.clone();
    commands::plan::add_switch_actions(&mut plan, &switches);
    let text = utf8(out);
    let n = plan.actions.len();
    assert!(
        text.contains(&format!("Plan: {n} action(s).")) || n == 0,
        "{text}"
    );
    (text, plan)
}

#[test]
fn plan_shows_a_destroy_old_step_before_the_create_on_a_provider_switch() {
    let (text, plan) = plan_text_for(&modal_config(), &running_host_state());
    let destroy = text
        .find("destroy old (")
        .unwrap_or_else(|| panic!("{text}"));
    assert!(text.contains(&label("support_local")), "{text}");
    assert!(destroy < text.find("action:   update").unwrap(), "{text}");
    assert_eq!(plan.actions.len(), 2);
    assert_eq!(
        plan.actions[0].kind,
        amicu_plan::contract::ActionKind::Destroy
    );
    assert_eq!(
        plan.actions[1].kind,
        amicu_plan::contract::ActionKind::Update
    );

    let (text, _) = plan_text_for(VALID_CONFIG, &running_modal_state());
    assert!(
        text.contains("destroy old (modal amicu-support-local)"),
        "{text}"
    );
}

#[test]
fn plan_shows_the_modal_rename_for_v1_state_without_stop_records() {
    // v1 state: no stop records, only the graph. Apply derives the legacy-named
    // record and replaces that app, so the plan must show the same step.
    let mut state = state_with(modal_support_graph());
    state.project_id = Some("aaaa1111".to_owned());
    assert_eq!(state.stops, Vec::<StopRecord>::new());
    // A recorded fingerprint that differs, so the deployment is re-launched.
    state.fingerprints.insert(
        "support_local".into(),
        amicu_engine::fingerprint::Fingerprint {
            binary: Some("0".repeat(64)),
            ..Default::default()
        },
    );
    let (text, plan) = plan_text_for(&modal_config(), &state);
    assert!(
        text.contains("destroy old (modal amicu-support-local)"),
        "{text}"
    );
    assert!(text.contains("endpoint URL changes"), "{text}");
    assert!(text.contains("no rollback"), "{text}");
    assert_eq!(
        plan.actions[0].kind,
        amicu_plan::contract::ActionKind::Destroy
    );
}

#[test]
fn plan_shows_a_legacy_label_replaced_by_a_project_scoped_one_but_not_a_plain_update() {
    // Same provider, but the recorded label is the legacy unscoped one and the
    // new start uses the project-scoped label: apply stops the old first.
    let changed = VALID_CONFIG.replace("8000", "8001");
    let mut state = running_host_state();
    state.project_id = Some("a1b2c3d4".to_owned());
    let (text, plan) = plan_text_for(&changed, &state);
    assert!(
        text.contains("destroy old (") && text.contains(&label("support_local")),
        "{text}"
    );
    assert_eq!(plan.actions.len(), 2);

    state.stops = vec![StopRecord::host_in(
        "a1b2c3d4",
        &"support_local".into(),
        Some(&platform_ctx()),
        None,
    )];
    let (text, plan) = plan_text_for(&changed, &state);
    assert!(!text.contains("destroy old"), "{text}");
    assert_eq!(plan.actions.len(), 1);
}

#[test]
fn modal_to_host_stops_and_confirms_the_old_app_before_starting_the_host() {
    let td = TempDir::new();
    let project = saved(&td, &running_modal_state());
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let rig = Rig::new();
    rig.modal
        .apps
        .lock()
        .unwrap()
        .push(("amicu-support-local".to_owned(), "ap-old".to_owned()));
    apply_with(&project, &rig).unwrap();

    let events = rig.log.events.lock().unwrap().clone();
    assert_eq!(
        events,
        [
            "stop modal ap-old".to_owned(),
            format!("start host {}", label("support_local")),
        ]
    );
    let stops = stops_for(&project, "support_local");
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0].backend, Backend::host());
}

#[test]
fn modal_to_host_with_an_unconfirmed_old_stop_fails_and_keeps_the_old_record() {
    let td = TempDir::new();
    let project = saved(&td, &running_modal_state());
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let mut rig = Rig::new();
    rig.modal
        .apps
        .lock()
        .unwrap()
        .push(("amicu-support-local".to_owned(), "ap-old".to_owned()));
    rig.modal.stop_fails = true;
    let err = apply_with(&project, &rig).expect_err("stop not confirmed");
    assert!(err.to_string().contains("could not be confirmed"), "{err}");

    assert!(
        rig.log.started.lock().unwrap().is_empty(),
        "the new instance must not start"
    );
    let stops = stops_for(&project, "support_local");
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert!(stops[0].backend.is_modal());
    assert_eq!(stops[0].app_id.as_deref(), Some("ap-old"));
}

#[test]
fn host_to_modal_with_an_unconfirmed_old_stop_fails_and_keeps_the_old_record() {
    let td = TempDir::new();
    let project = saved(&td, &running_host_state());
    write(&td.path, "amicu.hcl", &modal_config());
    let rig = Rig::failing();
    // Proxy tokens are injected so the Modal pre-check passes and apply
    // reaches the stale-stop loop (no real process environment involved).
    let mut env = rig.env();
    env.proxy_tokens = || {
        amicu_modal::proxy_auth::TokenConfig::from_values(Some("wk-1".into()), Some("ws-1".into()))
    };
    let err = commands::apply::run_with(
        &project,
        true,
        None,
        OutputFormat::Human,
        &mut Vec::new(),
        &mut std::io::Cursor::new(Vec::new()),
        &env,
    )
    .expect_err("old stop not confirmed");
    assert!(err.to_string().contains("could not be confirmed"), "{err}");
    assert_eq!(
        rig.log.events.lock().unwrap().clone(),
        [format!("stop host failed {}", label("support_local"))],
        "the host stop was attempted, and nothing else happened"
    );
    assert!(rig.log.started.lock().unwrap().is_empty());
    assert!(rig.modal.stopped.lock().unwrap().is_empty());
    let stops = stops_for(&project, "support_local");
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0].backend, Backend::host());
}

// ---- compare-and-swap conflicts reach the user with the right advice --------

/// A hook that makes another amicu process write state, bumping its serial.
fn concurrent_writer(root: &std::path::Path) -> Box<dyn FnOnce() + Send> {
    let root = root.to_path_buf();
    Box::new(move || {
        let other = Project::new(&root);
        let state = other.load_state().unwrap().unwrap();
        other.save_state(&state).unwrap();
    })
}

/// State with `support_local` running, and a config that also adds
/// `support_two`, which apply will start.
fn running_one_apply_adds_another(td: &TempDir) -> Project {
    write(&td.path, "amicu.hcl", &two_deployments());
    saved(td, &running_host_state())
}

#[test]
fn apply_final_save_conflict_lists_only_what_this_run_started() {
    let td = TempDir::new();
    let project = running_one_apply_adds_another(&td);
    let rig = Rig::new();
    rig.log
        .hooks
        .lock()
        .unwrap()
        .insert("start", concurrent_writer(&td.path));
    let msg = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(msg.contains("changed since it was read"), "{msg}");
    assert!(msg.contains("support_two"), "{msg}");
    assert!(msg.contains("recorded in state as pending"), "{msg}");
    assert!(msg.contains("amicu apply"), "{msg}");
    assert!(
        !msg.contains("support_local"),
        "an untouched deployment must not be listed as untracked: {msg}"
    );
}

#[test]
fn apply_write_ahead_conflict_says_nothing_was_started() {
    let td = TempDir::new();
    let project = running_one_apply_adds_another(&td);
    let rig = Rig::new();
    rig.log
        .hooks
        .lock()
        .unwrap()
        .insert("ctx", concurrent_writer(&td.path));
    let msg = apply_with(&project, &rig).unwrap_err().to_string();
    assert!(msg.contains("changed since it was read"), "{msg}");
    assert!(msg.contains("Nothing was started by this run"), "{msg}");
    assert!(!msg.contains("stop each by hand"), "{msg}");
    assert_eq!(rig.log.started.lock().unwrap().len(), 0);
}

#[test]
fn destroy_conflict_says_rerun_and_never_lists_kept_deployments() {
    let td = TempDir::new();
    let mut state = state_with(two_deployment_graph());
    state.stops = vec![host_rec("support_local"), host_rec("support_two")];
    let project = saved(&td, &state);
    let rig = Rig::new();
    rig.log
        .hooks
        .lock()
        .unwrap()
        .insert("stop", concurrent_writer(&td.path));
    let (result, _) = run_destroy(
        &project,
        &["support_local".to_owned()],
        true,
        OutputFormat::Human,
        "",
        &rig,
    );
    let msg = result.expect_err("conflict").to_string();
    assert!(msg.contains("changed since it was read"), "{msg}");
    assert!(msg.contains("re-run of `amicu destroy`"), "{msg}");
    assert!(!msg.contains("stop each by hand"), "{msg}");
    assert!(!msg.contains("still be running"), "{msg}");
    assert!(
        !msg.contains("support_two"),
        "a kept deployment must not be listed: {msg}"
    );
}

#[test]
fn broken_pipe_exits_quietly() {
    use crate::error::CliError;

    let broken = Err(CliError::io(
        "write failed",
        std::io::Error::from(std::io::ErrorKind::BrokenPipe),
    ));
    let mut err = Vec::new();
    assert_eq!(crate::exit_status(&broken, &mut err), 0);
    assert!(err.is_empty(), "broken pipe must not print an error");

    let other = Err(CliError::io(
        "write failed",
        std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    ));
    assert_eq!(crate::exit_status(&other, &mut err), 1);
    assert!(String::from_utf8(err).unwrap().starts_with("error: "));
    assert_eq!(crate::exit_status(&Ok(3), &mut Vec::new()), 3);
}

// ---- project discovery: only this project's config --------------------------

#[test]
fn lock_files_and_foreign_project_dirs_are_not_loaded() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    // A Terraform lock file anywhere, and a whole nested Terraform project.
    let tf = td.path.join("infra/bootstrap");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    let lock = "provider \"registry.terraform.io/hashicorp/aws\" {\n  version = \"5.0.0\"\n}\n";
    std::fs::write(tf.join(".terraform.lock.hcl"), lock).unwrap();
    std::fs::write(tf.join(".terraform/stuff.hcl"), lock).unwrap();
    std::fs::write(tf.join("terragrunt.hcl"), lock).unwrap();
    std::fs::write(td.path.join("deps.lock.hcl"), lock).unwrap();

    let graph = project.load_graph().unwrap();
    assert_eq!(graph.models.len(), 1);
    let files = project.config_files().unwrap();
    assert_eq!(files, vec![td.path.join("amicu.hcl")]);
}

#[test]
fn a_nested_amicu_project_is_not_part_of_the_outer_one() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let inner = td.path.join("other");
    std::fs::create_dir_all(inner.join(".amicu")).unwrap();
    std::fs::write(inner.join(".amicu/config.yaml"), "").unwrap();
    std::fs::write(inner.join("amicu.hcl"), SECOND_MODEL).unwrap();
    assert_eq!(project.load_graph().unwrap().models.len(), 1);
}

#[test]
fn a_parent_of_unrelated_projects_is_not_an_amicu_project() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    let other = td.path.join("other-project/bootstrap");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join(".terraform.lock.hcl"), "provider \"x\" {}\n").unwrap();
    std::fs::write(other.join("stack.hcl"), "provider \"x\" {}\n").unwrap();

    let err = project.load_graph().unwrap_err();
    assert!(matches!(err, crate::error::CliError::NoConfig(_)));
    assert_eq!(
        err.to_string(),
        "no amicu project found here (run `amicu init`)"
    );
    let mut out = Vec::new();
    let err = commands::plan::run(&project, OutputFormat::Human, &mut out).unwrap_err();
    assert!(err.to_string().contains("no amicu project found here"));
}

// ---- non-interactive confirmation (#260) -------------------------------------

#[test]
fn destroy_without_a_terminal_fails_naming_auto_approve_and_stops_nothing() {
    let td = TempDir::new();
    let project = saved(&td, &state_with(two_deployment_graph()));
    let rig = Rig::new();
    let mut env = rig.env();
    env.interactive = false;
    let mut out = Vec::new();
    let err = commands::destroy::run_with(
        &project,
        &[],
        false,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(b"y\n".to_vec()),
        &env,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--auto-approve"), "{err}");
    assert!(!utf8(out).contains("Destroy cancelled"));
    assert_eq!(rig.stopped().len(), 0);
    assert_eq!(
        project
            .load_state()
            .unwrap()
            .unwrap()
            .graph
            .deployments
            .len(),
        2
    );
}

#[test]
fn apply_without_a_terminal_fails_naming_auto_approve_and_writes_no_state() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let rig = Rig::new();
    let mut env = rig.env();
    env.interactive = false;
    let mut out = Vec::new();
    let err = commands::apply::run_with(
        &project,
        false,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(b"y\n".to_vec()),
        &env,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--auto-approve"), "{err}");
    assert!(project.load_state().unwrap().is_none());
}

#[test]
fn an_interactive_destroy_still_prompts_and_auto_approve_needs_no_terminal() {
    let td = TempDir::new();
    let project = saved(&td, &state_with(two_deployment_graph()));
    let rig = Rig::new();
    // Interactive: a "n" answer cancels, as before.
    let (res, out) = run_destroy(&project, &[], false, OutputFormat::Human, "n\n", &rig);
    assert_eq!(res.unwrap(), 0);
    assert!(out.contains("Destroy cancelled."), "{out}");
    // No terminal but --auto-approve: proceeds.
    let mut env = rig.env();
    env.interactive = false;
    let mut out = Vec::new();
    let code = commands::destroy::run_with(
        &project,
        &[],
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &env,
    )
    .unwrap();
    assert_eq!(code, 0);
}

// ---- launch arguments are tracked (#261) -------------------------------------

#[test]
fn plan_shows_a_launch_argument_change_as_an_update_with_a_reason() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let graph = project.load_graph().unwrap();
    let d = &graph.deployments[0];
    let mut recorded = std::collections::BTreeMap::new();
    recorded.insert(
        d.id.clone(),
        amicu_engine::fingerprint::Fingerprint {
            launch: Some("0".repeat(64)),
            ..Default::default()
        },
    );
    let result =
        crate::planner::build_project_plan_with(&project, &graph, Some(&graph), &recorded, &P)
            .unwrap();
    let mut out = Vec::new();
    commands::plan::print_human(&result, &graph, &mut out).unwrap();
    let text = utf8(out);
    assert!(text.contains("action:   update"), "{text}");
    assert!(
        text.contains("reason:   launch arguments changed since it was last started"),
        "{text}"
    );
}

#[test]
fn state_without_a_recorded_launch_digest_is_not_drift() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    let graph = project.load_graph().unwrap();
    let mut recorded = std::collections::BTreeMap::new();
    recorded.insert(
        graph.deployments[0].id.clone(),
        amicu_engine::fingerprint::Fingerprint::default(),
    );
    let result =
        crate::planner::build_project_plan_with(&project, &graph, Some(&graph), &recorded, &P)
            .unwrap();
    assert!(result.items.is_empty());
}

// ---- purge-pending leftovers (#262) ------------------------------------------

#[cfg(target_os = "macos")]
#[test]
fn a_plain_destroy_records_the_leftover_logs_and_a_later_purge_removes_them() {
    let home = TempDir::new();
    let rec = scoped_rec("aaaa1111", "support_two", &home.path);
    write_files(&rec);
    let rig = Rig::new();
    let (result, project, _td, _) = destroy_scoped(rec.clone(), &rig, false);
    result.unwrap();

    // Tracked in state, not as a stop record or deployment.
    let state = project.load_state().unwrap().unwrap();
    assert!(
        state
            .stops
            .iter()
            .all(|s| s.deployment.as_str() != "support_two")
    );
    assert_eq!(state.purge_pending.len(), 1);
    assert_eq!(state.purge_pending[0].logs, rec.logs);
    // Not drift: the plan against the unchanged config touches only what the
    // config says, never the leftover record.
    assert!(
        state
            .graph
            .deployments
            .iter()
            .all(|d| d.id.as_str() != "support_two")
    );

    // `destroy --purge` with nothing else to destroy removes them and clears the record.
    let mut empty = state.clone();
    empty.graph = amicu_core::graph::Graph::new();
    empty.stops.clear();
    empty.endpoints.clear();
    project.save_state(&empty).unwrap();
    let mut env = rig.env();
    env.purge = true;
    let mut out = Vec::new();
    let code = commands::destroy::run_with(
        &project,
        &[],
        false,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &env,
    )
    .unwrap();
    assert_eq!(code, 0);
    let out = utf8(out);
    assert!(out.contains("Purged the log files"), "{out}");
    for l in &rec.logs {
        assert!(!std::path::Path::new(l).exists(), "{l} removed");
    }
    assert_eq!(
        project.load_state().unwrap().unwrap().purge_pending.len(),
        0
    );
}

#[cfg(target_os = "macos")]
#[test]
fn destroy_without_purge_lists_tracked_leftovers_and_keeps_the_record() {
    let home = TempDir::new();
    let rec = scoped_rec("aaaa1111", "support_two", &home.path);
    write_files(&rec);
    let td = TempDir::new();
    let mut state = state_with(amicu_core::graph::Graph::new());
    state.purge_pending = vec![rec.purge_entry().unwrap()];
    let project = saved(&td, &state);
    let (res, out) = run_destroy(&project, &[], true, OutputFormat::Human, "", &Rig::new());
    assert_eq!(res.unwrap(), 0);
    assert!(
        out.contains("amicu destroy --purge") && out.contains(&rec.logs[0]),
        "{out}"
    );
    assert_eq!(
        project.load_state().unwrap().unwrap().purge_pending.len(),
        1
    );
    assert!(std::path::Path::new(&rec.logs[0]).exists());
}

#[cfg(target_os = "macos")]
#[test]
fn a_purge_pending_log_that_is_not_the_records_own_is_refused_and_kept() {
    let home = TempDir::new();
    let victim = home.path.join("precious.log");
    std::fs::write(&victim, "keep").unwrap();
    let mut rec = scoped_rec("aaaa1111", "support_two", &home.path);
    rec.logs = vec![victim.to_string_lossy().into_owned()];
    let td = TempDir::new();
    let mut state = state_with(amicu_core::graph::Graph::new());
    state.purge_pending = vec![rec.purge_entry().unwrap()];
    let project = saved(&td, &state);
    let rig = Rig::new();
    let mut env = rig.env();
    env.purge = true;
    let mut out = Vec::new();
    let err = commands::destroy::run_with(
        &project,
        &[],
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &env,
    )
    .unwrap_err();
    assert!(err.to_string().contains("will not delete"), "{err}");
    assert!(victim.exists());
    assert_eq!(
        project.load_state().unwrap().unwrap().purge_pending.len(),
        1
    );
}

#[test]
fn purge_pending_survives_a_state_round_trip_and_older_state_loads() {
    let td = TempDir::new();
    let mut state = state_with(amicu_core::graph::Graph::new());
    state.purge_pending = vec![crate::stops::PurgePending {
        deployment: "support_two".into(),
        backend: crate::stops::Backend::HostLaunchd,
        identity: "x".to_owned(),
        logs: vec!["/a/x.out.log".to_owned()],
    }];
    let project = saved(&td, &state);
    assert_eq!(
        project.load_state().unwrap().unwrap().purge_pending.len(),
        1
    );
    let plain = TempDir::new();
    let p2 = saved(&plain, &state_with(amicu_core::graph::Graph::new()));
    let text = std::fs::read_to_string(p2.state_file()).unwrap();
    assert!(!text.contains("purge_pending"));
}

#[test]
fn a_lock_only_dot_amicu_does_not_hide_a_subdirectory() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    // What running `amicu apply` inside `models/` leaves behind: the state lock.
    let models = td.path.join("models");
    std::fs::create_dir_all(models.join(".amicu/state")).unwrap();
    std::fs::write(models.join(".amicu/state/state.lock"), "").unwrap();
    std::fs::write(models.join("extra.hcl"), SECOND_MODEL).unwrap();
    assert_eq!(project.load_graph().unwrap().models.len(), 2);
}

#[cfg(unix)]
#[test]
fn a_directory_symlink_loop_is_not_followed() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", VALID_CONFIG);
    std::os::unix::fs::symlink(&td.path, td.path.join("loop")).unwrap();
    assert_eq!(project.load_graph().unwrap().models.len(), 1);
}

// ---- project root marker -----------------------------------------------------

#[test]
fn a_bare_dot_amicu_dir_does_not_make_a_project_root() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    // What the state lock leaves behind in any directory amicu ran from.
    std::fs::create_dir_all(td.path.join(".amicu/state")).unwrap();
    std::fs::write(td.path.join(".amicu/state/state.lock"), "").unwrap();
    let sub = td.path.join("unrelated");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("main.hcl"), VALID_CONFIG).unwrap();

    assert!(!project.is_project_root().unwrap());
    assert!(matches!(
        project.config_files().unwrap_err(),
        crate::error::CliError::NoConfig(_)
    ));
}

#[test]
fn a_real_marker_or_a_direct_hcl_file_makes_a_project_root() {
    let config = TempDir::new();
    std::fs::create_dir_all(config.path.join(".amicu")).unwrap();
    std::fs::write(config.path.join(".amicu/config.yaml"), "").unwrap();
    assert!(Project::new(&config.path).is_project_root().unwrap());

    let state = TempDir::new();
    std::fs::create_dir_all(state.path.join(".amicu/state")).unwrap();
    std::fs::write(state.path.join(".amicu/state/state.json"), "{}").unwrap();
    assert!(Project::new(&state.path).is_project_root().unwrap());

    let hcl = TempDir::new();
    write(&hcl.path, "amicu.hcl", VALID_CONFIG);
    assert!(Project::new(&hcl.path).is_project_root().unwrap());
}

#[test]
fn destroy_outside_a_project_creates_no_state_dir() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    let rig = Rig::new();
    let mut out = Vec::new();
    let code = commands::destroy::run_with(
        &project,
        &[],
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &rig.env(),
    )
    .unwrap();
    assert_eq!(code, 0);
    assert_eq!(utf8(out), "Nothing to destroy.\n");
    assert!(!td.path.join(".amicu").exists());
}

// ---- unset secrets are caught before anything starts -------------------------

fn nothing_set(_: &str) -> bool {
    false
}

fn keyed_config() -> String {
    let cfg = VALID_CONFIG.replacen(
        "  runtime  = \"llama_cpp\"\n",
        "  runtime  = \"llama_cpp\"\n  api_key  = secret(\"SUPPORT_LOCAL_API_KEY\")\n",
        1,
    );
    assert!(cfg.contains("secret("));
    cfg
}

#[test]
fn plan_errors_on_an_unset_secret_naming_it_and_never_a_value() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", &keyed_config());
    let mut out = Vec::new();
    let code =
        commands::plan::run_with(&project, OutputFormat::Human, &mut out, nothing_set).unwrap();
    let text = utf8(out);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("AMICU-POLICY-002"), "{text}");
    assert!(text.contains("'SUPPORT_LOCAL_API_KEY'"), "{text}");

    // Set: the plan works and still shows the key redacted.
    let mut out = Vec::new();
    let code = commands::plan::run_with(&project, OutputFormat::Human, &mut out, |_| true).unwrap();
    let text = utf8(out);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("******"), "{text}");
}

#[test]
fn apply_with_an_unset_secret_fails_before_any_state_or_start() {
    let td = TempDir::new();
    let project = Project::new(&td.path);
    write(&td.path, "amicu.hcl", &keyed_config());
    let rig = Rig::new();
    let mut env = rig.env();
    env.secret_is_set = nothing_set;
    let mut out = Vec::new();
    let code = commands::apply::run_with(
        &project,
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &env,
    )
    .unwrap();
    let text = utf8(out);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("'SUPPORT_LOCAL_API_KEY'"), "{text}");
    assert!(project.load_state().unwrap().is_none(), "no state written");
    assert_eq!(rig.stopped().len(), 0);
}

#[test]
fn an_unset_secret_of_an_unchanged_deployment_blocks_nothing() {
    let td = TempDir::new();
    let cfg = keyed_config();
    let mut state = state_with(amicu_hcl::parse_graph(&cfg).unwrap());
    state.stops = vec![StopRecord::host(
        &"support_local".into(),
        Some(&platform_ctx()),
    )];
    let project = saved(&td, &state);
    write(&td.path, "amicu.hcl", &cfg);
    let rig = Rig::new();
    let mut env = rig.env();
    env.secret_is_set = nothing_set;
    let mut out = Vec::new();
    let code = commands::apply::run_with(
        &project,
        true,
        None,
        OutputFormat::Human,
        &mut out,
        &mut std::io::Cursor::new(Vec::new()),
        &env,
    )
    .unwrap();
    assert_eq!(code, 0, "{}", utf8(out));
}

#[test]
fn missing_secret_message_lists_every_name_once() {
    let one = crate::secret::missing_secrets_message(&["A".to_owned()]);
    assert_eq!(one, "secret 'A' is not set in the environment");
    let two = crate::secret::missing_secrets_message(&["A".to_owned(), "B".to_owned()]);
    assert_eq!(two, "secrets 'A', 'B' are not set in the environment");
}

// --- host-aware init, plan and doctor; catalog --------------------------------

mod host_catalog {
    use amicu_engine::host::HostFacts;

    use super::*;
    use crate::commands::init::{InitTarget, pick_tier, starter_hcl};
    use crate::host::{HostInfo, fake};

    const GIB: u64 = 1024 * 1024 * 1024;

    fn hosts() -> Vec<(InitTarget, HostInfo)> {
        let mut v = Vec::new();
        for gib in [4, 8, 16, 24, 48, 128] {
            v.push((InitTarget::Mac, fake::mac("m4_pro", gib)));
            v.push((InitTarget::LinuxCpu, fake::linux(gib, None)));
            let gpu = if gib == 24 {
                "rtx-4090-24gb"
            } else {
                "custom_gpu"
            };
            v.push((InitTarget::LinuxCuda, fake::linux(64, Some((gpu, gib)))));
        }
        v.push((InitTarget::Modal, fake::linux(8, None)));
        v
    }

    #[test]
    fn every_starter_validates_plans_and_fits() {
        for (target, host) in hosts() {
            let hcl = starter_hcl(target, Some(&host)).unwrap();
            let graph = amicu_hcl::parse_graph(&hcl)
                .unwrap_or_else(|e| panic!("{target:?} on {}: {e}\n{hcl}", host.describe()));
            let report = report_for(&graph);
            let errors: Vec<_> = report
                .diagnostics
                .iter()
                .filter(|d| d.severity == amicu_core::diagnostic::Severity::Error)
                .map(|d| format!("{} {}", d.code, d.message))
                .collect();
            assert!(
                errors.is_empty(),
                "{target:?} on {}: {errors:?}\n{hcl}",
                host.describe()
            );
            let plan = build_plan(&graph, None).unwrap_or_else(|_| panic!("plan failed: {hcl}"));
            assert!(!plan.has_errors(), "{target:?} on {}", host.describe());
            assert_eq!(plan.items.len(), 1);
        }
    }

    #[test]
    fn starters_are_validated_end_to_end_through_the_commands() {
        for (target, host) in [
            (InitTarget::Mac, fake::mac("m4_pro", 24)),
            (
                InitTarget::LinuxCuda,
                fake::linux(32, Some(("rtx_4090", 24))),
            ),
            (InitTarget::LinuxCpu, fake::linux(16, None)),
            (InitTarget::Modal, fake::mac("m1", 8)),
        ] {
            let td = TempDir::new();
            let project = Project::new(&td.path);
            commands::init::run(&project, Some(Some(target)), Some(&host), &mut Vec::new())
                .unwrap();
            let mut out = Vec::new();
            let code =
                commands::validate::run(&project, false, OutputFormat::Human, &mut out).unwrap();
            assert_eq!(code, 0, "{target:?}: {}", utf8(out));
            let mut out = Vec::new();
            // The secret lookup is injected: the Modal starter needs CHAT_API_KEY.
            let code = commands::plan::run_with(&project, OutputFormat::Human, &mut out, |_| true)
                .unwrap();
            assert_eq!(code, 0, "{target:?}: {}", utf8(out));
            let mut out = Vec::new();
            assert_eq!(commands::fmt::run(&project, true, &mut out).unwrap(), 0);
        }
    }

    #[test]
    fn starter_declares_the_detected_host_and_a_model_that_fits() {
        let hcl = starter_hcl(InitTarget::Mac, Some(&fake::mac("m4_pro", 24))).unwrap();
        assert!(hcl.contains("m4_pro"), "{hcl}");
        assert!(hcl.contains("24GiB"), "{hcl}");
        assert!(hcl.contains("Qwen3.5-9B-Q4_K_M.gguf"), "{hcl}");

        let small = starter_hcl(InitTarget::Mac, Some(&fake::mac("m1", 8))).unwrap();
        assert!(small.contains("Qwen3.5-4B"), "{small}");

        let cuda = starter_hcl(
            InitTarget::LinuxCuda,
            Some(&fake::linux(64, Some(("rtx_3060", 12)))),
        )
        .unwrap();
        assert!(cuda.contains("rtx_3060"), "{cuda}");
        assert!(cuda.contains("n_gpu_layers   = -1"), "{cuda}");
        assert!(cuda.contains("12GiB"), "{cuda}");

        let cpu = starter_hcl(InitTarget::LinuxCpu, Some(&fake::linux(64, None))).unwrap();
        assert!(cpu.contains("local_cpu"), "{cpu}");
        assert!(cpu.contains("n_gpu_layers   = 0"), "{cpu}");
        // CPU skips the 8B tier even with plenty of memory.
        assert!(cpu.contains("Qwen3.5-4B"), "{cpu}");
    }

    #[test]
    fn target_defaults_follow_the_host() {
        assert_eq!(InitTarget::detect(&fake::mac("m4", 16)), InitTarget::Mac);
        assert_eq!(
            InitTarget::detect(&fake::linux(32, Some(("rtx_4090", 24)))),
            InitTarget::LinuxCuda
        );
        assert_eq!(
            InitTarget::detect(&fake::linux(32, None)),
            InitTarget::LinuxCpu
        );
    }

    #[test]
    fn init_without_target_uses_the_detected_host() {
        let td = TempDir::new();
        let project = Project::new(&td.path);
        let mut out = Vec::new();
        commands::init::run(&project, Some(None), Some(&fake::linux(16, None)), &mut out).unwrap();
        let text = utf8(out);
        assert!(text.contains("Example: linux-cpu."), "{text}");
        let hcl = std::fs::read_to_string(td.path.join("amicu.hcl")).unwrap();
        assert!(hcl.contains("local_cpu"));
    }

    #[test]
    fn mismatched_or_undetected_host_is_an_error() {
        let td = TempDir::new();
        let project = Project::new(&td.path);
        let err = commands::init::run(
            &project,
            Some(Some(InitTarget::Mac)),
            Some(&fake::linux(16, None)),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("needs a Mac"), "{err}");
        let err = commands::init::run(
            &project,
            Some(Some(InitTarget::LinuxCuda)),
            Some(&fake::linux(16, None)),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no NVIDIA GPU"), "{err}");
        let err = commands::init::run(&project, Some(None), None, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("--example"), "{err}");
        // Nothing was written by the failed attempts.
        assert!(!td.path.join("amicu.hcl").exists());
        // Modal needs no host at all.
        commands::init::run(
            &project,
            Some(Some(InitTarget::Modal)),
            None,
            &mut Vec::new(),
        )
        .unwrap();
    }

    #[test]
    fn starter_size_is_canonical_and_parses_with_or_without_the_suffix() {
        let td = TempDir::new();
        let project = Project::new(&td.path);
        commands::init::run(
            &project,
            Some(Some(InitTarget::Mac)),
            Some(&fake::mac("m4_pro", 24)),
            &mut Vec::new(),
        )
        .unwrap();
        let hcl = std::fs::read_to_string(td.path.join("amicu.hcl")).unwrap();
        assert!(hcl.contains("size = \"5680522464B\""), "{hcl}");
        // The bare form the docs used to show means the same.
        let bare = hcl.replace("5680522464B", "5680522464");
        let a = amicu_hcl::parse_graph(&hcl).unwrap();
        let b = amicu_hcl::parse_graph(&bare).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn tier_picking_follows_memory() {
        let pick = |gib: u64, cpu| pick_tier(gib * GIB, cpu).id;
        assert_eq!(pick(48, false), "qwen35_27b");
        assert_eq!(pick(32, false), "qwen35_27b");
        assert_eq!(pick(24, false), "qwen35_9b");
        assert_eq!(pick(16, false), "qwen35_9b");
        assert_eq!(pick(12, false), "qwen35_4b");
        assert_eq!(pick(6, false), "qwen35_2b");
        assert_eq!(pick(2, false), "qwen35_0_8b");
        assert_eq!(pick(48, true), "qwen35_4b");
        // A "32 GB" machine reports a little under 32 GiB.
        assert_eq!(pick_tier(31 * GIB, false).id, "qwen35_27b");
        assert_eq!(pick_tier(30 * GIB, false).id, "qwen35_9b");
    }

    const BIG_PROFILE: &str = r#"resource "model" "m" {
  source {
    registry  = "hugging_face"
    reference = "unsloth/Qwen3.5-4B-GGUF"
  }
  architecture   = "qwen35"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "m.gguf"
    size         = "2GiB"
  }
}

resource "hardware_profile" "big" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m4_max"
    count  = 1
    memory = "128GiB"
  }
  memory = "128GiB"
}

resource "deployment" "d" {
  model    = model.m.id
  hardware = hardware_profile.big.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 4096
    port           = 8000
    n_gpu_layers   = -1
  }
}
"#;

    fn facts(gib: u64) -> HostFacts {
        HostFacts {
            memory: amicu_core::ByteSize::from_bytes(gib * GIB),
            accelerator_memory: Some(amicu_core::ByteSize::from_bytes(gib * GIB)),
        }
    }

    #[test]
    fn plan_warns_but_succeeds_when_the_profile_exceeds_the_host() {
        let td = TempDir::new();
        write(&td.path, "amicu.hcl", BIG_PROFILE);
        let project = Project::new(&td.path);
        let mut out = Vec::new();
        let code = commands::plan::run_with_host(
            &project,
            OutputFormat::Human,
            &mut out,
            |_| true,
            &|| Some(facts(16)),
        )
        .unwrap();
        let text = utf8(out);
        assert_eq!(code, 0, "a host mismatch must not fail plan: {text}");
        assert!(text.contains("warn: deployment.d"), "{text}");
        assert!(
            text.contains("hardware profile `big` declares memory 128.0 GiB"),
            "{text}"
        );
        assert!(text.contains("Plan: 1 action"), "{text}");
    }

    #[test]
    fn plan_is_silent_when_the_host_is_big_enough_or_unknown() {
        let td = TempDir::new();
        write(&td.path, "amicu.hcl", BIG_PROFILE);
        let project = Project::new(&td.path);
        for host in [Some(facts(128)), None] {
            let mut out = Vec::new();
            let code = commands::plan::run_with_host(
                &project,
                OutputFormat::Human,
                &mut out,
                |_| true,
                &|| host,
            )
            .unwrap();
            assert_eq!(code, 0);
            assert!(!utf8(out).contains("warn:"));
        }
    }

    #[test]
    fn plan_does_not_probe_the_host_for_remote_profiles() {
        let td = TempDir::new();
        write(
            &td.path,
            "amicu.hcl",
            &BIG_PROFILE.replace("local_apple", "modal"),
        );
        let project = Project::new(&td.path);
        let mut out = Vec::new();
        let probe = || -> Option<HostFacts> { panic!("host must not be probed") };
        // The config has no api_key, so plan fails on that; the point is the probe.
        let _ = commands::plan::run_with_host(
            &project,
            OutputFormat::Human,
            &mut out,
            |_| true,
            &probe,
        );
    }

    #[test]
    fn doctor_reports_the_profile_against_the_host() {
        let td = TempDir::new();
        write(&td.path, "amicu.hcl", BIG_PROFILE);
        let project = Project::new(&td.path);
        let run = |host: &HostInfo| {
            let mut out = Vec::new();
            commands::doctor::run_with(&project, OutputFormat::Json, &mut out, Ok(host), None)
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            v["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == "profile")
                .cloned()
        };
        let small = run(&fake::mac("m1", 8)).expect("profile check");
        assert_eq!(small["status"], "warn");
        assert!(small["detail"].as_str().unwrap().contains("AMICU-MEM-003"));
        let big = run(&fake::mac("m4_max", 128)).expect("profile check");
        assert_eq!(big["status"], "pass");
    }

    #[test]
    fn doctor_has_no_profile_check_without_a_config() {
        let td = TempDir::new();
        let project = Project::new(&td.path);
        let mut out = Vec::new();
        commands::doctor::run_with(
            &project,
            OutputFormat::Json,
            &mut out,
            Ok(&fake::mac("m1", 8)),
            None,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(
            v["checks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["name"] != "profile")
        );
    }

    #[test]
    fn catalog_hardware_lists_gpus_and_chips_as_json() {
        let mut out = Vec::new();
        commands::catalog::hardware(OutputFormat::Json, &mut out).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let rows = v.as_array().unwrap();
        let find = |id: &str| {
            rows.iter()
                .find(|r| r["id"] == id)
                .unwrap_or_else(|| panic!("{id}"))
        };
        assert_eq!(find("m4_pro")["kind"], "apple_silicon");
        assert_eq!(
            find("m4_pro")["memory_gib"],
            serde_json::json!([24, 48, 64])
        );
        assert_eq!(find("rtx_4090")["kind"], "gpu");
        assert_eq!(find("rtx_4090")["memory_gib"], serde_json::json!([24]));
    }

    #[test]
    fn catalog_hardware_human_table_has_a_header_and_rows() {
        let mut out = Vec::new();
        commands::catalog::hardware(OutputFormat::Human, &mut out).unwrap();
        let text = utf8(out);
        assert!(text.starts_with("ID"), "{text}");
        assert!(text.contains("m4_pro"));
        assert!(text.contains("24, 48, 64"));
    }

    #[test]
    fn catalog_presets_lists_every_preset() {
        let mut out = Vec::new();
        commands::catalog::presets(OutputFormat::Json, &mut out).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let ids: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), amicu_recommend::catalog().len());
        assert!(ids.contains(&"local_dev_chat"));
        let mut out = Vec::new();
        commands::catalog::presets(OutputFormat::Human, &mut out).unwrap();
        assert!(utf8(out).contains("local_dev_chat"));
    }

    #[test]
    fn catalog_fits_depends_on_the_host() {
        let report = |host: &HostInfo| {
            let mut out = Vec::new();
            commands::catalog::fits(Ok(host), OutputFormat::Json, &mut out).unwrap();
            serde_json::from_slice::<serde_json::Value>(&out).unwrap()
        };
        let verdict = |v: &serde_json::Value, id: &str| {
            v["starter_models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == id)
                .unwrap()["verdict"]
                .clone()
        };
        let big = report(&fake::mac("m4_pro", 48));
        assert_eq!(big["version"], "v1");
        assert_eq!(big["host"]["accelerator"], "m4_pro");
        assert_eq!(verdict(&big, "qwen35_9b"), "fits");

        let tiny = report(&fake::linux(4, None));
        assert_eq!(verdict(&tiny, "qwen35_9b"), "too_big");
        assert_eq!(verdict(&tiny, "qwen35_0_8b"), "fits");

        // A GPU host is judged by its VRAM, not its system memory.
        let gpu = report(&fake::linux(128, Some(("rtx_3060", 4))));
        assert_eq!(verdict(&gpu, "qwen35_9b"), "too_big");

        // Presets are evaluated too; the 48 GiB host fits the local dev preset.
        let preset = big["presets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "local_dev_chat")
            .unwrap();
        assert_eq!(preset["verdict"], "fits", "{preset}");
        let small = report(&fake::linux(4, None));
        let preset = small["presets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "local_dev_chat")
            .unwrap();
        assert_eq!(preset["verdict"], "too_big", "{preset}");
    }

    #[test]
    fn catalog_fits_human_output_and_undetected_host() {
        let mut out = Vec::new();
        commands::catalog::fits(Ok(&fake::mac("m4_pro", 24)), OutputFormat::Human, &mut out)
            .unwrap();
        let text = utf8(out);
        assert!(text.contains("Host: macos"), "{text}");
        assert!(text.contains("qwen35_9b"));
        assert!(text.contains("local_dev_chat"));
        let err = commands::catalog::fits(
            Err("unsupported OS: freebsd"),
            OutputFormat::Human,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsupported OS"));
    }

    // --- empty init, examples/ -------------------------------------------------

    #[test]
    fn plain_init_writes_only_a_header_that_validates_and_plans_to_nothing() {
        let td = TempDir::new();
        let project = Project::new(&td.path);
        commands::init::run(&project, None, None, &mut Vec::new()).unwrap();
        let hcl = std::fs::read_to_string(td.path.join("amicu.hcl")).unwrap();
        assert!(
            hcl.lines().all(|l| l.starts_with('#') || l.is_empty()),
            "{hcl}"
        );
        assert!(hcl.contains("amicu init --example"));
        assert!(hcl.contains("amicu catalog"));
        assert!(hcl.contains("https://amicu.dev/docs"));

        let mut out = Vec::new();
        assert_eq!(
            commands::validate::run(&project, false, OutputFormat::Human, &mut out).unwrap(),
            0,
            "{}",
            utf8(out)
        );
        let mut out = Vec::new();
        let code =
            commands::plan::run_with(&project, OutputFormat::Human, &mut out, |_| true).unwrap();
        let text = utf8(out);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("No changes"), "{text}");
        let mut out = Vec::new();
        assert_eq!(commands::fmt::run(&project, true, &mut out).unwrap(), 0);
    }

    #[test]
    fn example_upgrades_an_untouched_header_but_never_a_users_file() {
        let td = TempDir::new();
        let project = Project::new(&td.path);
        let host = fake::mac("m4_pro", 48);
        commands::init::run(&project, None, None, &mut Vec::new()).unwrap();
        commands::init::run(&project, Some(None), Some(&host), &mut Vec::new()).unwrap();
        let hcl = std::fs::read_to_string(td.path.join("amicu.hcl")).unwrap();
        assert!(hcl.contains("resource \"model\""), "{hcl}");

        // Now the user's own file: a second --example leaves it alone.
        std::fs::write(td.path.join("amicu.hcl"), "# mine\n").unwrap();
        commands::init::run(&project, Some(None), Some(&host), &mut Vec::new()).unwrap();
        assert_eq!(
            std::fs::read_to_string(td.path.join("amicu.hcl")).unwrap(),
            "# mine\n"
        );
    }

    /// Every tier, on the smallest memory it is picked at, gets a context
    /// that passes the engine's own fit check (weights + KV + overhead).
    #[test]
    fn starter_context_fits_the_engine_check_at_every_tier_threshold() {
        for t in &crate::commands::init::TIERS {
            // The smallest memory the picker still hands this tier (5% slack).
            let mem = (t.min_gib.max(4) * GIB * 95).div_ceil(100);
            let host = fake::mac("m4_pro", 0);
            let mut hw = host.fit_profile();
            hw.memory = amicu_core::ByteSize::from_bytes(mem);
            if let Some(a) = hw.accelerator.as_mut() {
                a.memory = amicu_core::ByteSize::from_bytes(mem);
            }
            let ctx = crate::commands::init::context_for(t, &hw);
            let want = match t.id {
                "qwen35_27b" | "qwen35_4b" | "qwen35_0_8b" => 131_072,
                _ => 262_144,
            };
            assert_eq!(ctx, want, "{} at {mem} bytes", t.id);
            assert!(ctx.is_power_of_two() && ctx >= 2048, "{}: {ctx}", t.id);
        }
    }

    #[test]
    fn starters_use_the_native_window_where_it_fits_and_cap_the_output() {
        let big = starter_hcl(InitTarget::Mac, Some(&fake::mac("m4_pro", 48))).unwrap();
        assert!(big.contains("Qwen3.5-27B-Q4_K_M.gguf"), "{big}");
        assert!(big.contains("context_length = 262144"), "{big}");
        assert!(big.contains("[\"--n-predict\", \"65536\"]"), "{big}");
        // 27B at 262144 needs ~36 GB; a 32 GiB Mac halves the window once.
        let mid = starter_hcl(InitTarget::Mac, Some(&fake::mac("m4_pro", 32))).unwrap();
        assert!(mid.contains("Qwen3.5-27B-Q4_K_M.gguf"), "{mid}");
        assert!(mid.contains("    context_length = 131072"), "{mid}");
    }

    #[test]
    fn sixteen_gb_cards_get_the_9b_tier() {
        // A "16 GB" GPU reports 16376 MiB.
        assert_eq!(pick_tier(16376 * 1024 * 1024, false).id, "qwen35_9b");
        // A "8 GB" card reports about 8188 MiB.
        assert_eq!(pick_tier(8188 * 1024 * 1024, false).id, "qwen35_4b");
        // Well under the threshold still steps down.
        assert_eq!(pick_tier(14 * GIB, false).id, "qwen35_4b");
        let hcl = starter_hcl(
            InitTarget::LinuxCuda,
            Some(&HostInfo {
                nvidia: Some(crate::host::NvidiaGpu {
                    model: "rtx_4080".into(),
                    count: 1,
                    memory: amicu_core::ByteSize::from_bytes(16376 * 1024 * 1024),
                }),
                ..fake::linux(64, None)
            }),
        )
        .unwrap();
        assert!(hcl.contains("Qwen3.5-9B"), "{hcl}");
    }

    /// The machines the files under `examples/` are generated for.
    fn reference(target: InitTarget) -> (HostInfo, &'static str) {
        match target {
            InitTarget::Mac => (
                fake::mac("m4_pro", 48),
                "Apple M4 Pro, 48 GiB unified memory",
            ),
            InitTarget::LinuxCuda => (
                fake::linux(64, Some(("rtx_4090", 24))),
                "Linux, 64 GiB RAM, one NVIDIA RTX 4090 (24 GiB VRAM)",
            ),
            InitTarget::LinuxCpu => (fake::linux(16, None), "Linux, 16 GiB RAM, no GPU"),
            InitTarget::Modal => (fake::linux(16, None), "no local hardware needed"),
        }
    }

    fn example_path(dir: &str, file: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples")
            .join(dir)
            .join(file)
    }

    fn example_dirs() -> [(&'static str, InitTarget); 4] {
        [
            ("mac", InitTarget::Mac),
            ("linux-cuda", InitTarget::LinuxCuda),
            ("linux-cpu", InitTarget::LinuxCpu),
            ("modal", InitTarget::Modal),
        ]
    }

    /// The expected file: a comment naming the reference machine, then exactly
    /// what `init --example` writes for it.
    fn expected_example(target: InitTarget) -> String {
        let (host, label) = reference(target);
        let body = crate::commands::init::canonical_starter(target, Some(&host)).unwrap();
        format!(
            "# Reference machine: {label}.\n# `amicu init --example` sizes the starter to your own machine instead.\n\n{body}"
        )
    }

    #[test]
    fn example_files_match_init_output_and_validate_and_plan() {
        // `AMICU_UPDATE_EXAMPLES=1 cargo test -p amicu-cli example_files` rewrites them.
        let update = std::env::var_os("AMICU_UPDATE_EXAMPLES").is_some();
        for (dir, target) in example_dirs() {
            let path = example_path(dir, "amicu.hcl");
            let want = expected_example(target);
            if update {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &want).unwrap();
            }
            let have = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(
                have, want,
                "examples/{dir}/amicu.hcl is stale; rerun with AMICU_UPDATE_EXAMPLES=1"
            );
            assert!(
                example_path(dir, "README.md").is_file(),
                "examples/{dir}/README.md"
            );

            let td = TempDir::new();
            write(&td.path, "amicu.hcl", &have);
            let project = Project::new(&td.path);
            let mut out = Vec::new();
            let code =
                commands::validate::run(&project, false, OutputFormat::Human, &mut out).unwrap();
            assert_eq!(code, 0, "{dir}: {}", utf8(out));
            let mut out = Vec::new();
            let code = commands::plan::run_with(&project, OutputFormat::Human, &mut out, |_| true)
                .unwrap();
            assert_eq!(code, 0, "{dir}: {}", utf8(out));
        }
    }
}

mod runpod;
