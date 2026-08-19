//! Rendering the visible, secret-free `llama-server` command line.

use std::path::Path;

use amicu_core::resource::LlamaCppSettings;

/// Render the `llama-server` command for `model_path` under `settings`.
///
/// Shape:
/// `llama-server -m <model-path> --port <port> --ctx-size <n> --n-gpu-layers <n>`.
///
/// The text carries no secrets: llama.cpp settings are plain launch flags.
#[must_use]
pub fn render_command(model_path: &Path, settings: &LlamaCppSettings) -> String {
    format!(
        "llama-server -m {model} --port {port} --ctx-size {ctx} --n-gpu-layers {ngl}",
        model = model_path.display(),
        port = settings.port,
        ctx = settings.context_length,
        ngl = settings.n_gpu_layers,
    )
}
