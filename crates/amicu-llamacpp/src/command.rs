//! Rendering the `llama-server` launch: a structured argv (the source of truth
//! supervisors spawn from) plus a shell-quoted display string.

use std::path::Path;

use amicu_core::resource::LlamaCppSettings;

/// The flags a typed field renders, in the order [`render_command`] emits
/// them. Shared with [`crate::adapter`] diagnostics (`AMICU-CAP-002`) so a
/// duplicated flag in `extra_args` is detected against exactly what this
/// function actually renders, not a hand-maintained shadow list.
pub(crate) const TYPED_FLAGS: &[&str] = &["--port", "--ctx-size", "--n-gpu-layers"];

/// Flags `extra_args` may never carry, checked by
/// [`crate::adapter::LlamaCppAdapter::validate`]/`plan_launch`
/// (`AMICU-CAP-003`).
///
/// - `--host`: amicu owns the bind address (`ServiceSupervisor::start`
///   derives the health-check endpoint from the typed `port` alone; a
///   user-supplied `--host` would desync the two).
/// - `--api-key`, `--api-key-file`, `--ssl-key-file`: each puts secret
///   material, or a path to it, directly into argv, which is visible to any
///   process that can read `/proc/<pid>/cmdline` or a process listing -
///   exactly what secret-designated `env` (amicu-planning#119) exists to
///   avoid.
pub(crate) const DENYLISTED_FLAGS: &[&str] =
    &["--host", "--api-key", "--api-key-file", "--ssl-key-file"];

/// Render the `llama-server` launch for `model_path` under `settings`.
///
/// Typed flags are emitted first (`-m`, `--port`, `--ctx-size`,
/// `--n-gpu-layers`), then `settings.extra_args` verbatim, in declaration
/// order - it is argv order. Duplicate/denylisted flags are caught as
/// diagnostics before this ever runs (`crate::adapter`); this function
/// itself renders whatever it is given.
///
/// Returns `(argv, text)`: `argv` is the real argument vector (`argv[0]` =
/// `llama-server`) that supervisors exec directly, and `text` is a shell-quoted
/// join for display only. Neither carries secrets: llama.cpp settings are plain
/// launch flags; secrets enter only through the process environment at apply
/// time (`ServiceSupervisor::start`).
#[must_use]
pub fn render_command(model_path: &Path, settings: &LlamaCppSettings) -> (Vec<String>, String) {
    let mut argv = vec![
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
    argv.extend(settings.extra_args.iter().cloned());
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
