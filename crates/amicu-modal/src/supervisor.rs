//! Bridges vLLM's local-launch renderer onto Modal, so `amicu apply` can
//! start a `provider = "modal"` deployment through the same
//! `VllmAdapter`-shaped seam it uses for a local process, without
//! `amicu-vllm` (or `amicu-engine`, which does not depend on this crate at
//! all - see the packaging note on [`crate`]) ever knowing Modal exists.
//!
//! [`ModalSupervisor`] implements [`amicu_llamacpp::ProcessSupervisor`] - the
//! same trait [`amicu_llamacpp::host_supervisor`] returns for a local
//! launchd/systemd process, and the trait `amicu-vllm`'s `VllmAdapter` is
//! already generic over (`Box<dyn ProcessSupervisor>`). So `amicu-cli`'s
//! `apply` command selects a supervisor *per deployment* - this one when the
//! hardware profile has `provider = "modal"`, `host_supervisor()` otherwise -
//! and builds a `VllmAdapter` over whichever applies, rather than either
//! crate branching on provider internally.
//!
//! No bridging runtime is needed to implement the trait: `ProcessSupervisor`
//! is a genuine `#[async_trait]`, and `amicu-cli`'s `apply` command is itself
//! driven from an async fn (`block_on` at the top, not inside this call), so
//! [`ModalSupervisor`]'s methods call [`ModalClient`]'s async RPCs directly.
//!
//! Mapping a vLLM launch onto a Modal [`ServerSpec`] (see each item's own doc
//! comment for the reasoning): `argv` is `RenderedLaunch::Command`'s, with
//! `--host 127.0.0.1` rewritten to `0.0.0.0` ([`bind_all_interfaces`] - Modal
//! reaches the container over its internal network, never loopback); the
//! model reference must be a Hugging Face repo id, not a local path Modal's
//! container cannot see ([`reject_local_path_model`]); `gpu` is the hardware
//! profile's [`Accelerator`] rendered as Modal's `"<MODEL>:<count>"` string
//! ([`gpu_string`] - a **hand-maintained coupling**, not validated against
//! `amicu-providers::modal`'s GPU catalog, although this crate does now
//! depend on `amicu-providers` for [`modal_proxy_region`]); `image` is [`DEFAULT_VLLM_IMAGE`], overridable via
//! [`VLLM_IMAGE_ENV_VAR`] (no HCL schema seam exists for it yet); `secret_env`
//! carries only resolved [`LaunchPlan::secret_refs`] (never a plain
//! `vllm { env = {...} }` entry - those become baked-in image env) plus the
//! [`HF_TOKEN_ENV_VAR`] fallback documented on that constant.
//!
//! # llama.cpp
//!
//! A `llama_cpp` launch goes through the same `build_spec`, selected by
//! `plan.settings`. What differs, and why:
//!
//! - **Image**: [`DEFAULT_LLAMACPP_IMAGE`] (overridable via
//!   [`LLAMACPP_IMAGE_ENV_VAR`]). Its `ENTRYPOINT` (`/app/llama-server`) is
//!   already cleared by [`crate::ImageSpec`]'s `ENTRYPOINT []`, so Modal's
//!   runtime is PID 1; the image has no Python, so
//!   [`llamacpp_setup_commands`] installs it and puts `llama-server` on `PATH`.
//! - **GGUF delivery**: the llama.cpp renderer emits `-m <local path>`, which
//!   cannot exist in the container. `build_spec` replaces it with
//!   `-hf <repo> -hff <file>` from the model's Hugging Face source
//!   ([`ModalSupervisor::with_gguf_model`]); `LLAMA_CACHE` points at a Modal
//!   Volume ([`LLAMACPP_VOLUMES`]) so a cold start does not re-download. A
//!   model that is not on Hugging Face is rejected with a clear error.
//!   `HF_TOKEN` is forwarded through `secret_env` exactly as for vLLM.

use std::collections::BTreeMap;
use std::time::Duration;

use amicu_core::ByteSize;
use amicu_core::resource::ScalingSettings;
use amicu_core::resource::{
    Accelerator, AcceleratorKind, HardwareProfile, Model, Runtime, SourceRegistry,
};
use amicu_llamacpp::{LlamaCppError, ProcessSupervisor, ResolvedSecret};
use amicu_registry::GgufResolver;
use amicu_runtime::{
    LaunchPlan, LogOptions, LogStream, RuntimeHandle, RuntimeKind, RuntimeMetrics, RuntimeStatus,
};
use amicu_telemetry::hardware::HardwareSample;
use async_trait::async_trait;

use amicu_providers::modal::modal_proxy_region;

use crate::client::ModalClient;
use crate::credentials::Credentials;
use crate::deploy::ServerSpec;
use crate::error::ModalError;
use crate::image::ImageSpec;

/// The vLLM base image `ModalSupervisor::start` builds unless overridden by
/// [`VLLM_IMAGE_ENV_VAR`]. Must already contain Python and vLLM on `PATH`
/// (see `crate::image::ImageSpec::base_registry_image`'s doc comment) - the
/// official `vllm/vllm-openai` tags do.
pub const DEFAULT_VLLM_IMAGE: &str = "vllm/vllm-openai:v0.29.0";

/// Environment variable overriding [`DEFAULT_VLLM_IMAGE`]. There is no HCL
/// schema seam for this today (see the module docs), so this is the entire
/// override surface. Read fresh by [`ProcessSupervisor::start`] on every
/// call, not cached - the supervisor itself is stateless between calls, so
/// there is nothing to invalidate if the value changes between deploys.
pub const VLLM_IMAGE_ENV_VAR: &str = "AMICU_MODAL_VLLM_IMAGE";

/// The llama.cpp base image used for `llama_cpp` launches unless overridden
/// by [`LLAMACPP_IMAGE_ENV_VAR`]: the official CUDA server build from
/// `ghcr.io/ggml-org/llama.cpp` (Ubuntu 24.04, CUDA 12.8.1, amd64). The tag
/// was verified to exist in the ghcr.io registry (manifest list with amd64)
/// on 2026-10-10; the image ships `llama-server` at `/app/llama-server` with
/// `ENTRYPOINT ["/app/llama-server"]` and no Python (see
/// [`llamacpp_setup_commands`]).
pub const DEFAULT_LLAMACPP_IMAGE: &str = "ghcr.io/ggml-org/llama.cpp:server-cuda-b11515";

/// Environment variable overriding [`DEFAULT_LLAMACPP_IMAGE`]; same
/// semantics as [`VLLM_IMAGE_ENV_VAR`].
pub const LLAMACPP_IMAGE_ENV_VAR: &str = "AMICU_MODAL_LLAMACPP_IMAGE";

/// Where llama-server caches `-hf` downloads inside the container
/// (`LLAMA_CACHE`); the mount path of the llama.cpp cache Volume.
pub const LLAMA_CACHE_PATH: &str = "/root/.cache/llama.cpp";

/// Persistent Modal Volumes every Modal-deployed llama.cpp server mounts, as
/// `(volume name, container mount path)` pairs. Shared across amicu
/// deployments in a workspace, like [`VOLUMES`]: a repeated or restarted
/// deploy of the same GGUF finds it already downloaded.
pub const LLAMACPP_VOLUMES: [(&str, &str); 1] = [("amicu-llamacpp-cache", LLAMA_CACHE_PATH)];

/// Default base image for a runtime family.
#[must_use]
pub const fn default_image(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Vllm => DEFAULT_VLLM_IMAGE,
        RuntimeKind::LlamaCpp => DEFAULT_LLAMACPP_IMAGE,
    }
}

/// Operator environment variable that overrides [`default_image`].
#[must_use]
pub const fn image_env_var(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Vllm => VLLM_IMAGE_ENV_VAR,
        RuntimeKind::LlamaCpp => LLAMACPP_IMAGE_ENV_VAR,
    }
}

/// Dockerfile steps that make the llama.cpp server image usable by Modal's
/// Python container runtime. This is the Dockerfile equivalent of Modal's
/// `add_python=` (which would need an extra mount RPC, see
/// [`crate::ImageSpec::base_registry_image`]):
///
/// 1. install `python3` (Ubuntu 24.04: 3.12, inside Modal's 3.10-3.13) plus
///    `python-is-python3` so `python` exists, and `ca-certificates` for the
///    runtime's TLS - only when `python` is not already on `PATH`, so a
///    custom image that has it is left alone;
/// 2. make `llama-server` resolvable by bare name: the official image keeps
///    it at `/app/llama-server`, off `PATH`, and the amicu renderer emits
///    `llama-server` as `argv[0]`.
///
/// Modal's own package is not installed (it is mounted at runtime), so no
/// `pip` is needed.
#[must_use]
pub fn llamacpp_setup_commands() -> Vec<String> {
    vec![
        "RUN command -v python >/dev/null 2>&1 || (apt-get update \
         && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
         python3 python-is-python3 ca-certificates \
         && rm -rf /var/lib/apt/lists/*)"
            .to_owned(),
        "RUN command -v llama-server >/dev/null 2>&1 \
         || ln -s /app/llama-server /usr/local/bin/llama-server"
            .to_owned(),
    ]
}

/// Dockerfile step that makes `python` resolve on `PATH` for the vLLM image.
///
/// Modal's builder records `python -VV` as the image's
/// `ImageMetadata.python_version_info`, and `FunctionCreate` fails with
/// `FailedPrecondition` ("unable to determine the version of Python
/// installed in the Image") when it is unset. `vllm/vllm-openai` installs
/// Python only as `python3` (deadsnakes + `update-alternatives` for
/// `python3`, no `python`), so alias it. A no-op when `python` already
/// exists; the build fails loudly if there is no `python3` either.
#[must_use]
pub fn vllm_setup_commands() -> Vec<String> {
    vec![
        "RUN command -v python >/dev/null 2>&1 \
         || ln -s \"$(command -v python3)\" /usr/local/bin/python"
            .to_owned(),
    ]
}

/// Environment variable [`ModalSupervisor::start`] reads a Hugging Face
/// token from when the launch plan carries no resolved `HF_TOKEN` secret.
/// Read from the *operator's local* process environment - never from the
/// deployment's own `vllm { env = {...} }` block - and forwarded only
/// through Modal's Secret mechanism (never baked into the image). Like
/// [`VLLM_IMAGE_ENV_VAR`], this is read fresh on every `start()` call, not
/// cached.
pub const HF_TOKEN_ENV_VAR: &str = "HF_TOKEN";

/// Persistent Modal Volumes every Modal-deployed vLLM server mounts, as
/// `(volume name, container mount path)` pairs. Created if missing
/// (`crate::deploy`'s `VolumeGetOrCreate` call is always
/// `CreateIfMissing`), and shared across every amicu deployment on the same
/// Modal workspace - repeated deploys of the same or a different model reuse
/// the same warmed cache rather than re-downloading weights per app.
pub const VOLUMES: [(&str, &str); 2] = [
    ("amicu-hf-cache", "/root/.cache/huggingface"),
    ("amicu-vllm-cache", "/root/.cache/vllm"),
];

/// Idle seconds before a replica scales down to zero.
pub const SCALEDOWN_WINDOW_SECS: u32 = 900;
/// Seconds Modal waits for the vLLM process to accept connections on `port`
/// before declaring the deploy failed. vLLM's own startup (loading weights,
/// building the KV cache) is the dominant cost here, not container cold
/// start.
pub const STARTUP_TIMEOUT_SECS: u32 = 600;
/// Maximum lifetime of a single container.
pub const TIMEOUT_SECS: u32 = 3600;
/// Requests a single replica handles concurrently before Modal scales out.
pub const TARGET_CONCURRENCY: u32 = 32;
/// Replicas kept warm; `0` scales to zero between requests.
pub const MIN_CONTAINERS: u32 = 0;

/// Starts, inspects, and stops a Modal-deployed vLLM server behind the same
/// [`amicu_llamacpp::ProcessSupervisor`] contract the local launchd/systemd
/// supervisor implements. See the module docs for the full argv/GPU/secret
/// mapping.
///
/// One instance targets one deployment: `app_name` is fixed at construction,
/// and [`ModalSupervisor::with_accelerator`]/[`ModalSupervisor::with_region`]
/// attach that deployment's hardware profile before the first `start`.
/// `status`/`stop` need neither - they address an already-deployed app
/// purely through the [`RuntimeHandle`] `start` returned - so
/// [`ModalSupervisor::new`] alone is enough to build one for those calls.
pub struct ModalSupervisor {
    app_name: String,
    gpu: Option<String>,
    region: Option<String>,
    scaling: ScalingSettings,
    gguf: Option<GgufSource>,
}

/// A GGUF file on Hugging Face: what a Modal-deployed llama-server fetches
/// with `-hf <repo> -hff <file>` instead of the local `-m <path>` the
/// renderer emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufSource {
    /// Hugging Face repo id (`org/model`).
    pub repo: String,
    /// File name within the repo.
    pub file: String,
}

impl ModalSupervisor {
    /// A supervisor for `app_name`, with no GPU/region constraint yet. Valid
    /// on its own for [`ProcessSupervisor::status`]/[`ProcessSupervisor::stop`];
    /// [`ProcessSupervisor::start`] additionally needs
    /// [`ModalSupervisor::with_accelerator`] (and, optionally,
    /// [`ModalSupervisor::with_region`]) called first.
    #[must_use]
    pub fn new(app_name: impl Into<String>) -> Self {
        Self {
            app_name: app_name.into(),
            gpu: None,
            region: None,
            gguf: None,
            scaling: ScalingSettings::default(),
        }
    }

    /// Applies a deployment's `scaling {}` overrides (amicu-planning#180);
    /// `None` or omitted fields keep the module defaults
    /// ([`SCALEDOWN_WINDOW_SECS`], [`TARGET_CONCURRENCY`], [`MIN_CONTAINERS`],
    /// unlimited max). Values are not re-validated here: `amicu validate`/
    /// `plan` reject invalid blocks first (`AMICU-CAP-008`).
    #[must_use]
    pub fn with_scaling(mut self, scaling: Option<&ScalingSettings>) -> Self {
        self.scaling = scaling.cloned().unwrap_or_default();
        self
    }

    /// Sets the GPU request from a hardware profile's accelerator (`None`
    /// for a CPU-only container). Validates eagerly - before any RPC - that
    /// the accelerator is a GPU: Modal has no TPU or Apple Silicon offering,
    /// so [`AcceleratorKind::Tpu`]/[`AcceleratorKind::AppleSilicon`] are
    /// rejected here rather than surfacing as an opaque `FunctionCreate`
    /// failure later.
    ///
    /// # Errors
    /// Returns [`LlamaCppError::Supervisor`] if `accelerator` is present but
    /// is not [`AcceleratorKind::Gpu`].
    pub fn with_accelerator(
        mut self,
        accelerator: Option<&Accelerator>,
    ) -> Result<Self, LlamaCppError> {
        self.gpu = accelerator.map(gpu_string).transpose()?;
        Ok(self)
    }

    /// Sets the region constraint (a hardware profile's `region.code`, if
    /// any).
    #[must_use]
    pub fn with_region(mut self, region: Option<String>) -> Self {
        self.region = region;
        self
    }

    /// Sets the Hugging Face GGUF a llama.cpp launch downloads, from the
    /// deployment's model: its Hugging Face source repo and the same
    /// recommended variant `LlamaCppAdapter::plan_launch` renders. Left unset
    /// (so a llama.cpp `start` is rejected) for a model with any other source
    /// registry or no variants. A no-op for vLLM launches.
    #[must_use]
    pub fn with_gguf_model(mut self, model: &Model) -> Self {
        self.gguf = (model.source.registry == SourceRegistry::HuggingFace)
            .then(|| {
                GgufResolver::from_variants(&model.variants)
                    .recommended()
                    .cloned()
            })
            .flatten()
            .map(|variant| GgufSource {
                repo: model.source.reference.clone(),
                file: variant.file,
            });
        self
    }

    /// Convenience: builds a supervisor for `app_name` fully configured from
    /// a resolved [`HardwareProfile`] in one call.
    ///
    /// # Errors
    /// See [`ModalSupervisor::with_accelerator`].
    pub fn for_hardware(
        app_name: impl Into<String>,
        hardware: &HardwareProfile,
    ) -> Result<Self, LlamaCppError> {
        let region = hardware.region.as_ref().map(|r| r.code.clone());
        let supervisor = Self::new(app_name).with_accelerator(hardware.accelerator.as_ref())?;
        Ok(supervisor.with_region(region))
    }
}

/// Deployment-name-safe app name: `amicu-<sanitized deployment id>`.
///
/// Lower-cases `deployment_id`, replaces every byte that is not
/// `[a-z0-9-]` with `-`, collapses repeats, and trims leading/trailing
/// dashes - conservative enough to satisfy any reasonable "deployment name"
/// constraint without needing Modal's exact validation regex (not published
/// as of this writing).
#[must_use]
pub fn app_name_for(deployment_id: &str) -> String {
    let mut out = String::with_capacity(deployment_id.len() + 6);
    out.push_str("amicu-");
    let mut last_was_dash = true; // swallow a leading separator too
    for byte in deployment_id.to_ascii_lowercase().bytes() {
        let ch = if byte.is_ascii_alphanumeric() {
            byte as char
        } else {
            '-'
        };
        if ch == '-' {
            if !last_was_dash {
                out.push('-');
            }
            last_was_dash = true;
        } else {
            out.push(ch);
            last_was_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Renders an [`Accelerator`] as Modal's `gpu=` string (`"H100"`, `"H100:2"`).
fn gpu_string(accelerator: &Accelerator) -> Result<String, LlamaCppError> {
    if accelerator.kind != AcceleratorKind::Gpu {
        return Err(LlamaCppError::Supervisor(format!(
            "Modal has no {:?} offering; only GPU accelerators can be deployed there",
            accelerator.kind
        )));
    }
    let model = accelerator.model.to_uppercase();
    Ok(if accelerator.count <= 1 {
        model
    } else {
        format!("{model}:{}", accelerator.count)
    })
}

/// Whether `model_reference` looks like a local filesystem path rather than
/// a Hugging Face repo id (`"org/model"`). Deliberately conservative - it
/// only catches the unambiguous cases (a leading `/`, `./`, `../`, or `~`) -
/// because false positives here would block a legitimate deploy, whereas a
/// false negative just reaches Modal's own "no such repo" error instead of
/// amicu's clearer one.
#[must_use]
fn looks_like_local_path(model_reference: &str) -> bool {
    model_reference.starts_with('/')
        || model_reference.starts_with("./")
        || model_reference.starts_with("../")
        || model_reference.starts_with('~')
}

/// llama-server flags that name a Hugging Face repo (`-hf <repo>[:quant]`).
const LLAMA_HF_FLAGS: [&str; 3] = ["-hf", "-hfr", "--hf-repo"];
/// llama-server flags that name a local model file.
const LLAMA_MODEL_FLAGS: [&str; 2] = ["-m", "--model"];
/// llama-server flags whose value is a local file path (besides the model
/// and draft model handled separately).
const LLAMA_LOCAL_PATH_FLAGS: [&str; 7] = [
    "--mmproj",
    "-mm",
    "--chat-template-file",
    "--lora",
    "--lora-scaled",
    "--control-vector",
    "--control-vector-scaled",
];
/// llama-server flags that carry (or point at) a secret; baked into the image
/// argv they would leak.
const LLAMA_SECRET_FLAGS: [&str; 4] = ["-hft", "--hf-token", "--api-key-file", "--api-key"];
/// llama-server flags that name a speculative-decoding draft model (a local
/// path when rendered by amicu).
const LLAMA_DRAFT_FLAGS: [&str; 3] = ["-md", "--model-draft", "--spec-draft-model"];

/// Whether `argv` is a `llama-server` launch (`argv[0]` is `llama-server`,
/// bare or by absolute path).
fn is_llama_server(argv: &[String]) -> bool {
    argv.first()
        .is_some_and(|a| a == "llama-server" || a.ends_with("/llama-server"))
}

/// The first of `flags` present in `argv[1..]`, as `--flag value` or
/// `--flag=value`.
fn first_flag<'a>(argv: &[String], flags: &[&'a str]) -> Option<&'a str> {
    argv.iter().skip(1).find_map(|arg| {
        let name = arg.split_once('=').map_or(arg.as_str(), |(n, _)| n);
        flags.iter().copied().find(|f| *f == name)
    })
}

/// The value of the first of `flags` in `argv[1..]`, accepting both
/// `--flag value` and `--flag=value`.
fn flag_value<'a>(argv: &'a [String], flags: &[&str]) -> Option<&'a str> {
    let mut rest = argv.iter().skip(1);
    while let Some(arg) = rest.next() {
        if flags.contains(&arg.as_str()) {
            return rest.next().map(String::as_str);
        }
        if let Some((name, value)) = arg.split_once('=')
            && flags.contains(&name)
        {
            return Some(value);
        }
    }
    None
}

/// The rendered model reference from a launch `argv`, if the shape matches:
/// `["vllm", "serve", "<model>", ...]` for vLLM, or a `llama-server` argv,
/// where a Hugging Face reference (`-hf <repo>[:quant]`) wins over a model
/// file (`-m <path>`, what the amicu renderer emits).
fn model_reference(argv: &[String]) -> Option<&str> {
    if is_llama_server(argv) {
        return flag_value(argv, &LLAMA_HF_FLAGS).or_else(|| flag_value(argv, &LLAMA_MODEL_FLAGS));
    }
    (argv.first().map(String::as_str) == Some("vllm")
        && argv.get(1).map(String::as_str) == Some("serve"))
    .then(|| argv.get(2).map(String::as_str))
    .flatten()
}

/// `argv` without any `-m`/`--model` model-file flag (and its value).
fn without_model_file(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut iter = argv.iter().enumerate();
    while let Some((i, arg)) = iter.next() {
        if i > 0 && LLAMA_MODEL_FLAGS.contains(&arg.as_str()) {
            iter.next();
        } else if i > 0
            && arg
                .split_once('=')
                .is_some_and(|(name, _)| LLAMA_MODEL_FLAGS.contains(&name))
        {
            // `--model=<path>` carries its value inline.
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// Refuses a launch whose model reference is a local path: Modal's
/// container has no access to the operator's filesystem, so the server there
/// must be given a Hugging Face repo id (vLLM) or `-hf` GGUF repo
/// (llama-server).
fn reject_local_path_model(argv: &[String]) -> Result<(), LlamaCppError> {
    if is_llama_server(argv) && flag_value(argv, &LLAMA_HF_FLAGS).is_none() {
        // A llama-server `-m` value is always a file path, however relative
        // (the amicu renderer emits `models/<id>/<file>`), so it never
        // reaches `looks_like_local_path`'s prefix heuristic.
        if let Some(path) = flag_value(argv, &LLAMA_MODEL_FLAGS) {
            return Err(LlamaCppError::Supervisor(format!(
                "Modal deployments need a Hugging Face GGUF reference (a model whose source \
                 registry is hugging_face, e.g. 'ggml-org/gemma-3-1b-it-GGUF'), not a local \
                 model file; got '{path}' - Modal's container cannot see this machine's \
                 filesystem"
            )));
        }
    }
    match model_reference(argv) {
        Some(reference) if looks_like_local_path(reference) => {
            Err(LlamaCppError::Supervisor(format!(
                "Modal deployments need a Hugging Face model id (e.g. \
                 'meta-llama/Llama-3.1-8B-Instruct'), not a local path; got '{reference}' - \
                 Modal's container cannot see this machine's filesystem"
            )))
        }
        _ => Ok(()),
    }
}

/// Rewrites `--host 127.0.0.1` (what `amicu_vllm::command::render_command`
/// always renders) to `--host 0.0.0.0`. See the module docs for why this
/// happens here rather than in the vLLM renderer.
fn bind_all_interfaces(argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();
    if let Some(pos) = out.iter().position(|a| a == "--host") {
        if let Some(value) = out.get_mut(pos + 1) {
            "0.0.0.0".clone_into(value);
        } else {
            out.push("0.0.0.0".to_owned());
        }
    } else {
        out.push("--host".to_owned());
        out.push("0.0.0.0".to_owned());
    }
    out
}

/// Folds a [`ModalError`] into the [`LlamaCppError`] the `ProcessSupervisor`
/// trait requires. `ModalError`'s `Display` never contains a token or secret
/// value (see `crate::credentials`/`crate::error`'s doc comments), so this is
/// always safe to surface.
fn modal_err(error: &ModalError) -> LlamaCppError {
    LlamaCppError::Supervisor(error.to_string())
}

/// Hard cap on buffered lines when no `tail` is given. A memory bound only,
/// not a termination mechanism: `stream_logs` is always called one-shot, so
/// it ends on its own. When the cap is hit the oldest lines are dropped.
const MAX_BUFFERED_LINES: usize = 10_000;

/// Collects [`crate::ops::LogLine`]s for [`ProcessSupervisor::logs`]. Pure and
/// socket-free so its behaviour is unit-tested directly.
///
/// Keeps a rolling window of the **last** `tail` lines (all lines, up to
/// [`MAX_BUFFERED_LINES`], when `tail` is `None`), matching the host
/// supervisor's `tail()` in `amicu_llamacpp::service`. It never breaks the
/// stream early: the newest lines arrive last, so stopping at N would return
/// the first N.
///
/// Image-build output is not filtered: `stream_logs` delivers it like any
/// other line (see its docs).
struct LogCollector {
    lines: std::collections::VecDeque<String>,
    limit: usize,
}

impl LogCollector {
    fn new(options: LogOptions) -> Self {
        let limit = options
            .tail
            .map_or(MAX_BUFFERED_LINES, |n| (n as usize).min(MAX_BUFFERED_LINES));
        Self {
            lines: std::collections::VecDeque::new(),
            limit,
        }
    }

    fn push(&mut self, line: &crate::ops::LogLine) -> std::ops::ControlFlow<()> {
        if self.limit == 0 {
            return std::ops::ControlFlow::Break(());
        }
        self.lines
            .push_back(line.text.trim_end_matches('\n').to_owned());
        while self.lines.len() > self.limit {
            self.lines.pop_front();
        }
        std::ops::ControlFlow::Continue(())
    }

    fn finish(self) -> Vec<String> {
        self.lines.into()
    }
}

impl ModalSupervisor {
    /// Connects an authenticated [`ModalClient`], reading
    /// `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` fresh each call - this
    /// supervisor is stateless between calls, matching
    /// `amicu_llamacpp::ServiceSupervisor`'s own statelessness.
    async fn connect(&self) -> Result<ModalClient, LlamaCppError> {
        let credentials = Credentials::from_env().map_err(|e| {
            LlamaCppError::Supervisor(format!(
                "{e}; set MODAL_TOKEN_ID and MODAL_TOKEN_SECRET before deploying to Modal"
            ))
        })?;
        ModalClient::connect(credentials)
            .await
            .map_err(|e| modal_err(&e))
    }

    /// Accelerator memory in use across the app's containers, or `None` when
    /// the probe could not run or nothing reported it.
    async fn accelerator_memory_used(&self, app_id: &str) -> Option<u64> {
        let mut client = self.connect().await.ok()?;
        let containers = client.probe_app_hardware(app_id).await.ok()?;
        containers
            .iter()
            .filter_map(|c| c.outcome.as_ref().ok())
            .filter_map(HardwareSample::accelerator_memory_used)
            .reduce(u64::saturating_add)
    }

    /// Validates a llama-server launch for Modal and rewrites its model flags
    /// (see [`ModalSupervisor::rewrite_gguf_args`]). Rejects, before any RPC,
    /// flags that point at this machine's filesystem
    /// ([`LLAMA_LOCAL_PATH_FLAGS`]) or would bake a secret into the image's
    /// argv ([`LLAMA_SECRET_FLAGS`]; only the flag name is reported, never a
    /// value). `-hf` auto-downloads the repo's multimodal projector, so
    /// `--no-mmproj` is appended unless the deployment declared `multimodal`.
    fn hf_llama_argv(
        &self,
        argv: &[String],
        has_multimodal: bool,
    ) -> Result<Vec<String>, LlamaCppError> {
        if !is_llama_server(argv) {
            return Err(LlamaCppError::Supervisor(
                "the Modal supervisor expected a `llama-server` command for a llama.cpp launch"
                    .to_owned(),
            ));
        }
        if let Some(flag) = first_flag(argv, &LLAMA_LOCAL_PATH_FLAGS) {
            return Err(LlamaCppError::Supervisor(format!(
                "'{flag}' is not supported on Modal yet: it names a local file Modal's \
                 container cannot see"
            )));
        }
        if let Some(flag) = first_flag(argv, &LLAMA_SECRET_FLAGS) {
            return Err(LlamaCppError::Supervisor(format!(
                "'{flag}' is not allowed on Modal: it would bake a secret into the container \
                 image's command line; pass the Hugging Face token as the HF_TOKEN secret \
                 instead"
            )));
        }
        let mut out = self.rewrite_gguf_args(argv)?;
        if !has_multimodal && first_flag(&out, &["--no-mmproj"]).is_none() {
            out.push("--no-mmproj".to_owned());
        }
        Ok(out)
    }

    /// The llama-server argv for a Modal container: the renderer's local
    /// `-m <path>` is replaced by `-hf <repo> -hff <file>` from
    /// [`ModalSupervisor::with_gguf_model`] (unless the launch already names a
    /// Hugging Face repo itself), and anything that points at this machine's
    /// filesystem is rejected.
    fn rewrite_gguf_args(&self, argv: &[String]) -> Result<Vec<String>, LlamaCppError> {
        if flag_value(argv, &LLAMA_DRAFT_FLAGS).is_some() {
            return Err(LlamaCppError::Supervisor(
                "speculative decoding (a draft model) is not yet supported on Modal: the draft \
                 GGUF is a local path Modal's container cannot see"
                    .to_owned(),
            ));
        }
        if flag_value(argv, &LLAMA_HF_FLAGS).is_some() {
            let out = without_model_file(argv);
            reject_local_path_model(&out)?;
            return Ok(out);
        }
        let Some(source) = &self.gguf else {
            // No Hugging Face source known: surface the local-path error.
            reject_local_path_model(argv)?;
            return Err(LlamaCppError::Supervisor(
                "Modal deployments need a Hugging Face GGUF reference (a model whose source \
                 registry is hugging_face, with at least one variant)"
                    .to_owned(),
            ));
        };
        let mut out = without_model_file(argv);
        out.splice(
            1..1,
            [
                "-hf".to_owned(),
                source.repo.clone(),
                "-hff".to_owned(),
                source.file.clone(),
            ],
        );
        reject_local_path_model(&out)?;
        Ok(out)
    }

    /// The argv the Modal container actually runs for `plan`: the renderer's
    /// local launch with the Modal rewrites applied (`-hf`/`-hff` and
    /// `--no-mmproj` for llama.cpp, `--host 0.0.0.0` for both). Single source
    /// of truth for [`ModalSupervisor::build_spec`] and `amicu plan`'s display.
    ///
    /// # Errors
    /// [`LlamaCppError::Supervisor`] if `plan` is not a command launch or names
    /// something Modal cannot reach (a local model path, a secret flag, ...).
    pub fn launch_argv(&self, plan: &LaunchPlan) -> Result<Vec<String>, LlamaCppError> {
        let argv = plan.rendered.argv().ok_or_else(|| {
            LlamaCppError::Supervisor("Modal supervisor expects a command launch".to_owned())
        })?;
        Ok(match &plan.settings {
            Runtime::Vllm(_) => {
                reject_local_path_model(argv)?;
                bind_all_interfaces(argv)
            }
            Runtime::LlamaCpp(s) => {
                bind_all_interfaces(&self.hf_llama_argv(argv, s.multimodal.is_some())?)
            }
        })
    }

    /// [`ModalSupervisor::launch_argv`] as one shell-quoted line, for display.
    ///
    /// # Errors
    /// See [`ModalSupervisor::launch_argv`].
    pub fn launch_text(&self, plan: &LaunchPlan) -> Result<String, LlamaCppError> {
        Ok(self
            .launch_argv(plan)?
            .iter()
            .map(|a| crate::image::shell_quote(a))
            .collect::<Vec<_>>()
            .join(" "))
    }

    /// Builds the [`ServerSpec`] for `plan`, forwarding `secrets` (plus the
    /// [`HF_TOKEN_ENV_VAR`] fallback) as Modal Secret env, never image env.
    ///
    /// # Errors
    /// - [`LlamaCppError::Supervisor`] if `plan` is not a vLLM launch, is not
    ///   a [`amicu_runtime::RenderedLaunch::Command`], names a local-path
    ///   model (see [`reject_local_path_model`]), or has a settings `env` key
    ///   that collides with a resolved secret name (ambiguous - which one
    ///   should win is not guessable, same rule
    ///   `amicu_llamacpp::ServiceSupervisor::start` applies to the local
    ///   path).
    ///
    /// `base_registry_image` and `hf_token_fallback` are read from the
    /// process environment by [`ProcessSupervisor::start`] ([`VLLM_IMAGE_ENV_VAR`]/
    /// [`HF_TOKEN_ENV_VAR`]) and passed in explicitly here rather than read
    /// directly - this keeps the mapping itself a pure function of its
    /// arguments, exercised by the unit tests below with no process-global
    /// state to set up or restore.
    fn build_spec(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
        base_registry_image: &str,
        hf_token_fallback: Option<&str>,
    ) -> Result<ServerSpec, LlamaCppError> {
        let (settings_env, port, runtime) = match &plan.settings {
            Runtime::Vllm(s) => (&s.env, s.port, RuntimeKind::Vllm),
            Runtime::LlamaCpp(s) => (&s.env, s.port, RuntimeKind::LlamaCpp),
        };
        let argv = self.launch_argv(plan)?;

        if let Some(s) = secrets.iter().find(|s| settings_env.contains_key(&s.name)) {
            return Err(LlamaCppError::Supervisor(format!(
                "env var '{}' is set both by the deployment's `env` block and a resolved secret",
                s.name
            )));
        }

        let mut image_env: BTreeMap<String, String> = settings_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if runtime == RuntimeKind::LlamaCpp {
            // `-hf` downloads land here, on the Volume mounted at the same
            // path. A value the deployment's own `env` block set wins.
            image_env
                .entry("LLAMA_CACHE".to_owned())
                .or_insert_with(|| LLAMA_CACHE_PATH.to_owned());
        }

        let mut secret_env = BTreeMap::new();
        for secret in secrets {
            let value = secret.value.expose_secret_str().map_err(|_| {
                LlamaCppError::Supervisor(format!("secret '{}' is not valid utf-8", secret.name))
            })?;
            secret_env.insert(secret.name.clone(), value.to_owned());
        }
        // Documented fallback (see the module docs and `HF_TOKEN_ENV_VAR`):
        // read from the operator's own environment, never the deployment's.
        if !secret_env.contains_key(HF_TOKEN_ENV_VAR)
            && !image_env.contains_key(HF_TOKEN_ENV_VAR)
            && let Some(token) = hf_token_fallback
        {
            secret_env.insert(HF_TOKEN_ENV_VAR.to_owned(), token.to_owned());
        }

        Ok(ServerSpec {
            app_name: self.app_name.clone(),
            gpu: self.gpu.clone(),
            region: self.region.clone(),
            proxy_regions: vec![modal_proxy_region(self.region.as_deref()).to_owned()],
            port,
            ready_path: (runtime == RuntimeKind::LlamaCpp).then(|| "/health".to_owned()),
            argv,
            image: ImageSpec {
                base_registry_image: base_registry_image.to_owned(),
                pip_packages: Vec::new(),
                setup_commands: match runtime {
                    RuntimeKind::Vllm => vllm_setup_commands(),
                    RuntimeKind::LlamaCpp => llamacpp_setup_commands(),
                },
                env: image_env,
            },
            volumes: match runtime {
                RuntimeKind::Vllm => VOLUMES.as_slice(),
                RuntimeKind::LlamaCpp => LLAMACPP_VOLUMES.as_slice(),
            }
            .iter()
            .map(|(name, path)| ((*name).to_owned(), (*path).to_owned()))
            .collect(),
            secret_env,
            scaledown_window_secs: self
                .scaling
                .scaledown_window_secs
                .unwrap_or(SCALEDOWN_WINDOW_SECS),
            startup_timeout_secs: STARTUP_TIMEOUT_SECS,
            timeout_secs: TIMEOUT_SECS,
            target_concurrency: self
                .scaling
                .target_concurrency
                .unwrap_or(TARGET_CONCURRENCY),
            min_containers: self.scaling.min_replicas.unwrap_or(MIN_CONTAINERS),
            max_containers: self.scaling.max_replicas,
        })
    }
}

/// `GET <endpoint>/health` with a short timeout, mirroring
/// `amicu_llamacpp::health::HttpHealthProbe` but over TLS (Modal's endpoint
/// is always HTTPS) - see the module docs for why that probe cannot be reused
/// as-is. A `2xx` is healthy; anything else (including a network error or
/// timeout) is not. Returns the status code when a response arrived (so a
/// proxy `401` can be told apart from "not up yet"); proxy auth headers are
/// attached when `MODAL_PROXY_TOKEN_*` are set.
async fn health_status(endpoint: &str) -> Option<u16> {
    let url = format!("{}/health", endpoint.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = crate::proxy_auth::apply(client.get(url))
        .send()
        .await
        .ok()?;
    Some(resp.status().as_u16())
}

/// One-line phase note printed to stderr before the (possibly slow) deploy.
const BUILD_PHASE_NOTE: &str =
    "building image and deploying to Modal (first build of an image can take several minutes)...";

/// Prefix put on every streamed image-build line.
const BUILD_LOG_PREFIX: &str = "  build: ";

/// Env keys whose (plain, image) values are treated as secrets for redaction.
const SECRET_KEY_HINTS: [&str; 7] = [
    "TOKEN",
    "KEY",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
];

/// Values that must never reach the terminal: every resolved-secret value, plus
/// plain image-env values whose key looks like a credential (a user may have
/// put a token in the `env` block and a builder can echo `ENV k=v`). Other
/// image-env values (`true`, cache paths, ...) are left alone so ordinary build
/// output is not mangled. Non-secret-looking values shorter than 4 bytes are
/// never redacted.
fn redaction_values(spec: &ServerSpec) -> Vec<String> {
    let secrets = spec.secret_env.values().filter(|v| !v.is_empty());
    let env = spec
        .image
        .env
        .iter()
        .filter(|(k, v)| {
            let k = k.to_ascii_uppercase();
            v.len() >= 4 && SECRET_KEY_HINTS.iter().any(|h| k.contains(h))
        })
        .map(|(_, v)| v);
    secrets.chain(env).cloned().collect()
}

/// Upper bound on a buffered partial line before it is flushed anyway.
const MAX_PARTIAL_LINE: usize = 64 * 1024;

/// Writes image-build output to a (non-stdout) writer: one prefixed line per
/// build line, control characters stripped, secret values replaced by `***`.
/// Chunks are reassembled into whole lines before redaction so a value split
/// across chunks cannot leak. Write errors are ignored: a broken stderr must
/// not fail a deploy.
struct BuildLogSink<W: std::io::Write> {
    out: W,
    redact: Vec<String>,
    partial: String,
}

impl<W: std::io::Write> BuildLogSink<W> {
    fn new(out: W, redact: Vec<String>) -> Self {
        Self {
            out,
            redact,
            partial: String::new(),
        }
    }

    fn phase(&mut self, note: &str) {
        let _ = writeln!(self.out, "{note}");
    }

    /// Feed one chunk of build output (may hold several lines or a fragment).
    fn line(&mut self, chunk: &str) {
        self.partial.push_str(chunk);
        // `\n` and a bare `\r` (progress update) both terminate a line.
        while let Some(end) = self.partial.find(['\n', '\r']) {
            let raw: String = self.partial.drain(..=end).collect();
            self.emit(&raw);
        }
        if self.partial.len() > MAX_PARTIAL_LINE {
            let raw = std::mem::take(&mut self.partial);
            self.emit(&raw);
        }
    }

    /// Flush any trailing unterminated line.
    fn finish(&mut self) {
        let raw = std::mem::take(&mut self.partial);
        self.emit(&raw);
    }

    fn emit(&mut self, raw: &str) {
        let mut text: String = raw
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect();
        if text.trim().is_empty() {
            return;
        }
        for value in &self.redact {
            text = text.replace(value.as_str(), "***");
        }
        let _ = writeln!(self.out, "{BUILD_LOG_PREFIX}{text}");
    }
}

#[async_trait]
impl ProcessSupervisor for ModalSupervisor {
    async fn start(
        &self,
        plan: &LaunchPlan,
        secrets: &[ResolvedSecret],
    ) -> Result<RuntimeHandle, LlamaCppError> {
        let runtime = RuntimeKind::of(&plan.settings);
        let base_registry_image = std::env::var(image_env_var(runtime))
            .unwrap_or_else(|_| default_image(runtime).to_owned());
        let hf_token_fallback = std::env::var(HF_TOKEN_ENV_VAR).ok();
        let spec = self.build_spec(
            plan,
            secrets,
            &base_registry_image,
            hf_token_fallback.as_deref(),
        )?;
        let mut client = self.connect().await?;
        // Stream the image build to stderr (stdout stays clean so
        // `--output json` is one JSON value). Lines are scrubbed of every
        // image-env and secret value first, see `BuildLogSink`.
        let mut sink = BuildLogSink::new(std::io::stderr(), redaction_values(&spec));
        sink.phase(BUILD_PHASE_NOTE);
        let mut on_log = |line: &str| sink.line(line);
        let deployed = client.deploy_server(&spec, &mut on_log).await;
        sink.finish();
        let deployment = deployed.map_err(|e| modal_err(&e))?;
        Ok(RuntimeHandle {
            id: deployment.app_id,
            deployment: plan.deployment.clone(),
            endpoint: deployment.url,
        })
    }

    async fn status(&self, handle: &RuntimeHandle) -> Result<RuntimeStatus, LlamaCppError> {
        let code = health_status(&handle.endpoint).await;
        Ok(if matches!(code, Some(c) if (200..300).contains(&c)) {
            RuntimeStatus::Running {
                detail: "healthy".to_owned(),
            }
        } else if let Some(hint) =
            code.and_then(|c| crate::proxy_auth::hint_for_status(c, crate::proxy_auth::env_state()))
        {
            RuntimeStatus::Failed {
                detail: format!("auth required: {hint}"),
            }
        } else {
            RuntimeStatus::Failed {
                detail: "endpoint not ready".to_owned(),
            }
        })
    }

    async fn metrics(&self, handle: &RuntimeHandle) -> Result<RuntimeMetrics, LlamaCppError> {
        // Scrapes the runtime's Prometheus `/metrics` over the Modal URL (see
        // `crate::metrics` for the series and their sources). One second apart
        // for vLLM, which has no throughput gauge.
        let mut metrics =
            crate::metrics::scrape_with_rate(&handle.endpoint, Duration::from_secs(1))
                .await
                .map(|m| m.to_runtime_metrics())
                .map_err(|e| LlamaCppError::Supervisor(e.to_string()))?;
        // `memory_used` is the accelerator memory summed over the app's
        // containers, from the hardware probe (`crate::hardware`). Best
        // effort: an exec that fails or an accelerator that does not report
        // memory leaves the zero `to_runtime_metrics` set, never an error.
        if let Some(bytes) = self.accelerator_memory_used(&handle.id).await {
            metrics.memory_used = ByteSize::from_bytes(bytes);
        }
        Ok(metrics)
    }

    async fn logs(
        &self,
        handle: &RuntimeHandle,
        options: LogOptions,
    ) -> Result<LogStream, LlamaCppError> {
        // `follow` is intentionally treated as one-shot: `LogStream` is a
        // finished batch (like the host supervisor, which ignores `follow`
        // and returns the last N lines), so a live tail cannot be expressed
        // here and would never return. Use `amicu modal logs <app> --follow`
        // to stream.
        if options.tail == Some(0) {
            return Ok(LogStream(Vec::new()));
        }
        let mut client = self.connect().await?;
        let mut collector = LogCollector::new(options);
        // `handle.id` is the Modal app id (`start` stores it there).
        client
            .stream_logs(&handle.id, None, false, |line| collector.push(&line))
            .await
            .map_err(|e| modal_err(&e))?;
        Ok(LogStream(collector.finish()))
    }

    async fn stop(&self, handle: &RuntimeHandle) -> Result<(), LlamaCppError> {
        let mut client = self.connect().await?;
        client.stop_app(&handle.id).await.map_err(|e| modal_err(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_log_sink_prefixes_splits_and_redacts() {
        let mut buf = Vec::new();
        {
            let mut sink = BuildLogSink::new(&mut buf, vec!["hf_supersecret".to_owned()]);
            sink.phase("starting");
            sink.line("Step 1\nusing hf_supersecret here\n\n");
            sink.line("\x1b[31mred\x1b[0m\rprogress");
            sink.line("\n   \n");
            // Secret split across chunks, plus an unterminated tail.
            sink.line("tok hf_super");
            sink.line("secret end\ntail hf_supersecret");
            sink.finish();
        }
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(
            out,
            "starting\n  build: Step 1\n  build: using *** here\n  build: [31mred[0m\n  build: progress\n  build: tok *** end\n  build: tail ***\n"
        );
        assert!(!out.contains("hf_supersecret"));
    }

    #[test]
    fn redaction_values_cover_secrets_and_image_env() {
        let mut settings = base_settings();
        settings.env = BTreeMap::from([
            ("MY_API_TOKEN".to_owned(), "plainvalue".to_owned()),
            ("FLAG".to_owned(), "true".to_owned()),
            ("CACHE".to_owned(), "/root/.cache/huggingface".to_owned()),
        ]);
        let plan = plan_with(base_argv(), settings);
        let secret = ResolvedSecret {
            name: "HF_TOKEN".to_owned(),
            value: SecretValue::new("tok".to_owned()),
        };
        let spec = ModalSupervisor::new("amicu-dep")
            .build_spec(&plan, &[secret], DEFAULT_VLLM_IMAGE, None)
            .unwrap();
        let values = redaction_values(&spec);
        assert!(values.contains(&"plainvalue".to_owned()));
        assert!(values.contains(&"tok".to_owned()));
        assert!(!values.contains(&"true".to_owned()));
        assert!(!values.contains(&"/root/.cache/huggingface".to_owned()));
    }
    use amicu_core::ByteSize;
    use amicu_core::id::DeploymentId;
    use amicu_core::resource::VllmSettings;
    use amicu_core::secret::SecretValue;
    use amicu_runtime::{RenderedLaunch, ResourceEstimate};
    use std::collections::BTreeMap;

    fn base_argv() -> Vec<String> {
        vec![
            "vllm".to_owned(),
            "serve".to_owned(),
            "meta-llama/Llama-3.1-8B-Instruct".to_owned(),
            "--port".to_owned(),
            "8000".to_owned(),
            "--host".to_owned(),
            "127.0.0.1".to_owned(),
            "--max-model-len".to_owned(),
            "8192".to_owned(),
        ]
    }

    fn plan_with(argv: Vec<String>, settings: VllmSettings) -> LaunchPlan {
        let text = argv.join(" ");
        LaunchPlan {
            deployment: DeploymentId::from("dep"),
            runtime: amicu_runtime::RuntimeKind::Vllm,
            rendered: RenderedLaunch::Command { argv, text },
            settings: Runtime::Vllm(settings),
            secret_refs: Vec::new(),
            estimate: ResourceEstimate {
                estimated_memory: ByteSize::from_bytes(0),
                gpu_count: 1,
                kv_cache: None,
            },
        }
    }

    fn base_settings() -> VllmSettings {
        VllmSettings {
            context_length: 8192,
            port: 8000,
            ..Default::default()
        }
    }

    // --- app_name_for -------------------------------------------------

    #[test]
    fn app_name_sanitizes_and_prefixes() {
        assert_eq!(app_name_for("support-chat"), "amicu-support-chat");
        assert_eq!(app_name_for("Support_Chat.01"), "amicu-support-chat-01");
        assert_eq!(app_name_for("  weird//id--"), "amicu-weird-id");
    }

    // --- gpu_string -----------------------------------------------------

    fn accelerator(kind: AcceleratorKind, model: &str, count: u32) -> Accelerator {
        Accelerator {
            kind,
            model: model.to_owned(),
            count,
            memory: ByteSize::from_bytes(0),
            memory_bandwidth_gbps: None,
        }
    }

    #[test]
    fn gpu_string_upper_cases_and_omits_count_of_one() {
        let acc = accelerator(AcceleratorKind::Gpu, "h100", 1);
        assert_eq!(gpu_string(&acc).unwrap(), "H100");
    }

    #[test]
    fn gpu_string_includes_count_above_one() {
        let acc = accelerator(AcceleratorKind::Gpu, "h100", 2);
        assert_eq!(gpu_string(&acc).unwrap(), "H100:2");
    }

    #[test]
    fn gpu_string_rejects_non_gpu_accelerators() {
        for kind in [AcceleratorKind::Tpu, AcceleratorKind::AppleSilicon] {
            let acc = accelerator(kind, "whatever", 1);
            assert!(gpu_string(&acc).is_err());
        }
    }

    // --- with_accelerator / for_hardware --------------------------------

    #[test]
    fn with_accelerator_none_leaves_gpu_unset() {
        let sup = ModalSupervisor::new("amicu-x")
            .with_accelerator(None)
            .unwrap();
        assert!(sup.gpu.is_none());
    }

    #[test]
    fn for_hardware_rejects_tpu() {
        let hardware = HardwareProfile {
            id: "hw".into(),
            provider: "modal".into(),
            region: None,
            accelerator: Some(accelerator(AcceleratorKind::Tpu, "v5e", 1)),
            cpu: None,
            memory: ByteSize::from_bytes(0),
            interconnect: None,
            price: None,
        };
        assert!(ModalSupervisor::for_hardware("amicu-x", &hardware).is_err());
    }

    // --- LogCollector -----------------------------------------------------

    fn log_line(text: &str) -> crate::ops::LogLine {
        crate::ops::LogLine {
            entry_id: "en-1".to_owned(),
            fd: crate::ops::LogFd::Stdout,
            text: text.to_owned(),
            timestamp: 0.0,
        }
    }

    fn feed(options: LogOptions, n: usize) -> (Vec<String>, bool) {
        let mut collector = LogCollector::new(options);
        let mut broke = false;
        for i in 0..n {
            if collector.push(&log_line(&format!("l{i}\n"))).is_break() {
                broke = true;
                break;
            }
        }
        (collector.finish(), broke)
    }

    #[test]
    fn without_tail_keeps_everything_and_never_breaks() {
        let (lines, broke) = feed(LogOptions::default(), 3);
        assert_eq!(lines, ["l0", "l1", "l2"]);
        assert!(!broke);
    }

    #[test]
    fn tail_keeps_the_last_n_lines_for_both_follow_modes() {
        for follow in [false, true] {
            let options = LogOptions {
                follow,
                tail: Some(2),
            };
            let (lines, broke) = feed(options, 5);
            assert_eq!(lines, ["l3", "l4"], "follow={follow}");
            assert!(!broke, "must consume the whole finite stream");
            assert_eq!(feed(options, 1).0, ["l0"]);
        }
    }

    #[test]
    fn untailed_buffer_is_capped_to_the_newest_lines() {
        let (lines, _) = feed(LogOptions::default(), MAX_BUFFERED_LINES + 3);
        assert_eq!(lines.len(), MAX_BUFFERED_LINES);
        assert_eq!(lines[0], "l3");
    }

    #[test]
    fn tail_zero_returns_nothing() {
        let options = LogOptions {
            follow: false,
            tail: Some(0),
        };
        assert_eq!(feed(options, 3).0.len(), 0);
    }

    #[tokio::test]
    async fn logs_with_tail_zero_returns_empty_without_any_rpc() {
        // No credentials in play: it must return before `connect`.
        let handle = RuntimeHandle {
            id: "ap-1".to_owned(),
            deployment: amicu_core::id::DeploymentId::from("dep"),
            endpoint: "https://x".to_owned(),
        };
        let out = ModalSupervisor::new("amicu-x")
            .logs(
                &handle,
                LogOptions {
                    follow: true,
                    tail: Some(0),
                },
            )
            .await
            .unwrap();
        assert_eq!(out.0.len(), 0);
    }

    #[test]
    fn modal_errors_map_through_modal_err() {
        let error = ModalError::InvalidSpec {
            field: "proxy_regions",
            detail: "bad".to_owned(),
        };
        let LlamaCppError::Supervisor(message) = modal_err(&error) else {
            panic!("expected Supervisor");
        };
        assert_eq!(message, error.to_string());
    }

    // --- bind_all_interfaces --------------------------------------------

    #[test]
    fn bind_all_interfaces_rewrites_loopback_host() {
        let argv = bind_all_interfaces(&base_argv());
        let idx = argv.iter().position(|a| a == "--host").unwrap();
        assert_eq!(argv[idx + 1], "0.0.0.0");
    }

    #[test]
    fn bind_all_interfaces_appends_when_host_flag_absent() {
        let argv: Vec<String> = vec!["vllm".to_owned(), "serve".to_owned(), "m".to_owned()];
        let out = bind_all_interfaces(&argv);
        assert_eq!(out.last(), Some(&"0.0.0.0".to_owned()));
        assert_eq!(out[out.len() - 2], "--host");
    }

    // --- reject_local_path_model -----------------------------------------

    #[test]
    fn local_paths_are_rejected() {
        for bad in ["/models/llama", "./llama", "../llama", "~/llama"] {
            let mut argv = base_argv();
            argv[2] = bad.to_owned();
            assert!(reject_local_path_model(&argv).is_err(), "{bad}");
        }
    }

    #[test]
    fn hf_repo_ids_are_accepted() {
        assert!(reject_local_path_model(&base_argv()).is_ok());
    }

    // --- build_spec -------------------------------------------------------

    /// `build_spec` never reads the process environment itself (see its doc
    /// comment) - these two stand in for what `start` would read from
    /// [`VLLM_IMAGE_ENV_VAR`]/[`HF_TOKEN_ENV_VAR`], so tests never need
    /// `unsafe` env mutation (forbidden workspace-wide) to exercise either
    /// fallback.
    fn no_image_override() -> &'static str {
        DEFAULT_VLLM_IMAGE
    }
    fn no_hf_token_fallback() -> Option<&'static str> {
        None
    }

    #[test]
    fn build_spec_without_scaling_keeps_the_hardcoded_defaults() {
        let plan = plan_with(base_argv(), base_settings());
        let spec = ModalSupervisor::new("amicu-dep")
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap();
        assert_eq!(spec.min_containers, MIN_CONTAINERS);
        assert_eq!(spec.max_containers, None);
        assert_eq!(spec.target_concurrency, TARGET_CONCURRENCY);
        assert_eq!(spec.scaledown_window_secs, SCALEDOWN_WINDOW_SECS);
    }

    #[test]
    fn build_spec_applies_scaling_overrides_field_by_field() {
        let plan = plan_with(base_argv(), base_settings());
        let full = ScalingSettings {
            min_replicas: Some(1),
            max_replicas: Some(4),
            target_concurrency: Some(8),
            scaledown_window_secs: Some(120),
        };
        let spec = ModalSupervisor::new("amicu-dep")
            .with_scaling(Some(&full))
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap();
        assert_eq!(
            (
                spec.min_containers,
                spec.max_containers,
                spec.target_concurrency,
                spec.scaledown_window_secs
            ),
            (1, Some(4), 8, 120)
        );

        // Only max set: the other three keep their defaults.
        let only_max = ScalingSettings {
            max_replicas: Some(1),
            ..ScalingSettings::default()
        };
        let spec = ModalSupervisor::new("amicu-dep")
            .with_scaling(Some(&only_max))
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap();
        assert_eq!(spec.max_containers, Some(1));
        assert_eq!(spec.min_containers, MIN_CONTAINERS);
        assert_eq!(spec.target_concurrency, TARGET_CONCURRENCY);
        assert_eq!(spec.scaledown_window_secs, SCALEDOWN_WINDOW_SECS);
    }

    #[test]
    fn build_spec_binds_all_interfaces_and_uses_default_image() {
        let sup = ModalSupervisor::new("amicu-dep");
        let plan = plan_with(base_argv(), base_settings());
        let spec = sup
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap();
        let idx = spec.argv.iter().position(|a| a == "--host").unwrap();
        assert_eq!(spec.argv[idx + 1], "0.0.0.0");
        assert_eq!(spec.image.base_registry_image, DEFAULT_VLLM_IMAGE);
        assert_eq!(spec.volumes.len(), VOLUMES.len());
    }

    #[test]
    fn build_spec_rejects_a_local_path_model_before_any_rpc() {
        let sup = ModalSupervisor::new("amicu-dep");
        let mut argv = base_argv();
        argv[2] = "/models/llama".to_owned();
        let plan = plan_with(argv, base_settings());
        let err = sup
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap_err();
        assert!(err.to_string().contains("Hugging Face"));
    }

    #[test]
    fn build_spec_rejects_a_non_vllm_plan() {
        let sup = ModalSupervisor::new("amicu-dep");
        let mut plan = plan_with(base_argv(), base_settings());
        plan.settings = Runtime::LlamaCpp(amicu_core::resource::LlamaCppSettings::default());
        assert!(
            sup.build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
                .is_err()
        );
    }

    #[test]
    fn build_spec_forwards_resolved_secrets_as_secret_env_never_image_env() {
        let sup = ModalSupervisor::new("amicu-dep");
        let plan = plan_with(base_argv(), base_settings());
        let secrets = [ResolvedSecret {
            name: "HF_TOKEN".to_owned(),
            value: SecretValue::new("hf_super_secret".to_owned()),
        }];
        let spec = sup
            .build_spec(&plan, &secrets, no_image_override(), no_hf_token_fallback())
            .unwrap();
        assert_eq!(
            spec.secret_env.get("HF_TOKEN").map(String::as_str),
            Some("hf_super_secret")
        );
        assert!(!spec.image.env.contains_key("HF_TOKEN"));
    }

    #[test]
    fn build_spec_plain_env_settings_become_image_env_not_secret_env() {
        let sup = ModalSupervisor::new("amicu-dep");
        let mut settings = base_settings();
        settings.env = BTreeMap::from([("VLLM_LOGGING_LEVEL".to_owned(), "DEBUG".to_owned())]);
        let plan = plan_with(base_argv(), settings);
        let spec = sup
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap();

        assert_eq!(
            spec.image.env.get("VLLM_LOGGING_LEVEL").map(String::as_str),
            Some("DEBUG")
        );
        assert!(spec.secret_env.is_empty());
    }

    #[test]
    fn build_spec_rejects_env_key_colliding_with_a_resolved_secret() {
        let sup = ModalSupervisor::new("amicu-dep");
        let mut settings = base_settings();
        settings.env = BTreeMap::from([("HF_TOKEN".to_owned(), "not-a-secret".to_owned())]);
        let plan = plan_with(base_argv(), settings);
        let secrets = [ResolvedSecret {
            name: "HF_TOKEN".to_owned(),
            value: SecretValue::new("real-secret".to_owned()),
        }];
        assert!(
            sup.build_spec(&plan, &secrets, no_image_override(), no_hf_token_fallback())
                .is_err()
        );
    }

    #[test]
    fn build_spec_honours_the_image_override_argument() {
        let sup = ModalSupervisor::new("amicu-dep");
        let plan = plan_with(base_argv(), base_settings());
        let spec = sup
            .build_spec(
                &plan,
                &[],
                "vllm/vllm-openai:v0.30.0",
                no_hf_token_fallback(),
            )
            .unwrap();
        assert_eq!(spec.image.base_registry_image, "vllm/vllm-openai:v0.30.0");
    }

    #[test]
    fn build_spec_uses_the_hf_token_fallback_only_when_not_already_set() {
        let sup = ModalSupervisor::new("amicu-dep");
        let plan = plan_with(base_argv(), base_settings());
        let spec = sup
            .build_spec(&plan, &[], no_image_override(), Some("hf_fallback_token"))
            .unwrap();
        assert_eq!(
            spec.secret_env.get("HF_TOKEN").map(String::as_str),
            Some("hf_fallback_token")
        );

        // A resolved secret named HF_TOKEN wins over the fallback.
        let secrets = [ResolvedSecret {
            name: "HF_TOKEN".to_owned(),
            value: SecretValue::new("real-secret".to_owned()),
        }];
        let spec = sup
            .build_spec(
                &plan,
                &secrets,
                no_image_override(),
                Some("hf_fallback_token"),
            )
            .unwrap();
        assert_eq!(
            spec.secret_env.get("HF_TOKEN").map(String::as_str),
            Some("real-secret")
        );
    }

    // --- llama.cpp ---------------------------------------------------------

    fn llama_argv() -> Vec<String> {
        [
            "llama-server",
            "-m",
            "models/gemma/gemma-q4_k_m.gguf",
            "--port",
            "8080",
            "--ctx-size",
            "4096",
            "--n-gpu-layers",
            "-1",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    fn llama_plan(argv: Vec<String>, env: BTreeMap<String, String>) -> LaunchPlan {
        let mut plan = plan_with(argv, base_settings());
        plan.runtime = RuntimeKind::LlamaCpp;
        plan.settings = Runtime::LlamaCpp(amicu_core::resource::LlamaCppSettings {
            context_length: 4096,
            port: 8080,
            env,
            ..Default::default()
        });
        plan
    }

    fn gguf_model(registry: SourceRegistry) -> Model {
        Model {
            id: "gemma".into(),
            source: amicu_core::resource::ModelSource {
                registry,
                reference: "ggml-org/gemma-3-1b-it-GGUF".to_owned(),
            },
            architecture: "gemma".to_owned(),
            modalities: Vec::new(),
            artifact: amicu_core::resource::Artifact {
                format: amicu_core::resource::ArtifactFormat::Gguf,
                quantization: None,
            },
            context_length: None,
            variants: vec![amicu_core::resource::GgufVariant {
                quantization: amicu_core::resource::Quantization::Q4KM,
                file: "gemma-3-1b-it-Q4_K_M.gguf".to_owned(),
                size: None,
                sha256: None,
            }],
            n_layers: None,
            n_kv_heads: None,
            head_dim: None,
        }
    }

    fn llama_supervisor() -> ModalSupervisor {
        ModalSupervisor::new("amicu-dep").with_gguf_model(&gguf_model(SourceRegistry::HuggingFace))
    }

    #[test]
    fn model_reference_reads_a_llama_server_hf_repo_or_model_file() {
        let mut argv = llama_argv();
        assert_eq!(
            model_reference(&argv),
            Some("models/gemma/gemma-q4_k_m.gguf")
        );
        // `-hf` wins over `-m`, in any spelling.
        for flag in ["-hf", "-hfr", "--hf-repo"] {
            let mut with_hf = argv.clone();
            with_hf.extend([flag.to_owned(), "org/model:Q4_K_M".to_owned()]);
            assert_eq!(
                model_reference(&with_hf),
                Some("org/model:Q4_K_M"),
                "{flag}"
            );
        }
        argv.push("--hf-repo=org/inline".to_owned());
        assert_eq!(model_reference(&argv), Some("org/inline"));
        assert_eq!(model_reference(&["llama-server".to_owned()]), None);
        // An absolute binary path is still llama-server; another binary is not.
        assert!(is_llama_server(&["/app/llama-server".to_owned()]));
        assert_eq!(
            model_reference(&["other".to_owned(), "-m".to_owned(), "x".to_owned()]),
            None
        );
    }

    #[test]
    fn a_relative_llama_server_model_file_is_rejected_as_local() {
        let err = reject_local_path_model(&llama_argv())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Hugging Face GGUF"), "{err}");
        assert!(err.contains("models/gemma/gemma-q4_k_m.gguf"), "{err}");
        let mut hf = llama_argv();
        hf.extend(["-hf".to_owned(), "org/model".to_owned()]);
        assert!(reject_local_path_model(&hf).is_ok());
    }

    #[test]
    fn image_selection_is_per_runtime() {
        assert_eq!(default_image(RuntimeKind::Vllm), DEFAULT_VLLM_IMAGE);
        assert_eq!(default_image(RuntimeKind::LlamaCpp), DEFAULT_LLAMACPP_IMAGE);
        assert!(DEFAULT_LLAMACPP_IMAGE.starts_with("ghcr.io/ggml-org/llama.cpp:server-cuda-b"));
        assert_eq!(image_env_var(RuntimeKind::Vllm), "AMICU_MODAL_VLLM_IMAGE");
        assert_eq!(
            image_env_var(RuntimeKind::LlamaCpp),
            "AMICU_MODAL_LLAMACPP_IMAGE"
        );
    }

    #[test]
    fn build_spec_maps_a_llama_launch_onto_the_llamacpp_image_and_volume() {
        let plan = llama_plan(llama_argv(), BTreeMap::new());
        let spec = llama_supervisor()
            .build_spec(
                &plan,
                &[],
                default_image(RuntimeKind::LlamaCpp),
                no_hf_token_fallback(),
            )
            .unwrap();

        assert_eq!(spec.image.base_registry_image, DEFAULT_LLAMACPP_IMAGE);
        assert_eq!(spec.port, 8080);
        assert_eq!(
            spec.argv[..5],
            [
                "llama-server",
                "-hf",
                "ggml-org/gemma-3-1b-it-GGUF",
                "-hff",
                "gemma-3-1b-it-Q4_K_M.gguf"
            ]
        );
        // The local path is gone, the rest of the launch is untouched, and
        // the server binds all interfaces.
        assert!(!spec.argv.iter().any(|a| a == "-m" || a.contains("models/")));
        assert!(spec.argv.windows(2).any(|w| w == ["--ctx-size", "4096"]));
        let host = spec.argv.iter().position(|a| a == "--host").unwrap();
        assert_eq!(spec.argv[host + 1], "0.0.0.0");
        // Cache on a Volume, python + PATH setup in the image.
        assert_eq!(
            spec.image.env.get("LLAMA_CACHE").map(String::as_str),
            Some(LLAMA_CACHE_PATH)
        );
        assert_eq!(
            spec.volumes,
            vec![(
                "amicu-llamacpp-cache".to_owned(),
                LLAMA_CACHE_PATH.to_owned()
            )]
        );
        assert_eq!(spec.image.setup_commands, llamacpp_setup_commands());
    }

    #[test]
    fn vllm_launches_get_the_python_alias_and_the_vllm_volumes() {
        let plan = plan_with(base_argv(), base_settings());
        let spec = ModalSupervisor::new("amicu-dep")
            .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
            .unwrap();
        assert_eq!(spec.image.setup_commands, vllm_setup_commands());
        let setup = spec.image.setup_commands.join("\n");
        assert!(setup.contains("command -v python3"), "{setup}");
        assert!(!spec.image.env.contains_key("LLAMA_CACHE"));
        assert_eq!(spec.volumes[0].1, "/root/.cache/huggingface");
    }

    #[test]
    fn llamacpp_setup_installs_python_and_exposes_llama_server() {
        let commands = llamacpp_setup_commands().join("\n");
        assert!(commands.contains("python3 python-is-python3"), "{commands}");
        assert!(commands.contains("command -v python"), "{commands}");
        assert!(commands.contains("ln -s /app/llama-server /usr/local/bin/llama-server"));
        assert!(!commands.contains("HF_TOKEN"));
    }

    #[test]
    fn a_llama_launch_without_a_hugging_face_source_is_rejected() {
        let plan = llama_plan(llama_argv(), BTreeMap::new());
        for sup in [
            ModalSupervisor::new("amicu-dep"),
            ModalSupervisor::new("amicu-dep").with_gguf_model(&gguf_model(SourceRegistry::Local)),
        ] {
            let err = sup
                .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
                .unwrap_err()
                .to_string();
            assert!(err.contains("Hugging Face GGUF"), "{err}");
        }
    }

    #[test]
    fn a_llama_launch_that_already_names_an_hf_repo_keeps_it() {
        let mut argv = llama_argv();
        argv.extend(["-hf".to_owned(), "org/own:Q8_0".to_owned()]);
        let spec = ModalSupervisor::new("amicu-dep")
            .build_spec(
                &llama_plan(argv, BTreeMap::new()),
                &[],
                no_image_override(),
                no_hf_token_fallback(),
            )
            .unwrap();
        assert_eq!(spec.argv.iter().filter(|a| *a == "-hf").count(), 1);
        assert!(spec.argv.iter().any(|a| a == "org/own:Q8_0"));
        assert!(!spec.argv.iter().any(|a| a == "-m"));
    }

    #[test]
    fn a_llama_draft_model_is_rejected() {
        let mut argv = llama_argv();
        argv.extend(["--model-draft".to_owned(), "models/draft.gguf".to_owned()]);
        let err = llama_supervisor()
            .build_spec(
                &llama_plan(argv, BTreeMap::new()),
                &[],
                no_image_override(),
                no_hf_token_fallback(),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("draft"), "{err}");
    }

    #[test]
    fn llama_hf_token_goes_to_secret_env_never_image_env_or_errors() {
        let plan = llama_plan(llama_argv(), BTreeMap::new());
        let spec = llama_supervisor()
            .build_spec(&plan, &[], no_image_override(), Some("hf_secret_value"))
            .unwrap();
        assert_eq!(
            spec.secret_env.get("HF_TOKEN").map(String::as_str),
            Some("hf_secret_value")
        );
        assert!(!spec.image.env.contains_key("HF_TOKEN"));
        assert!(!format!("{spec:?}").contains("hf_secret_value"));
    }

    #[test]
    fn a_deployment_llama_cache_env_wins_over_the_default() {
        let env = BTreeMap::from([("LLAMA_CACHE".to_owned(), "/custom".to_owned())]);
        let spec = llama_supervisor()
            .build_spec(
                &llama_plan(llama_argv(), env),
                &[],
                no_image_override(),
                no_hf_token_fallback(),
            )
            .unwrap();
        assert_eq!(
            spec.image.env.get("LLAMA_CACHE").map(String::as_str),
            Some("/custom")
        );
    }

    #[test]
    fn with_gguf_model_uses_the_hugging_face_source_and_recommended_variant() {
        let sup =
            ModalSupervisor::new("a").with_gguf_model(&gguf_model(SourceRegistry::HuggingFace));
        assert_eq!(
            sup.gguf,
            Some(GgufSource {
                repo: "ggml-org/gemma-3-1b-it-GGUF".to_owned(),
                file: "gemma-3-1b-it-Q4_K_M.gguf".to_owned(),
            })
        );
        for registry in [
            SourceRegistry::Local,
            SourceRegistry::Url,
            SourceRegistry::Ollama,
        ] {
            assert!(
                ModalSupervisor::new("a")
                    .with_gguf_model(&gguf_model(registry))
                    .gguf
                    .is_none()
            );
        }
    }

    fn llama_spec_err(extra: &[&str]) -> String {
        let mut argv = llama_argv();
        argv.extend(extra.iter().map(|s| (*s).to_owned()));
        llama_supervisor()
            .build_spec(
                &llama_plan(argv, BTreeMap::new()),
                &[],
                no_image_override(),
                no_hf_token_fallback(),
            )
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn each_local_path_flag_is_rejected_in_both_forms() {
        for flag in LLAMA_LOCAL_PATH_FLAGS {
            let spaced = llama_spec_err(&[flag, "some/local/file"]);
            assert!(
                spaced.contains(flag) && spaced.contains("not supported on Modal"),
                "{spaced}"
            );
            let inline = format!("{flag}=some/local/file");
            let joined = llama_spec_err(&[inline.as_str()]);
            assert!(joined.contains(flag), "{joined}");
        }
    }

    #[test]
    fn each_secret_flag_is_rejected_in_both_forms_without_echoing_the_value() {
        for flag in LLAMA_SECRET_FLAGS {
            let spaced = llama_spec_err(&[flag, "s3cr3t-value"]);
            assert!(
                spaced.contains(flag) && spaced.contains("secret"),
                "{spaced}"
            );
            assert!(!spaced.contains("s3cr3t-value"));
            let inline = format!("{flag}=s3cr3t-value");
            let joined = llama_spec_err(&[inline.as_str()]);
            assert!(joined.contains(flag), "{joined}");
            assert!(!joined.contains("s3cr3t-value"));
        }
    }

    #[test]
    fn no_mmproj_is_appended_unless_the_deployment_declares_multimodal() {
        let build = |multimodal: bool, extra: &[&str]| {
            let mut argv = llama_argv();
            argv.extend(extra.iter().map(|s| (*s).to_owned()));
            let mut plan = llama_plan(argv, BTreeMap::new());
            if multimodal && let Runtime::LlamaCpp(s) = &mut plan.settings {
                s.multimodal = Some(amicu_core::resource::MultimodalSettings {
                    projector: None,
                    offload: Some(true),
                });
            }
            llama_supervisor()
                .build_spec(&plan, &[], no_image_override(), no_hf_token_fallback())
                .unwrap()
                .argv
        };
        let count = |argv: &[String]| argv.iter().filter(|a| *a == "--no-mmproj").count();
        assert_eq!(count(&build(false, &[])), 1);
        assert_eq!(count(&build(false, &["--no-mmproj"])), 1, "no duplicate");
        assert_eq!(count(&build(true, &[])), 0);
    }

    #[test]
    fn llama_waits_on_http_health_while_vllm_keeps_the_tcp_probe() {
        let llama = llama_supervisor()
            .build_spec(
                &llama_plan(llama_argv(), BTreeMap::new()),
                &[],
                no_image_override(),
                no_hf_token_fallback(),
            )
            .unwrap();
        assert_eq!(llama.ready_path.as_deref(), Some("/health"));
        let vllm = ModalSupervisor::new("amicu-dep")
            .build_spec(
                &plan_with(base_argv(), base_settings()),
                &[],
                no_image_override(),
                no_hf_token_fallback(),
            )
            .unwrap();
        assert_eq!(vllm.ready_path, None);
    }
}
