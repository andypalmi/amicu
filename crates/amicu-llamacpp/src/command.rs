//! Rendering the `llama-server` launch: a structured argv (the source of truth
//! supervisors spawn from) plus a shell-quoted display string.

use std::path::Path;

use amicu_core::resource::LlamaCppSettings;

/// Render the `llama-server` launch for `model_path` under `settings`.
///
/// Returns `(argv, text)`: `argv` is the real argument vector (`argv[0]` =
/// `llama-server`) that supervisors exec directly, and `text` is a shell-quoted
/// join for display only. Neither carries secrets: llama.cpp settings are plain
/// launch flags.
#[must_use]
pub fn render_command(model_path: &Path, settings: &LlamaCppSettings) -> (Vec<String>, String) {
    let argv = vec![
        "llama-server".to_owned(),
        "-m".to_owned(),
        model_path.display().to_string(),
        "--port".to_owned(),
        settings.port.to_string(),
        "--ctx-size".to_owned(),
        settings.context_length.to_string(),
        "--n-gpu-layers".to_owned(),
        settings.n_gpu_layers.to_string(),
    ];
    let text = shell_join(&argv);
    (argv, text)
}

/// Join `argv` into a display line, single-quoting any token that is not a
/// bare word so the string round-trips through a shell unambiguously.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote a single token for display: bare words pass through, everything else
/// is single-quoted with embedded quotes escaped.
fn shell_quote(token: &str) -> String {
    let safe = !token.is_empty()
        && token.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'=' | b':')
        });
    if safe {
        token.to_owned()
    } else {
        format!("'{}'", token.replace('\'', r"'\''"))
    }
}
