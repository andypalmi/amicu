//! Live-database integration tests for [`PostgresStateBackend`].
//!
//! `#[ignore]`d so `cargo test --workspace` skips them when no Postgres is
//! available (as in CI). Run against a live database with:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://amicu:amicu@localhost:5432/amicu \
//!     cargo test -p amicu-store -- --ignored
//! ```
//!
//! Each test uses a freshly-random `stack_id` (via [`unique`]) so tests can
//! share one database and one workspace without colliding: `stack_id` scopes
//! `state_snapshots`/`state_current`/`state_locks` alongside `workspace_id`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use amicu_core::Graph;
use amicu_core::adapter::AdapterId;
use amicu_core::id::{DeploymentId, HardwareProfileId, ModelId};
use amicu_core::resource::{
    Accelerator, AcceleratorKind, Artifact, ArtifactFormat, Deployment, GgufVariant,
    HardwareProfile, LlamaCppSettings, Modality, Model, ModelSource, Money, Price, Quantization,
    Region, Runtime, SourceRegistry,
};
use amicu_core::units::ByteSize;
use amicu_state::{
    LockHandle, STATE_LOCK_NOT_HELD, STATE_LOCKED, STATE_VERSION_NOT_FOUND, StateBackendAdapter,
    StateSnapshot, StateVersion,
};
use amicu_store::{PostgresStateBackend, WorkspaceId, connect, run_migrations};
use sqlx::PgPool;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A collision-resistant unique suffix for per-test identifiers.
fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos}_{n}")
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for the ignored store tests");
    let pool = connect(&url).await.expect("connect to Postgres");
    run_migrations(&pool).await.expect("apply migrations");
    pool
}

/// Provisions a `workspaces` row for a freshly-minted [`WorkspaceId`], so the
/// state tables' `workspace_id` foreign key is satisfiable. A real deployment
/// provisions this at tenant sign-up; tests stand it up directly.
async fn provision_workspace(pool: &PgPool, workspace: WorkspaceId) {
    sqlx::query!(
        "INSERT INTO workspaces (id, slug, name, tier, created_at) \
         VALUES ($1, $2, 'Test workspace', 'community', 0) \
         ON CONFLICT (id) DO NOTHING",
        workspace.as_uuid(),
        unique("ws"),
    )
    .execute(pool)
    .await
    .expect("provision workspace");
}

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

fn snapshot(graph: Graph) -> StateSnapshot {
    // The version placeholder is discarded by `push`, which mints its own.
    StateSnapshot::new("ignored", "2026-08-19T00:00:00Z", graph).expect("stamp checksum")
}

fn backend(pool: PgPool, workspace: WorkspaceId, stack: &str) -> PostgresStateBackend {
    PostgresStateBackend::with_workspace(pool, workspace, AdapterId::new(stack))
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn pull_with_no_pushed_state_returns_empty_graph() {
    let backend = backend(pool().await, WorkspaceId::DEFAULT, &unique("stack"));
    let pulled = backend.pull().await.expect("pull with no history");
    assert_eq!(pulled.graph, Graph::new());
    pulled.verify().expect("empty snapshot is self-consistent");
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn lock_push_pull_round_trip_preserves_checksum() {
    let backend = backend(pool().await, WorkspaceId::DEFAULT, &unique("stack"));

    let lock = backend.lock().await.expect("acquire lock");
    backend
        .push(&snapshot(sample_graph()), &lock)
        .await
        .expect("push under lock");
    backend.unlock(lock).await.expect("release lock");

    let pulled = backend.pull().await.expect("pull");
    assert_eq!(pulled.graph, sample_graph());
    pulled.verify().expect("checksum matches recomputed hash");
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn push_appends_history_without_mutating_prior_snapshots() {
    let pool = pool().await;
    let stack = unique("stack");
    let backend = backend(pool, WorkspaceId::DEFAULT, &stack);

    let lock = backend.lock().await.expect("lock");
    let mut g1 = sample_graph();
    g1.deployments.clear();
    backend
        .push(&snapshot(g1.clone()), &lock)
        .await
        .expect("push 1");

    let mut g2 = sample_graph();
    g2.models.clear();
    backend
        .push(&snapshot(g2.clone()), &lock)
        .await
        .expect("push 2");
    backend.unlock(lock).await.expect("unlock");

    let history = backend.history().await.expect("history");
    assert_eq!(history.len(), 2, "both pushes are recorded, in order");
    assert_eq!(history[0].parent, None, "first push has no parent");
    assert_eq!(
        history[1].parent.as_deref(),
        Some(history[0].id.as_str()),
        "second push chains to the first"
    );

    // The current graph is the second push's; the first push's content still
    // exists in history and was never mutated (rollback below confirms this).
    let current = backend.pull().await.expect("pull");
    assert_eq!(current.graph, g2);

    let lock = backend.lock().await.expect("relock for rollback");
    let v1 = history[0].clone();
    backend
        .rollback(&v1, &lock)
        .await
        .expect("rollback to first push");
    backend.unlock(lock).await.expect("unlock");
    let restored = backend.pull().await.expect("pull after rollback");
    assert_eq!(
        restored.graph, g1,
        "the first push's content is intact, unmutated by the second push or the rollback"
    );
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn second_lock_while_live_is_rejected_and_retryable() {
    let pool = pool().await;
    let stack = unique("stack");
    let a = backend(pool.clone(), WorkspaceId::DEFAULT, &stack);
    let b = backend(pool, WorkspaceId::DEFAULT, &stack);

    let _held = a.lock().await.expect("first lock");
    let err = b.lock().await.expect_err("second lock contends");
    assert_eq!(err.code(), STATE_LOCKED);
    assert_eq!(err.code().to_string(), "AMICU-STATE-001");
    assert!(err.is_retryable());
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn expired_lease_is_reacquirable() {
    let pool = pool().await;
    let stack = unique("stack");
    // A lease that is already expired the instant it is minted (negative TTL),
    // so a second acquire observes staleness deterministically, with no sleep.
    let a = backend(pool.clone(), WorkspaceId::DEFAULT, &stack).with_lease_ttl_seconds(-1);
    let b = backend(pool, WorkspaceId::DEFAULT, &stack);

    let first = a.lock().await.expect("first lock (immediately stale)");
    let second = b
        .lock()
        .await
        .expect("second lock reclaims the expired lease");
    assert_ne!(first.token(), second.token(), "a fresh token is minted");

    // The original (now-stale) handle no longer authorizes anything.
    let err = a
        .push(&snapshot(sample_graph()), &first)
        .await
        .expect_err("stale handle cannot push");
    assert_eq!(err.code(), STATE_LOCK_NOT_HELD);

    b.unlock(second).await.expect("release the live lease");
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn unlock_requires_the_matching_token_then_frees_the_lease() {
    let pool = pool().await;
    let stack = unique("stack");
    let backend = backend(pool, WorkspaceId::DEFAULT, &stack);

    let lock = backend.lock().await.expect("lock");
    let forged = LockHandle::new(AdapterId::new(&stack), lock.token().wrapping_add(1));
    let err = backend
        .unlock(forged)
        .await
        .expect_err("wrong token is rejected");
    assert_eq!(err.code(), STATE_LOCK_NOT_HELD);

    backend.unlock(lock).await.expect("genuine unlock succeeds");

    // A fresh lock() proves the lease was actually freed.
    let relocked = backend.lock().await.expect("lease is free again");
    backend.unlock(relocked).await.expect("cleanup");
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn push_without_lock_is_rejected_and_state_untouched() {
    let backend = backend(pool().await, WorkspaceId::DEFAULT, &unique("stack"));
    let forged = LockHandle::new(AdapterId::new("primary"), 999);
    let err = backend
        .push(&snapshot(sample_graph()), &forged)
        .await
        .expect_err("push without a held lock");
    assert_eq!(err.code(), STATE_LOCK_NOT_HELD);
    assert_eq!(err.code().to_string(), "AMICU-STATE-002");
    assert!(!err.is_retryable());

    // Never silently written: pull still reports the empty implicit state.
    let pulled = backend.pull().await.expect("pull");
    assert_eq!(pulled.graph, Graph::new());
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn rollback_to_unknown_version_is_explicit() {
    let backend = backend(pool().await, WorkspaceId::DEFAULT, &unique("stack"));
    let lock = backend.lock().await.expect("lock");
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
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn cross_tenant_isolation_state_invisible_to_other_workspace() {
    let pool = pool().await;
    let stack = unique("stack");
    let workspace_a = WorkspaceId::DEFAULT;
    let workspace_b = WorkspaceId::new();
    provision_workspace(&pool, workspace_b).await;

    let a = backend(pool.clone(), workspace_a, &stack);
    let lock = a.lock().await.expect("lock in workspace a");
    a.push(&snapshot(sample_graph()), &lock)
        .await
        .expect("push in workspace a");
    a.unlock(lock).await.expect("unlock");

    // Workspace B shares the same stack_id but must not see A's state: the
    // workspace_id predicate (and the RLS backstop) scope it out entirely.
    let b = backend(pool.clone(), workspace_b, &stack);
    let pulled_b = b.pull().await.expect("pull in workspace b");
    assert_eq!(
        pulled_b.graph,
        Graph::new(),
        "workspace b sees no state at all, not workspace a's graph"
    );
    assert!(
        b.history()
            .await
            .expect("history in workspace b")
            .is_empty(),
        "workspace b's history is empty"
    );

    // B can freely acquire its own lock even though A never released contention
    // logic is workspace-scoped too.
    let lock_b = b
        .lock()
        .await
        .expect("workspace b has its own independent lock");
    b.unlock(lock_b).await.expect("unlock b");

    // A's state is still intact and unaffected.
    let pulled_a = a.pull().await.expect("pull in workspace a");
    assert_eq!(pulled_a.graph, sample_graph());
}

#[tokio::test]
#[ignore = "requires a live Postgres via DATABASE_URL; run with -- --ignored"]
async fn detect_drift_reports_added_removed_changed_against_pulled_state() {
    let backend = backend(pool().await, WorkspaceId::DEFAULT, &unique("stack"));
    let lock = backend.lock().await.expect("lock");
    backend
        .push(&snapshot(sample_graph()), &lock)
        .await
        .expect("push baseline");
    backend.unlock(lock).await.expect("unlock");

    let recorded = backend.pull().await.expect("pull recorded state");

    // Desired: drop the model (removed), retarget the deployment's hardware
    // profile (changed), add a use case (added).
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
    assert!(report.has_drift());
}
