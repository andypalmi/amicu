//! Guard that the committed `crates/amicu-schema/openapi.json` artifact stays in
//! lock-step with the code-derived spec.
//!
//! Regenerate after intentional API changes with:
//! `AMICU_BLESS_OPENAPI=1 cargo test -p amicu-api --test openapi`

use std::path::PathBuf;

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../amicu-schema/openapi.json")
}

#[test]
fn openapi_json_matches_committed_artifact() {
    let generated = amicu_api::openapi_json();
    let path = schema_path();

    if std::env::var_os("AMICU_BLESS_OPENAPI").is_some() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create schema dir");
        }
        std::fs::write(&path, &generated).expect("write openapi.json");
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\nRegenerate with: AMICU_BLESS_OPENAPI=1 cargo test -p amicu-api --test openapi",
            path.display()
        )
    });

    assert_eq!(
        generated, committed,
        "openapi.json is out of date.\nRegenerate with: AMICU_BLESS_OPENAPI=1 cargo test -p amicu-api --test openapi"
    );
}

#[test]
fn openapi_generation_is_deterministic() {
    // Stable ordering: two independent renders are byte-identical.
    assert_eq!(amicu_api::openapi_json(), amicu_api::openapi_json());
}

#[test]
fn openapi_documents_engine_and_auth_paths() {
    let doc = amicu_api::api_openapi();
    let json = serde_json::to_value(&doc).expect("serialize");
    let paths = json["paths"].as_object().expect("paths object");
    for expected in ["/validate", "/plan", "/signup", "/token", "/token/refresh"] {
        assert!(paths.contains_key(expected), "missing path {expected}");
    }
    // The bearer security scheme is registered.
    assert!(
        json["components"]["securitySchemes"]["bearer"].is_object(),
        "bearer security scheme missing"
    );
}
