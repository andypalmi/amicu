//! Rendering the `llama-server` launch: a structured argv (the source of truth
//! supervisors spawn from) plus a shell-quoted display string.

use std::path::Path;

use amicu_core::resource::LlamaCppSettings;

/// Every flag spelling llama-server accepts for a field this crate renders
/// from a typed setting, long form first (the one [`render_command`] actually
/// emits). This table must stay exact - a spelling missing here is a spelling
/// `extra_args` can use to silently re-assert (or override, via
/// llama-server's own last-flag-wins argv parsing) a value amicu already
/// rendered. `--port` has no short alias. Shared with
/// [`crate::adapter`] diagnostics (`AMICU-CAP-002`), which flatten this via
/// [`is_typed_flag`] so a duplicated flag in `extra_args` is detected
/// against exactly what [`render_command`] renders, not a hand-maintained
/// shadow list.
pub(crate) const TYPED_FLAGS: &[&[&str]] = &[
    &["--model", "-m"],
    &["--port"],
    &["--ctx-size", "-c"],
    &["--n-gpu-layers", "-ngl", "--gpu-layers"],
];

/// Whether `flag` is any alias of any typed field's flag (see
/// [`TYPED_FLAGS`]).
#[must_use]
pub(crate) fn is_typed_flag(flag: &str) -> bool {
    TYPED_FLAGS.iter().any(|aliases| aliases.contains(&flag))
}

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

/// Environment variable names that let a spawned process load arbitrary
/// dynamic libraries into `llama-server`, regardless of any CLI-flag
/// equivalence - checked directly against `settings.env` keys (amicu
/// review hardening, amicu-planning#150), independent of the `LLAMA_ARG_*`
/// mapping below. Raises `AMICU-CAP-003`.
pub(crate) const DENYLISTED_ENV_KEYS: &[&str] = &[
    "LD_PRELOAD",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "LD_LIBRARY_PATH",
];

/// Map a `LLAMA_ARG_<X>` environment variable name to the CLI flag
/// llama-server treats it as equivalent to: lowercase the suffix, turn `_`
/// into `-`, and prefix `--` (e.g. `LLAMA_ARG_CTX_SIZE` -> `--ctx-size`,
/// `LLAMA_ARG_HOST` -> `--host`). llama-server reads `LLAMA_ARG_*` env vars
/// as a mirror of its CLI flags, so a settings `env` entry can re-assert (or
/// override) anything a typed field or the denylist governs just as
/// effectively as an `extra_args` flag can - this closes that channel for
/// [`crate::adapter`]'s `AMICU-CAP-002`/`AMICU-CAP-003` checks. Returns
/// `None` for a key with no `LLAMA_ARG_` prefix (or an empty suffix).
#[must_use]
pub(crate) fn llama_arg_env_flag(key: &str) -> Option<String> {
    let suffix = key.strip_prefix("LLAMA_ARG_")?;
    if suffix.is_empty() {
        return None;
    }
    Some(format!("--{}", suffix.to_lowercase().replace('_', "-")))
}

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
