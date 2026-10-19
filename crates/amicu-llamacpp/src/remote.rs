//! The provider-neutral half of running a rendered launch in a remote
//! container (Modal, RunPod): which argv the container runs and how the
//! deployment's environment splits into plain and secret variables.
//!
//! The local renderers (`amicu_vllm::command`, [`crate::command`]) produce an
//! argv for a process on this machine: loopback bind, a local GGUF path. A
//! container on someone else's GPU needs that argv rewritten - bind all
//! interfaces, fetch the GGUF from Hugging Face (`-hf <repo> -hff <file>`),
//! refuse anything that names this machine's filesystem or would put a secret
//! on argv. Every remote supervisor needs exactly the same rewrite, so it lives
//! here, below both provider crates. Error messages name the provider
//! (`provider`, e.g. `"Modal"`) so they read the same as before the split.

use std::collections::BTreeMap;

use amicu_core::resource::{Model, Runtime, SourceRegistry};
use amicu_registry::GgufResolver;
use amicu_runtime::{LaunchPlan, RuntimeKind};

use crate::error::LlamaCppError;
use crate::supervisor::ResolvedSecret;

/// The vLLM image remote backends run unless overridden: the official
/// OpenAI-compatible server image.
pub const DEFAULT_VLLM_IMAGE: &str = "vllm/vllm-openai:v0.29.0";

/// The llama.cpp image remote backends run unless overridden: the official
/// CUDA server build (`llama-server` at `/app/llama-server`, which is also the
/// image's `ENTRYPOINT`).
pub const DEFAULT_LLAMACPP_IMAGE: &str = "ghcr.io/ggml-org/llama.cpp:server-cuda-b11515";

/// Where `llama-server` lives in [`DEFAULT_LLAMACPP_IMAGE`] (off `PATH`).
pub const LLAMACPP_IMAGE_SERVER_PATH: &str = "/app/llama-server";

/// The port [`DEFAULT_LLAMACPP_IMAGE`] expects: its Docker `HEALTHCHECK`
/// curls `localhost:8080/health`.
pub const LLAMACPP_IMAGE_PORT: u16 = 8080;

/// Lowest host CUDA version [`DEFAULT_VLLM_IMAGE`] runs on (its
/// `NVIDIA_REQUIRE_CUDA` is `cuda>=13.0`).
pub const DEFAULT_VLLM_IMAGE_MIN_CUDA: &str = "13.0";

/// Lowest host CUDA version [`DEFAULT_LLAMACPP_IMAGE`] runs on (its
/// `NVIDIA_REQUIRE_CUDA` is `cuda>=12.8`).
pub const DEFAULT_LLAMACPP_IMAGE_MIN_CUDA: &str = "12.8";

/// The operator environment variable a Hugging Face token is read from when
/// the launch plan carries no resolved `HF_TOKEN` secret. Read from the
/// operator's own process, never from the deployment's `env` block, and only
/// ever forwarded as a secret.
pub const HF_TOKEN_ENV_VAR: &str = "HF_TOKEN";

/// Default image for a runtime family.
#[must_use]
pub const fn default_image(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Vllm => DEFAULT_VLLM_IMAGE,
        RuntimeKind::LlamaCpp => DEFAULT_LLAMACPP_IMAGE,
    }
}

/// Lowest host CUDA version (`major.minor`) the runtime's default image needs.
#[must_use]
pub const fn default_image_min_cuda(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Vllm => DEFAULT_VLLM_IMAGE_MIN_CUDA,
        RuntimeKind::LlamaCpp => DEFAULT_LLAMACPP_IMAGE_MIN_CUDA,
    }
}

/// A GGUF file on Hugging Face: what a remote llama-server fetches with
/// `-hf <repo> -hff <file>` instead of the local `-m <path>` the renderer
/// emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufSource {
    /// Hugging Face repo id (`org/model`).
    pub repo: String,
    /// File name within the repo.
    pub file: String,
}

impl GgufSource {
    /// The Hugging Face GGUF for `model`: its source repo and the same
    /// recommended variant `LlamaCppAdapter::plan_launch` renders. `None` for a
    /// model from any other registry or with no variants.
    #[must_use]
    pub fn for_model(model: &Model) -> Option<Self> {
        (model.source.registry == SourceRegistry::HuggingFace)
            .then(|| {
                GgufResolver::from_variants(&model.variants)
                    .recommended()
                    .cloned()
            })
            .flatten()
            .map(|variant| Self {
                repo: model.source.reference.clone(),
                file: variant.file,
            })
    }
}

/// Whether `model_reference` looks like a local filesystem path rather than
/// a Hugging Face repo id (`"org/model"`). Deliberately conservative - it
/// only catches the unambiguous cases (a leading `/`, `./`, `../`, or `~`) -
/// because false positives here would block a legitimate deploy, whereas a
/// false negative just reaches the provider's own "no such repo" error instead
/// of amicu's clearer one.
#[must_use]
fn looks_like_local_path(model_reference: &str) -> bool {
    model_reference.starts_with('/')
        || model_reference.starts_with("./")
        || model_reference.starts_with("../")
        || model_reference.starts_with('~')
}

/// llama-server flags that name a Hugging Face repo (`-hf <repo>[:quant]`).
pub const LLAMA_HF_FLAGS: [&str; 3] = ["-hf", "-hfr", "--hf-repo"];
/// llama-server flags that name a local model file.
pub const LLAMA_MODEL_FLAGS: [&str; 2] = ["-m", "--model"];
/// llama-server flags whose value is a local file path (besides the model
/// and draft model handled separately).
pub const LLAMA_LOCAL_PATH_FLAGS: [&str; 7] = [
    "--mmproj",
    "-mm",
    "--chat-template-file",
    "--lora",
    "--lora-scaled",
    "--control-vector",
    "--control-vector-scaled",
];
/// llama-server flags that carry (or point at) a secret; on a container's
/// argv they would leak.
pub const LLAMA_SECRET_FLAGS: [&str; 4] = ["-hft", "--hf-token", "--api-key-file", "--api-key"];
/// llama-server flags that name a speculative-decoding draft model (a local
/// path when rendered by amicu).
pub const LLAMA_DRAFT_FLAGS: [&str; 3] = ["-md", "--model-draft", "--spec-draft-model"];

/// Whether `argv` is a `llama-server` launch (`argv[0]` is `llama-server`,
/// bare or by absolute path).
#[must_use]
pub fn is_llama_server(argv: &[String]) -> bool {
    argv.first()
        .is_some_and(|a| a == "llama-server" || a.ends_with("/llama-server"))
}

/// The first of `flags` present in `argv[1..]`, as `--flag value` or
/// `--flag=value`.
#[must_use]
pub fn first_flag<'a>(argv: &[String], flags: &[&'a str]) -> Option<&'a str> {
    argv.iter().skip(1).find_map(|arg| {
        let name = arg.split_once('=').map_or(arg.as_str(), |(n, _)| n);
        flags.iter().copied().find(|f| *f == name)
    })
}

/// The value of the first of `flags` in `argv[1..]`, accepting both
/// `--flag value` and `--flag=value`.
#[must_use]
pub fn flag_value<'a>(argv: &'a [String], flags: &[&str]) -> Option<&'a str> {
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
#[must_use]
pub fn model_reference(argv: &[String]) -> Option<&str> {
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

/// Refuses a launch whose model reference is a local path: a remote
/// container has no access to the operator's filesystem, so the server there
/// must be given a Hugging Face repo id (vLLM) or `-hf` GGUF repo
/// (llama-server).
///
/// # Errors
/// [`LlamaCppError::Supervisor`] naming the path, never a secret.
pub fn reject_local_path_model(argv: &[String], provider: &str) -> Result<(), LlamaCppError> {
    if is_llama_server(argv) && flag_value(argv, &LLAMA_HF_FLAGS).is_none() {
        // A llama-server `-m` value is always a file path, however relative
        // (the amicu renderer emits `models/<id>/<file>`), so it never
        // reaches `looks_like_local_path`'s prefix heuristic.
        if let Some(path) = flag_value(argv, &LLAMA_MODEL_FLAGS) {
            return Err(LlamaCppError::Supervisor(format!(
                "{provider} deployments need a Hugging Face GGUF reference (a model whose source \
                 registry is hugging_face, e.g. 'ggml-org/gemma-3-1b-it-GGUF'), not a local \
                 model file; got '{path}' - {provider}'s container cannot see this machine's \
                 filesystem"
            )));
        }
    }
    match model_reference(argv) {
        Some(reference) if looks_like_local_path(reference) => {
            Err(LlamaCppError::Supervisor(format!(
                "{provider} deployments need a Hugging Face model id (e.g. \
                 'meta-llama/Llama-3.1-8B-Instruct'), not a local path; got '{reference}' - \
                 {provider}'s container cannot see this machine's filesystem"
            )))
        }
        _ => Ok(()),
    }
}

/// Rewrites `--host 127.0.0.1` (what the local renderers always emit) to
/// `--host 0.0.0.0`: a provider reaches the container over its internal
/// network, never loopback.
#[must_use]
pub fn bind_all_interfaces(argv: &[String]) -> Vec<String> {
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

/// The llama-server argv for a remote container: the renderer's local
/// `-m <path>` is replaced by `-hf <repo> -hff <file>` from `gguf` (unless the
/// launch already names a Hugging Face repo itself), and anything that points
/// at this machine's filesystem is rejected.
fn rewrite_gguf_args(
    argv: &[String],
    gguf: Option<&GgufSource>,
    provider: &str,
) -> Result<Vec<String>, LlamaCppError> {
    if flag_value(argv, &LLAMA_DRAFT_FLAGS).is_some() {
        return Err(LlamaCppError::Supervisor(format!(
            "speculative decoding (a draft model) is not yet supported on {provider}: the draft \
             GGUF is a local path {provider}'s container cannot see"
        )));
    }
    if flag_value(argv, &LLAMA_HF_FLAGS).is_some() {
        let out = without_model_file(argv);
        reject_local_path_model(&out, provider)?;
        return Ok(out);
    }
    let Some(source) = gguf else {
        // No Hugging Face source known: surface the local-path error.
        reject_local_path_model(argv, provider)?;
        return Err(LlamaCppError::Supervisor(format!(
            "{provider} deployments need a Hugging Face GGUF reference (a model whose source \
             registry is hugging_face, with at least one variant)"
        )));
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
    reject_local_path_model(&out, provider)?;
    Ok(out)
}

/// Validates a llama-server launch for a remote container and rewrites its
/// model flags (see [`rewrite_gguf_args`]). Rejects, before any provider
/// call, flags that point at this machine's filesystem
/// ([`LLAMA_LOCAL_PATH_FLAGS`]) or would put a secret on the container's argv
/// ([`LLAMA_SECRET_FLAGS`]; only the flag name is reported, never a value).
/// `-hf` auto-downloads the repo's multimodal projector, so `--no-mmproj` is
/// appended unless the deployment declared `multimodal`.
fn hf_llama_argv(
    argv: &[String],
    gguf: Option<&GgufSource>,
    has_multimodal: bool,
    provider: &str,
) -> Result<Vec<String>, LlamaCppError> {
    if !is_llama_server(argv) {
        return Err(LlamaCppError::Supervisor(format!(
            "the {provider} supervisor expected a `llama-server` command for a llama.cpp launch"
        )));
    }
    if let Some(flag) = first_flag(argv, &LLAMA_LOCAL_PATH_FLAGS) {
        return Err(LlamaCppError::Supervisor(format!(
            "'{flag}' is not supported on {provider} yet: it names a local file {provider}'s \
             container cannot see"
        )));
    }
    if let Some(flag) = first_flag(argv, &LLAMA_SECRET_FLAGS) {
        return Err(LlamaCppError::Supervisor(format!(
            "'{flag}' is not allowed on {provider}: it would bake a secret into the container \
             image's command line; pass the Hugging Face token as the HF_TOKEN secret \
             instead"
        )));
    }
    let mut out = rewrite_gguf_args(argv, gguf, provider)?;
    if !has_multimodal && first_flag(&out, &["--no-mmproj"]).is_none() {
        out.push("--no-mmproj".to_owned());
    }
    Ok(out)
}

/// The argv a remote container runs for `plan`: the renderer's local launch
/// with the remote rewrites applied (`-hf`/`-hff` and `--no-mmproj` for
/// llama.cpp, `--host 0.0.0.0` for both).
///
/// # Errors
/// [`LlamaCppError::Supervisor`] if `plan` is not a command launch or names
/// something the container cannot reach (a local model path, a secret flag,
/// ...).
pub fn launch_argv(
    plan: &LaunchPlan,
    gguf: Option<&GgufSource>,
    provider: &str,
) -> Result<Vec<String>, LlamaCppError> {
    let argv = plan.rendered.argv().ok_or_else(|| {
        LlamaCppError::Supervisor(format!("{provider} supervisor expects a command launch"))
    })?;
    Ok(match &plan.settings {
        Runtime::Vllm(_) => {
            reject_local_path_model(argv, provider)?;
            bind_all_interfaces(argv)
        }
        Runtime::LlamaCpp(s) => bind_all_interfaces(&hf_llama_argv(
            argv,
            gguf,
            s.multimodal.is_some(),
            provider,
        )?),
    })
}

/// A launch's environment, split by sensitivity: `plain` is the deployment's
/// own `env` block (visible in the provider's configuration), `secret` holds
/// every resolved secret value (plus the operator's `HF_TOKEN` fallback) and
/// must only reach the provider through its secret mechanism.
///
/// `Debug` prints the secret variable names only.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteEnv {
    /// Plain variables from the deployment's `env` block.
    pub plain: BTreeMap<String, String>,
    /// Secret variables: name to resolved value.
    pub secret: BTreeMap<String, String>,
}

impl std::fmt::Debug for RemoteEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEnv")
            .field("plain_keys", &self.plain.keys().collect::<Vec<_>>())
            .field("secret_keys", &self.secret.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Splits `plan`'s environment for a remote container: the settings `env`
/// block becomes [`RemoteEnv::plain`]; every resolved secret (including the
/// endpoint API key under the runtime's key variable) becomes
/// [`RemoteEnv::secret`], and `hf_token_fallback` is added under
/// [`HF_TOKEN_ENV_VAR`] when neither already sets it.
///
/// # Errors
/// [`LlamaCppError::Supervisor`] if a settings `env` key collides with a
/// resolved secret name (which should win is not guessable, the same rule the
/// local supervisor applies), or a secret value is not UTF-8. Neither message
/// contains a value.
pub fn split_env(
    plan: &LaunchPlan,
    secrets: &[ResolvedSecret],
    hf_token_fallback: Option<&str>,
) -> Result<RemoteEnv, LlamaCppError> {
    let settings_env = match &plan.settings {
        Runtime::Vllm(s) => &s.env,
        Runtime::LlamaCpp(s) => &s.env,
    };
    if let Some(s) = secrets.iter().find(|s| settings_env.contains_key(&s.name)) {
        return Err(LlamaCppError::Supervisor(format!(
            "env var '{}' is set both by the deployment's `env` block and a resolved secret",
            s.name
        )));
    }
    let plain_env: BTreeMap<String, String> = settings_env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut secret = BTreeMap::new();
    for s in secrets {
        let value = s.value.expose_secret_str().map_err(|_| {
            LlamaCppError::Supervisor(format!("secret '{}' is not valid utf-8", s.name))
        })?;
        secret.insert(s.name.clone(), value.to_owned());
    }
    if !secret.contains_key(HF_TOKEN_ENV_VAR)
        && !plain_env.contains_key(HF_TOKEN_ENV_VAR)
        && let Some(token) = hf_token_fallback
    {
        secret.insert(HF_TOKEN_ENV_VAR.to_owned(), token.to_owned());
    }
    Ok(RemoteEnv {
        plain: plain_env,
        secret,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::ByteSize;
    use amicu_core::id::DeploymentId;
    use amicu_core::resource::{LlamaCppSettings, VllmSettings};
    use amicu_core::secret::SecretValue;
    use amicu_runtime::{RenderedLaunch, ResourceEstimate};

    fn plan(argv: &[&str], settings: Runtime) -> LaunchPlan {
        let argv: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        LaunchPlan {
            deployment: DeploymentId::from("dep"),
            runtime: RuntimeKind::of(&settings),
            rendered: RenderedLaunch::Command {
                text: argv.join(" "),
                argv,
            },
            settings,
            secret_refs: Vec::new(),
            api_key: None,
            service_label: None,
            estimate: ResourceEstimate {
                estimated_memory: ByteSize::from_bytes(0),
                gpu_count: 1,
                kv_cache: None,
            },
        }
    }

    #[test]
    fn messages_name_the_provider() {
        let p = plan(
            &["vllm", "serve", "/models/x", "--host", "127.0.0.1"],
            Runtime::Vllm(VllmSettings::default()),
        );
        let err = launch_argv(&p, None, "RunPod").unwrap_err().to_string();
        assert!(err.contains("RunPod deployments need"), "{err}");
        assert!(err.contains("RunPod's container"), "{err}");
    }

    #[test]
    fn llama_launch_is_rewritten_to_hf_and_all_interfaces() {
        let p = plan(
            &[
                "llama-server",
                "-m",
                "models/g/g.gguf",
                "--host",
                "127.0.0.1",
            ],
            Runtime::LlamaCpp(LlamaCppSettings::default()),
        );
        let src = GgufSource {
            repo: "org/g-GGUF".into(),
            file: "g.gguf".into(),
        };
        assert_eq!(
            launch_argv(&p, Some(&src), "RunPod").unwrap(),
            [
                "llama-server",
                "-hf",
                "org/g-GGUF",
                "-hff",
                "g.gguf",
                "--host",
                "0.0.0.0",
                "--no-mmproj"
            ]
        );
    }

    #[test]
    fn split_env_separates_plain_and_secret_and_hides_values_in_debug() {
        let s = VllmSettings {
            env: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
            ..Default::default()
        };
        let p = plan(&["vllm", "serve", "m"], Runtime::Vllm(s));
        let secrets = [ResolvedSecret {
            name: "VLLM_API_KEY".into(),
            value: SecretValue::new("sk-very-secret".to_owned()),
        }];
        let env = split_env(&p, &secrets, Some("hf_fallback")).unwrap();
        assert_eq!(env.plain.get("A").map(String::as_str), Some("1"));
        assert_eq!(
            env.secret.get("VLLM_API_KEY").map(String::as_str),
            Some("sk-very-secret")
        );
        assert_eq!(
            env.secret.get(HF_TOKEN_ENV_VAR).map(String::as_str),
            Some("hf_fallback")
        );
        let dbg = format!("{env:?}");
        assert!(!dbg.contains("sk-very-secret") && !dbg.contains("hf_fallback"));
    }
}
