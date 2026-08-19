//! Contract tests: the sample plan round-trips through serde and validates
//! against the schemars-generated JSON Schema.

use amicu_plan::contract::{
    ActionKind, Artifact, Confidence, CostPerformance, Plan, PlannedAction, PolicyOutcome, Range,
    Setting,
};

fn sample_plan() -> Plan {
    Plan {
        version: Plan::VERSION.to_owned(),
        actions: vec![PlannedAction {
            kind: ActionKind::Create,
            address: "deployment.support_chat_local".to_owned(),
            artifact: Some(Artifact {
                name: "support_chat".to_owned(),
                format: Some("gguf".to_owned()),
                quantization: Some("Q4_K_M".to_owned()),
                estimated_download_size_bytes: Some(4_800_000_000),
            }),
            runtime: vec![
                Setting {
                    key: "engine".to_owned(),
                    value: "llama_cpp".to_owned(),
                },
                Setting {
                    key: "context_length".to_owned(),
                    value: "8192".to_owned(),
                },
            ],
            hardware: Some("gpu.a100_80gb".to_owned()),
            provider: Some("runpod".to_owned()),
            region: Some("eu-central".to_owned()),
            cost_performance: Some(CostPerformance {
                cost: Some(Range {
                    min: 1.10,
                    max: 1.90,
                    unit: Some("usd_per_hour".to_owned()),
                }),
                performance: Some(Range {
                    min: 40.0,
                    max: 70.0,
                    unit: Some("tokens_per_second".to_owned()),
                }),
                assumptions: vec!["batch size 1".to_owned(), "fp16 kv cache".to_owned()],
                provenance: Some("benchmark".to_owned()),
                confidence: Some(Confidence::Medium),
            }),
            required_secrets: vec!["RUNPOD_API_KEY".to_owned(), "HF_TOKEN".to_owned()],
            required_action: Some("Accept the model license on Hugging Face.".to_owned()),
        }],
        policy: Some(PolicyOutcome {
            allowed: true,
            risks: vec!["Estimated spend exceeds 80% of monthly budget.".to_owned()],
            denials: vec![],
        }),
    }
}

#[test]
fn sample_plan_roundtrips_serde() {
    let plan = sample_plan();
    let json = serde_json::to_string_pretty(&plan).unwrap();
    let back: Plan = serde_json::from_str(&json).unwrap();
    assert_eq!(plan, back);
}

#[test]
fn required_secrets_are_names_only() {
    let plan = sample_plan();
    let secrets = &plan.actions[0].required_secrets;
    // The field is a Vec<String> of names; asserting it carries the expected
    // names (and no value payload) documents the contract.
    assert_eq!(secrets, &["RUNPOD_API_KEY", "HF_TOKEN"]);
}

#[test]
fn sample_plan_validates_against_generated_schema() {
    let schema = schemars::schema_for!(Plan);
    let schema_json = serde_json::to_value(&schema).unwrap();
    let compiled = jsonschema::JSONSchema::compile(&schema_json)
        .expect("schemars output must be a valid JSON Schema");

    let instance = serde_json::to_value(sample_plan()).unwrap();
    if let Err(errors) = compiled.validate(&instance) {
        let msgs: Vec<String> = errors.map(|e| e.to_string()).collect();
        panic!("sample plan failed schema validation: {msgs:?}");
    }
}
