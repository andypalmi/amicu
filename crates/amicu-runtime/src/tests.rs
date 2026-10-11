//! A fake runtime exercising the [`RuntimeAdapter`] contract end to end.
//!
//! `FakeRuntime` mirrors the intended real adapters: pure planning that renders
//! a deterministic, secret-free command from a [`Deployment`], and apply-time
//! methods that resolve secrets through a [`SecretResolver`] before returning
//! canned lifecycle data.

use std::collections::HashMap;
use std::sync::Arc;

use amicu_core::ByteSize;
use amicu_core::adapter::{
    Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus, Registry,
};
use amicu_core::diagnostic::{Diagnostic, Severity, catalog};
use amicu_core::id::{DeploymentId, HardwareProfileId, ModelId};
use amicu_core::resource::{
    ArtifactFormat, Deployment, LlamaCppSettings, Modality, Quantization, Runtime, VllmSettings,
};
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use async_trait::async_trait;

use crate::{
    LaunchPlan, LogOptions, LogStream, Parallelism, Platform, RenderedLaunch, RuntimeAdapter,
    RuntimeCapabilities, RuntimeError, RuntimeHandle, RuntimeKind, RuntimeMetrics, RuntimeStatus,
};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// A local, minimal [`SecretResolver`] for tests. It never leaks into a plan.
struct FakeResolver {
    entries: HashMap<String, String>,
}

impl FakeResolver {
    fn with(name: &str, value: &str) -> Self {
        let mut entries = HashMap::new();
        entries.insert(name.to_owned(), value.to_owned());
        Self { entries }
    }
}

#[async_trait]
impl SecretResolver for FakeResolver {
    async fn resolve(&self, secret: &SecretRef) -> Result<SecretValue, AdapterError> {
        self.entries
            .get(&secret.name)
            .map(|v| SecretValue::new(v.clone()))
            .ok_or_else(|| {
                AdapterError::from_kind(RuntimeError::LaunchFailed(format!(
                    "secret '{}' is unavailable",
                    secret.name
                )))
            })
    }
}

struct FakeRuntime {
    id: AdapterId,
    kind: RuntimeKind,
    supported_formats: Vec<ArtifactFormat>,
    /// Artifact format the deployment's model provides (resolved by the planner
    /// in real adapters; supplied directly to the fake).
    model_format: ArtifactFormat,
    capacity: ByteSize,
    api_key: Option<SecretRef>,
}

impl FakeRuntime {
    fn llama_cpp() -> Self {
        Self {
            id: AdapterId::new("llama_cpp"),
            kind: RuntimeKind::LlamaCpp,
            supported_formats: vec![ArtifactFormat::Gguf],
            model_format: ArtifactFormat::Gguf,
            capacity: "80GiB".parse().unwrap(),
            api_key: Some(SecretRef::new("hf_token")),
        }
    }

    fn with_model_format(mut self, format: ArtifactFormat) -> Self {
        self.model_format = format;
        self
    }

    fn with_capacity(mut self, capacity: ByteSize) -> Self {
        self.capacity = capacity;
        self
    }

    fn secret_refs(&self) -> Vec<SecretRef> {
        self.api_key.iter().cloned().collect()
    }

    fn estimate(d: &Deployment) -> crate::ResourceEstimate {
        let (ctx, gpus) = match &d.runtime {
            Runtime::LlamaCpp(s) => (s.context_length, 1),
            Runtime::Vllm(s) => (s.context_length, s.tensor_parallel_size.unwrap_or(1)),
        };
        let bytes = 512 * MIB + u64::from(ctx) * 256 * KIB;
        crate::ResourceEstimate {
            estimated_memory: ByteSize::from_bytes(bytes),
            gpu_count: gpus,
            kv_cache: None,
        }
    }

    fn render(&self, d: &Deployment) -> RenderedLaunch {
        use std::fmt::Write as _;
        let refs = self.secret_refs();
        match &d.runtime {
            Runtime::LlamaCpp(s) => {
                let mut argv = vec![
                    "llama-server".to_owned(),
                    "--port".to_owned(),
                    s.port.to_string(),
                    "--ctx-size".to_owned(),
                    s.context_length.to_string(),
                    "--n-gpu-layers".to_owned(),
                    s.n_gpu_layers.to_string(),
                ];
                for r in &refs {
                    argv.push("--api-key-ref".to_owned());
                    argv.push(r.name.clone());
                }
                let text = argv.join(" ");
                RenderedLaunch::Command { argv, text }
            }
            Runtime::Vllm(s) => {
                let mut text = format!(
                    "runtime: vllm\nport: {}\ncontext_length: {}\ntensor_parallel_size: {}\n",
                    s.port,
                    s.context_length,
                    s.tensor_parallel_size.unwrap_or(1)
                );
                for r in &refs {
                    let _ = writeln!(text, "api_key_ref: {}", r.name);
                }
                RenderedLaunch::Manifest { text }
            }
        }
    }
}

#[async_trait]
impl Adapter for FakeRuntime {
    fn id(&self) -> AdapterId {
        self.id.clone()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        Ok(HealthStatus::Healthy)
    }
}

impl DescribeCapabilities for FakeRuntime {
    fn describe(&self) -> serde_json::Value {
        serde_json::to_value(self.capabilities()).unwrap_or_default()
    }
}

#[async_trait]
impl RuntimeAdapter for FakeRuntime {
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            runtime: self.kind,
            artifact_formats: self.supported_formats.clone(),
            platforms: vec![Platform::Cpu, Platform::Cuda, Platform::Metal],
            parallelism: Parallelism {
                tensor: matches!(self.kind, RuntimeKind::Vllm),
                pipeline: matches!(self.kind, RuntimeKind::Vllm),
                max_gpus: 8,
            },
            quantizations: vec![Quantization::Q4KM, Quantization::Q8_0, Quantization::F16],
            lora: true,
            tool_calling: true,
            multimodal: vec![Modality::Text],
        }
    }

    fn validate(&self, _d: &Deployment) -> Vec<Diagnostic> {
        let mut diags = Vec::new();
        if !self.supported_formats.contains(&self.model_format) {
            diags.push(Diagnostic::new(
                catalog::CAP_RUNTIME_ARTIFACT_MISMATCH,
                Severity::Error,
                format!(
                    "Runtime '{}' does not support artifact format {:?}.",
                    self.kind, self.model_format
                ),
            ));
        }
        diags
    }

    fn plan_launch(&self, d: &Deployment) -> Result<LaunchPlan, AdapterError> {
        if !self.supported_formats.contains(&self.model_format) {
            return Err(AdapterError::from_kind(
                RuntimeError::IncompatibleArtifact {
                    runtime: self.kind,
                    format: self.model_format,
                },
            ));
        }
        let estimate = Self::estimate(d);
        if estimate.estimated_memory > self.capacity {
            return Err(AdapterError::from_kind(RuntimeError::InsufficientMemory {
                required: estimate.estimated_memory,
                available: self.capacity,
            }));
        }
        Ok(LaunchPlan {
            deployment: d.id.clone(),
            runtime: RuntimeKind::of(&d.runtime),
            rendered: self.render(d),
            settings: d.runtime.clone(),
            secret_refs: self.secret_refs(),
            estimate,
        })
    }

    async fn apply(
        &self,
        p: &LaunchPlan,
        s: &dyn SecretResolver,
    ) -> Result<RuntimeHandle, AdapterError> {
        // Apply-time secret resolution: values enter here and never reach the plan.
        for r in &p.secret_refs {
            let value = s.resolve(r).await?;
            if value.is_empty() {
                return Err(AdapterError::from_kind(RuntimeError::LaunchFailed(
                    format!("secret '{}' resolved empty", r.name),
                )));
            }
        }
        let port = match &p.settings {
            Runtime::LlamaCpp(s) => s.port,
            Runtime::Vllm(s) => s.port,
        };
        Ok(RuntimeHandle {
            id: format!("fake-{}", p.deployment),
            deployment: p.deployment.clone(),
            endpoint: format!("http://127.0.0.1:{port}"),
        })
    }

    async fn status(&self, _h: &RuntimeHandle) -> Result<RuntimeStatus, AdapterError> {
        Ok(RuntimeStatus::Running {
            detail: "up 5s".to_owned(),
        })
    }

    async fn metrics(&self, _h: &RuntimeHandle) -> Result<RuntimeMetrics, AdapterError> {
        Ok(RuntimeMetrics {
            requests_total: 42,
            latency_p50_ms: 12.5,
            tokens_per_second: 87.0,
            memory_used: ByteSize::from_bytes(3 * 1024 * MIB),
        })
    }

    async fn logs(&self, _h: &RuntimeHandle, o: LogOptions) -> Result<LogStream, AdapterError> {
        let mut lines = vec!["loaded weights".to_owned(), "listening".to_owned()];
        if let Some(tail) = o.tail {
            let keep = (tail as usize).min(lines.len());
            lines = lines.split_off(lines.len() - keep);
        }
        Ok(LogStream(lines))
    }

    async fn stop(&self, _h: &RuntimeHandle) -> Result<(), AdapterError> {
        Ok(())
    }
}

fn llama_deployment(context_length: u32) -> Deployment {
    Deployment {
        scaling: None,
        id: DeploymentId::new("support_chat_local"),
        model: ModelId::new("support_chat"),
        hardware_profile: HardwareProfileId::new("a100_80gb"),
        runtime: Runtime::LlamaCpp(LlamaCppSettings {
            context_length,
            port: 8080,
            n_gpu_layers: -1,
            ..Default::default()
        }),
    }
}

fn vllm_deployment() -> Deployment {
    Deployment {
        scaling: None,
        id: DeploymentId::new("support_chat_gpu"),
        model: ModelId::new("support_chat"),
        hardware_profile: HardwareProfileId::new("a100_80gb"),
        runtime: Runtime::Vllm(VllmSettings {
            context_length: 8192,
            port: 8000,
            tensor_parallel_size: Some(2),
            gpu_memory_utilization: Some(0.9),
            ..Default::default()
        }),
    }
}

#[test]
fn capabilities_describe_runtime_family() {
    let caps = FakeRuntime::llama_cpp().capabilities();
    assert_eq!(caps.runtime, RuntimeKind::LlamaCpp);
    assert!(caps.artifact_formats.contains(&ArtifactFormat::Gguf));
    assert!(caps.platforms.contains(&Platform::Metal));
    assert!(caps.lora && caps.tool_calling);
    assert_eq!(caps.parallelism.max_gpus, 8);
}

#[test]
fn validate_is_pure_and_needs_no_runtime() {
    // No tokio runtime in scope: validate and plan_launch are pure sync fns.
    let good = FakeRuntime::llama_cpp();
    assert_eq!(good.validate(&llama_deployment(8192)), Vec::new());

    let bad = FakeRuntime::llama_cpp().with_model_format(ArtifactFormat::Safetensors);
    let diags = bad.validate(&llama_deployment(8192));
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code, catalog::CAP_RUNTIME_ARTIFACT_MISMATCH);
    assert_eq!(diags[0].code.to_string(), "AMICU-CAP-001");
    assert_eq!(diags[0].severity, Severity::Error);
}

#[test]
fn plan_launch_is_pure_and_hides_secret_values() {
    // Runs without a tokio runtime, proving plan_launch has no async work.
    let adapter = FakeRuntime::llama_cpp();
    let plan = adapter.plan_launch(&llama_deployment(8192)).expect("plan");

    assert_eq!(plan.deployment, DeploymentId::new("support_chat_local"));
    assert_eq!(plan.runtime, RuntimeKind::LlamaCpp);

    let text = plan.rendered.as_text();
    assert!(text.contains("llama-server --port 8080"));
    // Secrets appear by reference name only, never as values.
    assert!(text.contains("--api-key-ref hf_token"));
    assert_eq!(plan.secret_refs, vec![SecretRef::new("hf_token")]);
    assert!(!text.contains("secret-token-value"));

    // The whole serialized plan carries no resolved secret value.
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains("secret-token-value"));
}

#[test]
fn plan_launch_renders_vllm_manifest() {
    let adapter = FakeRuntime {
        id: AdapterId::new("vllm"),
        kind: RuntimeKind::Vllm,
        supported_formats: vec![ArtifactFormat::Safetensors],
        model_format: ArtifactFormat::Safetensors,
        capacity: "80GiB".parse().unwrap(),
        api_key: Some(SecretRef::new("hf_token")),
    };
    let plan = adapter.plan_launch(&vllm_deployment()).expect("plan");
    assert!(matches!(plan.rendered, RenderedLaunch::Manifest { .. }));
    assert_eq!(plan.estimate.gpu_count, 2);
    assert!(plan.rendered.as_text().contains("tensor_parallel_size: 2"));
}

#[test]
fn capacity_overflow_surfaces_mem_error() {
    let adapter = FakeRuntime::llama_cpp().with_capacity(ByteSize::from_bytes(MIB));
    let err = adapter
        .plan_launch(&llama_deployment(32_768))
        .expect_err("over capacity");
    assert_eq!(err.code(), catalog::MEM_VRAM_EXCEEDED);
    assert_eq!(err.code().to_string(), "AMICU-MEM-001");
    assert!(!err.is_retryable());
}

#[test]
fn error_enum_maps_cap_and_mem_and_provider() {
    let cap = AdapterError::from_kind(RuntimeError::IncompatibleArtifact {
        runtime: RuntimeKind::LlamaCpp,
        format: ArtifactFormat::Safetensors,
    });
    assert_eq!(cap.code(), catalog::CAP_RUNTIME_ARTIFACT_MISMATCH);
    assert!(!cap.is_retryable());

    let mem = AdapterError::from_kind(RuntimeError::InsufficientMemory {
        required: ByteSize::from_bytes(2 * MIB),
        available: ByteSize::from_bytes(MIB),
    });
    assert_eq!(mem.code(), catalog::MEM_VRAM_EXCEEDED);

    let started = AdapterError::from_kind(RuntimeError::LaunchFailed("oom".into()));
    assert_eq!(started.code(), catalog::PROVIDER_CAPACITY_UNAVAILABLE);
    assert!(started.is_retryable());
}

#[tokio::test]
async fn full_lifecycle_apply_status_metrics_logs_stop() {
    let adapter = FakeRuntime::llama_cpp();
    let plan = adapter.plan_launch(&llama_deployment(8192)).expect("plan");
    let resolver = FakeResolver::with("hf_token", "secret-token-value");

    let handle = adapter.apply(&plan, &resolver).await.expect("apply");
    assert_eq!(handle.id, "fake-support_chat_local");
    assert_eq!(handle.endpoint, "http://127.0.0.1:8080");

    let status = adapter.status(&handle).await.expect("status");
    assert!(status.is_running());

    let metrics = adapter.metrics(&handle).await.expect("metrics");
    assert_eq!(metrics.requests_total, 42);

    let logs = adapter
        .logs(
            &handle,
            LogOptions {
                follow: false,
                tail: Some(1),
            },
        )
        .await
        .expect("logs");
    assert_eq!(logs.lines(), ["listening".to_owned()]);

    adapter.stop(&handle).await.expect("stop");
}

#[tokio::test]
async fn apply_fails_when_secret_missing() {
    let adapter = FakeRuntime::llama_cpp();
    let plan = adapter.plan_launch(&llama_deployment(8192)).expect("plan");
    let resolver = FakeResolver::with("other", "x");

    let err = adapter
        .apply(&plan, &resolver)
        .await
        .expect_err("missing secret");
    assert_eq!(err.code(), catalog::PROVIDER_CAPACITY_UNAVAILABLE);
}

#[tokio::test]
async fn health_reports_without_side_effects() {
    let adapter = FakeRuntime::llama_cpp();
    assert_eq!(adapter.id(), AdapterId::new("llama_cpp"));
    assert!(adapter.health().await.expect("health").is_healthy());
}

#[test]
fn registry_registers_and_discovers_runtime_adapters() {
    let mut registry: Registry<dyn RuntimeAdapter> = Registry::new();
    registry.register(Arc::new(FakeRuntime::llama_cpp()));
    assert_eq!(registry.len(), 1);

    let found = registry
        .get(&AdapterId::new("llama_cpp"))
        .expect("registered");
    assert_eq!(found.capabilities().runtime, RuntimeKind::LlamaCpp);

    let discovered = registry.discover();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].id, AdapterId::new("llama_cpp"));
    assert_eq!(discovered[0].capabilities["runtime"], "llama_cpp");
}
