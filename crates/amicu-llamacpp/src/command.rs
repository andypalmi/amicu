//! Rendering the `llama-server` launch: a structured argv (the source of truth
//! supervisors spawn from) plus a shell-quoted display string.

use std::path::Path;

use amicu_core::resource::LlamaCppSettings;

/// Every flag spelling llama-server accepts for a field this crate renders
/// from a typed setting, long form first (the one [`render_command`] actually
/// emits). This table must stay exact - a spelling missing here is a spelling
/// `extra_args` can use to silently re-assert (or override, via
/// llama-server's own last-flag-wins argv parsing) a value amicu already
/// rendered. `--port` has no short alias. Spellings verified against
/// `common/arg.cpp` (amicu-planning#151): `--temp`/`--temperature` share one
/// `add_opt`, `-ctk`/`-ctv` are the K/V cache-type short forms, `-b`/`-ub` are
/// the batch/ubatch short forms, `-np` is `--parallel`, `-fa` is
/// `--flash-attn`. `--top-k`/`--top-p`/`--min-p`/`--repeat-penalty` have no
/// short alias. `--chat-template` and `--chat-template-file` (amicu-planning#151,
/// PR B) likewise have no documented short alias anywhere in this repo, so
/// none is assumed. `--jinja`/`--no-jinja` are one `add_opt`'s paired
/// spellings for the same boolean field (`common/arg.cpp`), grouped as one
/// entry below; `--reasoning-format` has no short alias. Shared with
/// [`crate::adapter`] diagnostics (`AMICU-CAP-002`), which flatten this via
/// [`is_typed_flag`] so a duplicated flag in `extra_args` is detected against
/// exactly what [`render_command`] renders, not a hand-maintained shadow
/// list.
pub(crate) const TYPED_FLAGS: &[&[&str]] = &[
    &["--model", "-m"],
    &["--port"],
    &["--ctx-size", "-c"],
    &["--n-gpu-layers", "-ngl", "--gpu-layers"],
    &["--temp", "--temperature"],
    &["--top-p"],
    &["--top-k"],
    &["--min-p"],
    &["--repeat-penalty"],
    &["--cache-type-k", "-ctk"],
    &["--cache-type-v", "-ctv"],
    &["--batch-size", "-b"],
    &["--ubatch-size", "-ub"],
    &["--parallel", "-np"],
    &["--flash-attn", "-fa"],
    &["--chat-template"],
    &["--chat-template-file"],
    &["--jinja", "--no-jinja"],
    &["--reasoning-format"],
    // amicu-planning#151 PR C - the `speculative {}` block. llama.cpp renamed
    // this whole family to a `--spec-draft-*` prefix and kept the older
    // spellings as aliases on the same `add_opt` (`common/arg.cpp`), except
    // `--draft`/`--draft-n`/`--draft-max` and `--draft-min`/`--draft-n-min`,
    // which were *removed* (they now abort with "the argument has been
    // removed. use --spec-draft-n-max ..."). Every spelling is listed here,
    // the removed ones included: an `extra_args` entry using a removed
    // spelling still names a setting amicu owns, and reporting that as
    // `AMICU-CAP-002` is more useful than letting llama-server abort on it at
    // launch.
    &["--model-draft", "--spec-draft-model", "-md"],
    &["--spec-draft-n-max", "--draft-max", "--draft-n", "--draft"],
    &["--spec-draft-n-min", "--draft-min", "--draft-n-min"],
    &["--draft-p-min", "--spec-draft-p-min"],
    &[
        "--n-gpu-layers-draft",
        "--spec-draft-ngl",
        "--gpu-layers-draft",
        "-ngld",
    ],
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

/// `LLAMA_ARG_<X>` env keys whose mirrored flag does not follow the
/// mechanical "lowercase, `_` -> `-`" rule [`llama_arg_env_flag`] otherwise
/// applies - llama-server's own `set_env("LLAMA_ARG_BATCH")`/
/// `set_env("LLAMA_ARG_UBATCH")`/`set_env("LLAMA_ARG_N_PARALLEL")`
/// (`common/arg.cpp`) name the env var after the *field*
/// (`n_batch`/`n_ubatch`/`n_parallel`), not the flag spelling, so the
/// mechanical rule would derive `--batch`/`--ubatch`/`--n-parallel` - none a
/// real flag - and silently miss the conflict. `LLAMA_ARG_THINK` is the same
/// situation for `--reasoning-format` (amicu-planning#151 PR B, review
/// hardening): `common/arg.cpp`'s `add_opt` for `--reasoning-format` calls
/// `.set_env("LLAMA_ARG_THINK")` - named after the internal `params.think`
/// field, not the flag - so the mechanical rule would derive
/// `--reasoning-format` from the *wrong* env key (`LLAMA_ARG_REASONING_FORMAT`,
/// which llama-server does not read at all) while missing the real one.
///
/// The `speculative {}` block (amicu-planning#151 PR C) deliberately adds no
/// entry here: each of its five env vars mechanically derives a flag spelling
/// that is already listed as an alias in [`TYPED_FLAGS`], so the fallback rule
/// alone catches them - `LLAMA_ARG_SPEC_DRAFT_MODEL` -> `--spec-draft-model`,
/// `LLAMA_ARG_SPEC_DRAFT_N_MAX` -> `--spec-draft-n-max`,
/// `LLAMA_ARG_SPEC_DRAFT_N_MIN` -> `--spec-draft-n-min`,
/// `LLAMA_ARG_SPEC_DRAFT_P_MIN` -> `--spec-draft-p-min`,
/// `LLAMA_ARG_N_GPU_LAYERS_DRAFT` -> `--n-gpu-layers-draft`. The two legacy
/// env vars llama-server still registers on its *removed* draft args,
/// `LLAMA_ARG_DRAFT_MAX` and `LLAMA_ARG_DRAFT_MIN`, likewise derive
/// `--draft-max`/`--draft-min`, both listed.
const ENV_FLAG_OVERRIDES: &[(&str, &str)] = &[
    ("LLAMA_ARG_BATCH", "--batch-size"),
    ("LLAMA_ARG_UBATCH", "--ubatch-size"),
    ("LLAMA_ARG_N_PARALLEL", "--parallel"),
    ("LLAMA_ARG_THINK", "--reasoning-format"),
];

/// Map a `LLAMA_ARG_<X>` environment variable name to the CLI flag
/// llama-server treats it as equivalent to: first check
/// [`ENV_FLAG_OVERRIDES`] for a known exception, then fall back to
/// lowercasing the suffix, turning `_` into `-`, and prefixing `--` (e.g.
/// `LLAMA_ARG_CTX_SIZE` -> `--ctx-size`, `LLAMA_ARG_HOST` -> `--host`).
/// llama-server reads `LLAMA_ARG_*` env vars as a mirror of its CLI flags, so
/// a settings `env` entry can re-assert (or override) anything a typed field
/// or the denylist governs just as effectively as an `extra_args` flag can -
/// this closes that channel for [`crate::adapter`]'s
/// `AMICU-CAP-002`/`AMICU-CAP-003` checks. Returns `None` for a key with no
/// `LLAMA_ARG_` prefix (or an empty suffix).
#[must_use]
pub(crate) fn llama_arg_env_flag(key: &str) -> Option<String> {
    if let Some((_, flag)) = ENV_FLAG_OVERRIDES.iter().find(|(k, _)| *k == key) {
        return Some((*flag).to_owned());
    }
    let suffix = key.strip_prefix("LLAMA_ARG_")?;
    if suffix.is_empty() {
        return None;
    }
    Some(format!("--{}", suffix.to_lowercase().replace('_', "-")))
}

/// Render the `llama-server` launch for `model_path` under `settings`.
///
/// Typed flags are emitted first (`-m`, `--port`, `--ctx-size`,
/// `--n-gpu-layers`, then any set sampling/KV-cache/batching/flash-attention/
/// chat-template/jinja/reasoning-format/speculative field in field-declaration
/// order), then `settings.extra_args` verbatim, in declaration order - it is
/// argv order. Duplicate/denylisted flags are caught as diagnostics before
/// this ever runs (`crate::adapter`); this function itself renders whatever it
/// is given. `f32` fields render via `{}` (plain decimal, e.g. `0.6`), never
/// scientific notation, at the magnitudes these settings take.
/// `chat_template`/`chat_template_file` are the one pair this function
/// itself refuses to render together (see the match below) rather than
/// leaving that entirely to the caller's pre-check.
///
/// `draft_model_path` is the resolved GGUF path of the `speculative {}`
/// block's draft model, resolved by the caller from the registry exactly like
/// `model_path` is (amicu-planning#151 PR C). It is passed separately rather
/// than read off `settings` because `settings.speculative.model` is a *model
/// resource reference*, not a path. `Some(_)` with no `settings.speculative`
/// (or `None` with one) renders no draft flags at all: the two travel
/// together, and the adapter never produces that combination.
///
/// Returns `(argv, text)`: `argv` is the real argument vector (`argv[0]` =
/// `llama-server`) that supervisors exec directly, and `text` is a shell-quoted
/// join for display only. Neither carries secrets: llama.cpp settings are plain
/// launch flags; secrets enter only through the process environment at apply
/// time (`ServiceSupervisor::start`).
#[must_use]
pub fn render_command(
    model_path: &Path,
    draft_model_path: Option<&Path>,
    settings: &LlamaCppSettings,
) -> (Vec<String>, String) {
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
    if let Some(v) = settings.temperature {
        argv.push("--temp".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.top_p {
        argv.push("--top-p".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.top_k {
        argv.push("--top-k".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.min_p {
        argv.push("--min-p".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.repeat_penalty {
        argv.push("--repeat-penalty".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.cache_type_k {
        argv.push("--cache-type-k".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.cache_type_v {
        argv.push("--cache-type-v".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.batch_size {
        argv.push("--batch-size".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.ubatch_size {
        argv.push("--ubatch-size".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.parallel_slots {
        argv.push("--parallel".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.flash_attention {
        argv.push("--flash-attn".to_owned());
        argv.push(v.to_string());
    }
    // `chat_template`/`chat_template_file` are mutually exclusive
    // (`AMICU-CAP-005`, `crate::adapter::chat_template_conflict`): when both
    // are set, this renders neither rather than guessing which one wins.
    match (&settings.chat_template, &settings.chat_template_file) {
        (Some(v), None) => {
            argv.push("--chat-template".to_owned());
            argv.push(v.clone());
        }
        (None, Some(v)) => {
            argv.push("--chat-template-file".to_owned());
            argv.push(v.clone());
        }
        (Some(_), Some(_)) | (None, None) => {}
    }
    // `--jinja`/`--no-jinja` is a paired bare-flag toggle (`common/arg.cpp`):
    // `Some(true)`/`Some(false)` render the matching bare flag - an explicit
    // `false` renders `--no-jinja` so it overrides an inherited
    // `LLAMA_ARG_JINJA=1` rather than just omitting the flag - and `None`
    // renders nothing, leaving llama-server's own default/environment as-is.
    match settings.jinja {
        Some(true) => argv.push("--jinja".to_owned()),
        Some(false) => argv.push("--no-jinja".to_owned()),
        None => {}
    }
    if let Some(v) = settings.reasoning_format {
        argv.push("--reasoning-format".to_owned());
        argv.push(v.to_string());
    }
    // Speculative decoding (amicu-planning#151 PR C). The draft path and the
    // typed draft knobs render together: without a resolved draft model there
    // is nothing for the draft parameters to apply to, and llama-server
    // ignores them. See `SpeculativeSettings` for why each spelling below is
    // the one emitted (`--spec-draft-n-max`/`-n-min` because their legacy
    // `--draft-max`/`--draft-min` spellings were removed upstream and now
    // abort; the others because their legacy spelling is still a live alias
    // and works on more builds).
    if let (Some(path), Some(spec)) = (draft_model_path, settings.speculative.as_ref()) {
        argv.push("--model-draft".to_owned());
        argv.push(path.display().to_string());
        if let Some(v) = spec.n_max {
            argv.push("--spec-draft-n-max".to_owned());
            argv.push(v.to_string());
        }
        if let Some(v) = spec.n_min {
            argv.push("--spec-draft-n-min".to_owned());
            argv.push(v.to_string());
        }
        if let Some(v) = spec.p_min {
            argv.push("--draft-p-min".to_owned());
            argv.push(v.to_string());
        }
        if let Some(v) = spec.n_gpu_layers {
            argv.push("--n-gpu-layers-draft".to_owned());
            argv.push(v.to_string());
        }
    }
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
