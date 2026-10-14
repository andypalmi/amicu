//! Rendering the `vllm serve` launch: a structured argv (the source of truth
//! supervisors spawn from) plus a shell-quoted display string.
//!
//! Mirrors `amicu_llamacpp::command`'s shape: typed
//! fields render first, `extra_args` verbatim after, argv is what actually
//! spawns, `text` is display-only.

use amicu_core::resource::VllmSettings;

/// Every flag spelling `vllm serve` accepts for a field this crate renders
/// from a typed setting, long form first (the one [`render_command`] actually
/// emits). Shared with [`crate::adapter`] diagnostics (`AMICU-CAP-002`), which
/// flatten this via [`is_typed_flag`] so a duplicated flag in `extra_args` is
/// detected against exactly what [`render_command`] renders, not a
/// hand-maintained shadow list.
///
/// `--host` is deliberately absent here even though [`render_command`] always
/// emits it: it is not sourced from a typed *field* the way the others are -
/// amicu hardcodes it to `127.0.0.1` unconditionally - so an `extra_args`
/// attempt to set it is a denylist violation (`AMICU-CAP-003`, see
/// [`DENYLISTED_FLAGS`]), not a "duplicates a typed field" one. Checking
/// [`is_typed_flag`] before the denylist (see
/// [`crate::adapter::flag_conflict`]) means a flag present in both lists
/// would always be misclassified as `AMICU-CAP-002` instead - `--host` must
/// stay out of this list for that reason.
///
/// Unlike llama.cpp, current stable vLLM `serve` CLI docs (`vllm serve
/// --help`, `docs.vllm.ai/en/stable/cli/serve`) document no short aliases for
/// any of these flags, so every entry here is a long spelling - but the list
/// is flat, not one-entry-per-field: a boolean field rendered as a paired
/// bare-flag toggle (see [`VllmSettings::enforce_eager`]) contributes both
/// its positive and `--no-` spelling as separate entries here, since either
/// one alone in `extra_args` would duplicate what that field renders.
pub(crate) const TYPED_FLAGS: &[&str] = &[
    "--model",
    "--port",
    "--max-model-len",
    "--tensor-parallel-size",
    "--gpu-memory-utilization",
    "--kv-cache-dtype",
    "--max-num-seqs",
    "--max-num-batched-tokens",
    "--enforce-eager",
    "--no-enforce-eager",
];

/// Whether `flag` is the flag any typed field renders (see [`TYPED_FLAGS`]).
#[must_use]
pub(crate) fn is_typed_flag(flag: &str) -> bool {
    TYPED_FLAGS.contains(&flag)
}

/// Flags `extra_args` may never carry, checked by
/// [`crate::adapter::VllmAdapter::validate`]/`plan_launch` (`AMICU-CAP-003`).
///
/// - `--host`: amicu owns the bind address, same reasoning as llama.cpp's
///   `--host` denylist entry (`amicu_llamacpp::command::DENYLISTED_FLAGS`) -
///   `ServiceSupervisor::start` derives the health-check endpoint from the
///   typed `port` alone, so a user-supplied `--host` would desync the two.
/// - `--api-key`: puts secret material directly into argv, visible to any
///   process that can read `/proc/<pid>/cmdline` or a process listing.
/// - `--ssl-keyfile`, `--ssl-certfile`: paths to key material; amicu does not
///   yet manage TLS termination for locally-supervised runtimes.
/// - `--root-path`: changes the mount path the OpenAI-compatible routes are
///   served under, which would desync amicu's own health-check path (`/health`
///   is assumed to be at the server root).
pub(crate) const DENYLISTED_FLAGS: &[&str] = &[
    "--host",
    "--api-key",
    "--ssl-keyfile",
    "--ssl-certfile",
    "--root-path",
];

/// Environment variable names `settings.env` may never carry, checked
/// directly against keys (`AMICU-CAP-003`).
///
/// - `VLLM_API_KEY`: vLLM's own env-var mirror of `--api-key` - same secret
///   material-in-the-clear concern as the `--api-key` flag above.
/// - The dynamic-loader-hook names: identical rationale to llama.cpp's
///   `DENYLISTED_ENV_KEYS` (`amicu_llamacpp::command`) - they let a spawned
///   process load arbitrary code, independent of any flag equivalence.
///
/// Unlike llama.cpp's `LLAMA_ARG_*` convention, vLLM's `VLLM_*` environment
/// variables are not a documented 1:1 mirror of every CLI flag, so there is no
/// analogous `env`-key-to-flag mapping check here - only this fixed denylist,
/// plus the secret-looking-value warning (`amicu_hcl::scan_runtime_env_secrets`,
/// `AMICU-POLICY-006`) shared with llama.cpp.
pub(crate) const DENYLISTED_ENV_KEYS: &[&str] = &[
    "VLLM_API_KEY",
    "LD_PRELOAD",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "LD_LIBRARY_PATH",
];

/// Render the `vllm serve` launch for `model_reference` (a Hugging Face repo
/// id or local path) under `settings`.
///
/// Typed flags are emitted first (`vllm serve <model>`, `--port`, `--host
/// 127.0.0.1`, `--max-model-len`, then any set parallelism/memory/KV-cache/
/// batching/eager-mode field in field-declaration order), then
/// `settings.extra_args` verbatim, in declaration order - it is argv order.
/// Duplicate/denylisted flags are caught as diagnostics before this ever
/// runs (`crate::adapter`); this function itself renders whatever it is
/// given.
///
/// Returns `(argv, text)`: `argv` is the real argument vector (`argv[0]` =
/// `vllm`) that supervisors exec directly, and `text` is a shell-quoted join
/// for display only. Neither carries secrets: vLLM settings are plain launch
/// flags; secrets enter only through the process environment at apply time
/// (`amicu_llamacpp::supervisor::ServiceSupervisor::start`, reused as-is for
/// vLLM).
#[must_use]
pub fn render_command(model_reference: &str, settings: &VllmSettings) -> (Vec<String>, String) {
    let mut argv = vec![
        "vllm".to_owned(),
        "serve".to_owned(),
        model_reference.to_owned(),
        "--port".to_owned(),
        settings.port.to_string(),
        // amicu always binds loopback-only; see DENYLISTED_FLAGS for why
        // `extra_args` cannot override this.
        "--host".to_owned(),
        "127.0.0.1".to_owned(),
        "--max-model-len".to_owned(),
        settings.context_length.to_string(),
    ];
    if let Some(v) = settings.tensor_parallel_size {
        argv.push("--tensor-parallel-size".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.gpu_memory_utilization {
        argv.push("--gpu-memory-utilization".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.kv_cache_dtype {
        argv.push("--kv-cache-dtype".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.max_num_seqs {
        argv.push("--max-num-seqs".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.max_num_batched_tokens {
        argv.push("--max-num-batched-tokens".to_owned());
        argv.push(v.to_string());
    }
    if let Some(v) = settings.enforce_eager {
        argv.push(if v {
            "--enforce-eager".to_owned()
        } else {
            "--no-enforce-eager".to_owned()
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::resource::VllmKvCacheDtype;
    use std::collections::BTreeMap;

    fn base_settings() -> VllmSettings {
        VllmSettings {
            context_length: 8192,
            port: 8000,
            ..Default::default()
        }
    }

    #[test]
    fn renders_base_command() {
        let (argv, text) = render_command("meta-llama/Llama-3.1-8B-Instruct", &base_settings());
        assert_eq!(
            argv,
            vec![
                "vllm",
                "serve",
                "meta-llama/Llama-3.1-8B-Instruct",
                "--port",
                "8000",
                "--host",
                "127.0.0.1",
                "--max-model-len",
                "8192",
            ]
        );
        assert_eq!(
            text,
            "vllm serve meta-llama/Llama-3.1-8B-Instruct --port 8000 --host 127.0.0.1 --max-model-len 8192"
        );
    }

    /// The value token following `flag`'s first occurrence in `argv`.
    fn value_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let i = argv
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag} missing from {argv:?}"));
        &argv[i + 1]
    }

    #[test]
    fn renders_tensor_parallel_size() {
        let settings = VllmSettings {
            tensor_parallel_size: Some(4),
            ..base_settings()
        };
        let (argv, _) = render_command("m", &settings);
        assert_eq!(value_after(&argv, "--tensor-parallel-size"), "4");
    }

    #[test]
    fn renders_gpu_memory_utilization() {
        let settings = VllmSettings {
            gpu_memory_utilization: Some(0.9),
            ..base_settings()
        };
        let (argv, _) = render_command("m", &settings);
        assert_eq!(value_after(&argv, "--gpu-memory-utilization"), "0.9");
    }

    #[test]
    fn renders_kv_cache_dtype() {
        let settings = VllmSettings {
            kv_cache_dtype: Some(VllmKvCacheDtype::Fp8),
            ..base_settings()
        };
        let (argv, _) = render_command("m", &settings);
        assert_eq!(value_after(&argv, "--kv-cache-dtype"), "fp8");
    }

    #[test]
    fn renders_max_num_seqs() {
        let settings = VllmSettings {
            max_num_seqs: Some(256),
            ..base_settings()
        };
        let (argv, _) = render_command("m", &settings);
        assert_eq!(value_after(&argv, "--max-num-seqs"), "256");
    }

    #[test]
    fn renders_max_num_batched_tokens() {
        let settings = VllmSettings {
            max_num_batched_tokens: Some(8192),
            ..base_settings()
        };
        let (argv, _) = render_command("m", &settings);
        assert_eq!(value_after(&argv, "--max-num-batched-tokens"), "8192");
    }

    #[test]
    fn extra_args_appended_verbatim_after_typed_flags() {
        let settings = VllmSettings {
            extra_args: vec!["--disable-log-stats".to_owned()],
            ..base_settings()
        };
        let (argv, _) = render_command("m", &settings);
        assert_eq!(argv.last().map(String::as_str), Some("--disable-log-stats"));
    }

    #[test]
    fn renders_enforce_eager_true() {
        let settings = VllmSettings {
            enforce_eager: Some(true),
            ..base_settings()
        };
        let (argv, _) = render_command("m", &settings);
        assert!(argv.iter().any(|a| a == "--enforce-eager"));
        assert!(!argv.iter().any(|a| a == "--no-enforce-eager"));
    }

    #[test]
    fn renders_enforce_eager_false() {
        let settings = VllmSettings {
            enforce_eager: Some(false),
            ..base_settings()
        };
        let (argv, _) = render_command("m", &settings);
        assert!(argv.iter().any(|a| a == "--no-enforce-eager"));
        assert!(!argv.iter().any(|a| a == "--enforce-eager"));
    }

    #[test]
    fn renders_neither_enforce_eager_flag_when_unset() {
        let settings = base_settings();
        let (argv, _) = render_command("m", &settings);
        assert!(!argv.iter().any(|a| a == "--enforce-eager"));
        assert!(!argv.iter().any(|a| a == "--no-enforce-eager"));
    }

    #[test]
    fn every_typed_flags_entry_is_a_long_flag() {
        for flag in TYPED_FLAGS {
            assert!(flag.starts_with("--"), "{flag} should be a long flag");
        }
    }

    #[test]
    fn is_typed_flag_matches_every_rendered_flag() {
        let settings = VllmSettings {
            tensor_parallel_size: Some(2),
            gpu_memory_utilization: Some(0.9),
            kv_cache_dtype: Some(VllmKvCacheDtype::Auto),
            max_num_seqs: Some(1),
            max_num_batched_tokens: Some(1),
            enforce_eager: Some(true),
            env: BTreeMap::new(),
            ..base_settings()
        };
        let (argv, _) = render_command("m", &settings);
        for token in &argv {
            // `--host` is a documented exception: always rendered, never a
            // typed field - see `TYPED_FLAGS`'s doc comment.
            if token.starts_with("--") && token != "--host" {
                assert!(is_typed_flag(token), "{token} missing from TYPED_FLAGS");
            }
        }
    }

    #[test]
    fn denylisted_flags_are_not_typed_flags() {
        for flag in DENYLISTED_FLAGS {
            assert!(
                !is_typed_flag(flag),
                "{flag} should not double as a typed flag"
            );
        }
    }

    #[test]
    fn shell_quote_escapes_unsafe_tokens() {
        let settings = VllmSettings {
            extra_args: vec!["--served-model-name=my model".to_owned()],
            ..base_settings()
        };
        let (_, text) = render_command("m", &settings);
        assert!(text.contains("'--served-model-name=my model'"));
    }
}
