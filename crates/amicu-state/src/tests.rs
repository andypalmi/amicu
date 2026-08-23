//! Contract tests for [`StateBackendAdapter`], exercised against
//! [`InMemoryStateBackend`] - the crate's own zero-configuration reference
//! implementation, also what `amicu-server`'s `build_app` falls back to when
//! `DATABASE_URL` is unset. Exercising the tests directly against it (rather
//! than a test-local duplicate) keeps this suite an honest proof that the
//! production fallback satisfies the contract.

use std::sync::Arc;

use amicu_core::adapter::{AdapterError, AdapterId, Registry};
use amicu_core::id::{DeploymentId, HardwareProfileId, ModelId};
use amicu_core::resource::{
    Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Deployment, GgufVariant,
    HardwareProfile, LlamaCppSettings, Modality, Model, ModelSource, Money, Price, Quantization,
    Region, Runtime, SourceRegistry,
};
use amicu_core::units::ByteSize;
use amicu_core::{Graph, diagnostic::Severity};
use serde::{Deserialize, Serialize};

use super::{
    InMemoryStateBackend, LockHandle, STATE_CORRUPTED, STATE_LOCK_NOT_HELD, STATE_LOCKED,
    STATE_VERSION_NOT_FOUND, StateBackendAdapter, StateSnapshot, StateVersion,
};

// --- sample graph builders (mirroring amicu-core's test fixtures) -----------

fn sample_model() -> Model {
    Model {
        id: ModelId::new("support_chat"),
        source: ModelSource {
            registry: SourceRegistry::HuggingFace,
            reference: "TheBloke/Mistral-7B-Instruct-GGUF".into(),
        },
        architecture: "llama".into(),
        modalities: vec![Modality::Text],
        artifact: Artifact {
            format: ArtifactFormat::Gguf,
            quantization: Some(Quantization::Q4KM),
        },
        context_length: Some(32_768),
        variants: vec![GgufVariant {
            quantization: Quantization::Q4KM,
            file: "mistral-7b-instruct.Q4_K_M.gguf".into(),
            size: Some(ByteSize::from_bytes(4 * 1024 * 1024 * 1024)),
        }],
    }
}

fn sample_hardware() -> HardwareProfile {
    HardwareProfile {
        id: HardwareProfileId::new("a100_80gb"),
        provider: "aws".into(),
        region: Some(Region::from_code("us-east-1")),
        accelerator: Some(Accelerator {
            kind: AcceleratorKind::Gpu,
            model: "a100".into(),
            count: 1,
            memory: "80GiB".parse().unwrap(),
        }),
        cpu: None,
        memory: "128GiB".parse().unwrap(),
        interconnect: None,
        price: Some(Price {
            per_hour: Money::new(1.5, "USD"),
        }),
    }
}

fn sample_deployment() -> Deployment {
    Deployment {
        id: DeploymentId::new("support_chat_prod"),
        model: ModelId::new("support_chat"),
        hardware_profile: HardwareProfileId::new("a100_80gb"),
        runtime: Runtime::LlamaCpp(LlamaCppSettings {
            context_length: 8192,
            port: 8080,
            n_gpu_layers: -1,
        }),
    }
}

fn sample_graph() -> Graph {
    Graph {
        models: vec![sample_model()],
        hardware_profiles: vec![sample_hardware()],
        deployments: vec![sample_deployment()],
        ..Graph::new()
    }
}

fn snapshot(version: &str, graph: Graph) -> StateSnapshot {
    StateSnapshot::new(version, "2026-08-19T00:00:00Z", graph).expect("stamp checksum")
}

// --- tests -------------------------------------------------------------------

fn backend() -> InMemoryStateBackend {
    InMemoryStateBackend::with_initial_snapshot_for_test("primary", snapshot("v0", sample_graph()))
}

/// A fresh backend built with the same zero-config constructor
/// `amicu-server`'s `build_app` uses (no seeded state) reports an empty,
/// unpersisted `v0` graph rather than an error - matching
/// `PostgresStateBackend`'s "no state yet" convention so callers cannot tell
/// the two backends apart from this behavior alone.
#[tokio::test]
async fn fresh_backend_pulls_empty_v0_graph() {
    let backend = InMemoryStateBackend::new("primary");
    let pulled = backend.pull().await.expect("pull with no prior push");
    assert_eq!(pulled.version, "v0");
    assert_eq!(pulled.graph, Graph::new());
    pulled
        .verify()
        .expect("fresh empty snapshot is internally consistent");
}

#[tokio::test]
async fn lock_push_pull_round_trip() {
    let backend = backend();
    let lock = backend.lock().await.expect("acquire lock");

    let mut desired = sample_graph();
    desired.deployments[0].model = ModelId::new("support_chat");
    let next = snapshot("ignored", desired.clone());
    backend.push(&next, &lock).await.expect("push under lock");
    backend.unlock(lock).await.expect("release lock");

    let pulled = backend.pull().await.expect("pull");
    assert_eq!(pulled.graph, desired);
    pulled.verify().expect("snapshot is intact");
}

#[tokio::test]
async fn push_without_lock_is_rejected() {
    let backend = backend();
    // A handle for a lock that was never acquired must not overwrite state.
    let forged = LockHandle::new(AdapterId::new("primary"), 999);
    let err = backend
        .push(&snapshot("x", sample_graph()), &forged)
        .await
        .expect_err("push without a held lock");
    assert_eq!(err.code(), STATE_LOCK_NOT_HELD);
    assert_eq!(err.code().to_string(), "AMICU-STATE-002");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn push_with_wrong_lock_is_rejected_without_overwrite() {
    let backend = backend();
    let _real = backend.lock().await.expect("acquire lock");
    let wrong = LockHandle::new(AdapterId::new("primary"), 424_242);

    let mut tampered = sample_graph();
    tampered.deployments.clear();
    let err = backend
        .push(&snapshot("x", tampered), &wrong)
        .await
        .expect_err("wrong lock");
    assert_eq!(err.code(), STATE_LOCK_NOT_HELD);

    // State was not silently overwritten.
    let pulled = backend.pull().await.expect("pull");
    assert_eq!(pulled.graph, sample_graph());
}

#[tokio::test]
async fn lock_contention_is_explicit() {
    let backend = backend();
    let _first = backend.lock().await.expect("first lock");
    let err = backend.lock().await.expect_err("second lock contends");
    assert_eq!(err.code(), STATE_LOCKED);
    assert_eq!(err.code().to_string(), "AMICU-STATE-001");
    // Contention is transient: the holder may release the lock.
    assert!(err.is_retryable());
}

#[tokio::test]
async fn lock_tokens_are_not_sequential() {
    // The fake is deterministic yet unpredictable; a real backend must use true
    // randomness (see `LockHandle`). Mint several tokens and assert none is a
    // guessable increment of the prior one.
    const N: usize = 8;
    let backend = backend();
    let mut tokens = Vec::with_capacity(N);
    for _ in 0..N {
        let handle = backend.lock().await.expect("lock");
        tokens.push(handle.token());
        backend.unlock(handle).await.expect("unlock");
    }

    let distinct: std::collections::HashSet<u64> = tokens.iter().copied().collect();
    assert_eq!(distinct.len(), N, "all minted tokens must be distinct");
    for pair in tokens.windows(2) {
        assert_ne!(
            pair[1],
            pair[0].wrapping_add(1),
            "tokens must not be sequential increments"
        );
    }
}

/// Secret-refs-by-type (see `tests/compile_fail/secret_value_in_persisted_field.rs`
/// for the negative half of this proof): a state-shaped field that needs to
/// reference a secret must be typed `SecretRef`, never `SecretValue`. `SecretRef`
/// is metadata only (a name/scope/version pointer) and is `Serialize`, so it
/// persists and round-trips exactly like any other state field, while the
/// plaintext it points to never appears here.
#[test]
fn secret_ref_is_representable_and_round_trips_in_a_state_shaped_type() {
    use amicu_core::secret::SecretRef;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct PersistedResource {
        id: String,
        credential: SecretRef,
    }

    let resource = PersistedResource {
        id: "example".to_string(),
        credential: SecretRef::new("db_password").with_scope("prod"),
    };

    let json = serde_json::to_string(&resource).expect("SecretRef serializes");
    assert!(json.contains("db_password"));
    assert_eq!(
        serde_json::from_str::<PersistedResource>(&json).unwrap(),
        resource,
        "a SecretRef-typed field round-trips like any other persisted-state field"
    );
}

#[test]
fn checksum_uses_versioned_format_and_is_deterministic() {
    let snap = snapshot("v0", sample_graph());
    let checksum = &snap.checksum;

    // Matches ^sha256-v1:[0-9a-f]{64}$.
    assert!(
        checksum.strip_prefix("sha256-v1:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }),
        "checksum should be sha256-v1:<64 lowercase hex>, got {checksum}"
    );

    // Recomputing over the same graph yields the identical string.
    let again = snapshot("different-version", sample_graph());
    assert_eq!(snap.checksum, again.checksum);
}

#[tokio::test]
async fn corruption_is_non_retryable_and_needs_recovery() {
    let backend = backend();
    backend.set_corrupt_for_test();
    let err = backend.pull().await.expect_err("corrupt store");
    assert_eq!(err.code(), STATE_CORRUPTED);
    assert_eq!(err.code().to_string(), "AMICU-STATE-003");
    assert_eq!(err.severity(), Severity::Error);
    assert!(!err.is_retryable());
}

#[test]
fn checksum_mismatch_surfaces_as_corruption() {
    let mut snap = snapshot("v0", sample_graph());
    snap.verify().expect("freshly stamped");
    // Tamper with the graph, leaving the checksum stale.
    snap.graph.deployments.clear();
    let err = AdapterError::from(snap.verify().expect_err("tampered graph"));
    assert_eq!(err.code(), STATE_CORRUPTED);
}

#[tokio::test]
async fn history_records_pushes_and_rollback_restores() {
    let backend = backend();

    let lock = backend.lock().await.expect("lock");
    let mut g1 = sample_graph();
    g1.deployments[0].model = ModelId::new("support_chat");
    backend
        .push(&snapshot("_", g1), &lock)
        .await
        .expect("push 1");

    let mut g2 = sample_graph();
    g2.deployments.clear();
    backend
        .push(&snapshot("_", g2), &lock)
        .await
        .expect("push 2");
    backend.unlock(lock).await.expect("unlock");

    let history = backend.history().await.expect("history");
    assert_eq!(history.len(), 3); // initial + two pushes
    assert_eq!(history[0].id, "v0");
    assert_eq!(history[2].parent.as_deref(), Some("v1"));

    // Roll back to the first push (under a held lock) and confirm restoration.
    let lock = backend.lock().await.expect("relock for rollback");
    let v1 = history[1].clone();
    backend.rollback(&v1, &lock).await.expect("rollback");
    let pulled = backend.pull().await.expect("pull after rollback");
    assert_eq!(pulled.graph.deployments.len(), 1);

    // Rolling back to an unknown version is explicit.
    let missing = StateVersion::new("v404", None, "nope");
    let err = backend
        .rollback(&missing, &lock)
        .await
        .expect_err("unknown version");
    assert_eq!(err.code(), STATE_VERSION_NOT_FOUND);
    assert_eq!(err.code().to_string(), "AMICU-STATE-004");
    backend.unlock(lock).await.expect("unlock");
}

#[tokio::test]
async fn rollback_requires_a_held_lock_and_never_races_a_push() {
    let backend = backend();

    // Record a second version so there is a prior version to target.
    let lock = backend.lock().await.expect("lock");
    let mut changed = sample_graph();
    changed.deployments.clear();
    backend
        .push(&snapshot("_", changed), &lock)
        .await
        .expect("push");
    backend.unlock(lock).await.expect("unlock");

    let history = backend.history().await.expect("history");
    let v0 = history[0].clone();

    // No lock held: rollback is refused and state is untouched.
    let forged = LockHandle::new(AdapterId::new("primary"), 999);
    let err = backend
        .rollback(&v0, &forged)
        .await
        .expect_err("rollback without a held lock");
    assert_eq!(err.code(), STATE_LOCK_NOT_HELD);
    assert!(!err.is_retryable());
    assert!(
        backend
            .pull()
            .await
            .expect("pull")
            .graph
            .deployments
            .is_empty()
    );

    // Lock held by another handle, wrong token: still refused, still untouched.
    let real = backend.lock().await.expect("lock");
    let wrong = LockHandle::new(AdapterId::new("primary"), 424_242);
    let err = backend
        .rollback(&v0, &wrong)
        .await
        .expect_err("rollback with the wrong lock");
    assert_eq!(err.code(), STATE_LOCK_NOT_HELD);
    assert!(
        backend
            .pull()
            .await
            .expect("pull")
            .graph
            .deployments
            .is_empty()
    );

    // The genuine held lock authorizes the rollback.
    backend
        .rollback(&v0, &real)
        .await
        .expect("rollback under lock");
    assert_eq!(
        backend.pull().await.expect("pull").graph.deployments.len(),
        1
    );
}

#[test]
fn detect_drift_is_identical_when_graphs_match() {
    let backend = backend();
    let report = backend.detect_drift(&sample_graph(), &snapshot("v0", sample_graph()));
    assert!(report.is_empty());
    assert!(!report.has_drift());
}

#[test]
fn detect_drift_reports_added_removed_changed() {
    let backend = backend();

    // Recorded state: the sample graph.
    let recorded = snapshot("v0", sample_graph());

    // Desired: drop the model (removed), change the deployment, add a use case.
    let mut desired = sample_graph();
    desired.models.clear();
    desired.deployments[0].hardware_profile = HardwareProfileId::new("a100_80gb_v2");
    desired.use_cases.push(amicu_core::resource::UseCase {
        id: amicu_core::id::UseCaseId::new("customer_support"),
        description: None,
        modalities: vec![Modality::Text],
        min_context_length: None,
        max_latency_ms: None,
        required_data_class: None,
        priority: None,
    });

    let report = backend.detect_drift(&desired, &recorded);
    assert_eq!(report.added, vec!["use_case.customer_support".to_string()]);
    assert_eq!(report.removed, vec!["model.support_chat".to_string()]);
    assert_eq!(
        report.changed,
        vec!["deployment.support_chat_prod".to_string()]
    );
}

#[tokio::test]
async fn registry_holds_state_backends_and_discovers_capabilities() {
    let mut registry: Registry<dyn StateBackendAdapter> = Registry::new();
    registry.register(Arc::new(backend()));

    assert_eq!(registry.len(), 1);
    let got = registry
        .get(&AdapterId::new("primary"))
        .expect("registered");
    assert!(got.health().await.expect("health").is_healthy());

    let discovered = registry.discover();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].capabilities["supports_rollback"], true);
}
