# llama-server argument mapping

Amicu builds the `llama-server` argv from the `llama_cpp {}` block. You do not
pass these by hand; this table explains what each field becomes so you can
reason about behavior and diagnostics.

| HCL field | llama-server arg | Notes |
|---|---|---|
| `variant.file` (from the model) | `-m <path>` | Resolved GGUF file path. |
| `port` | `--port <n>` | Local bind port. |
| `context_length` | `--ctx-size <n>` | KV-cache sizing; ≤ model context. |
| `n_gpu_layers` | `--n-gpu-layers <n>` | `-1` = all layers, `0` = CPU-only. |

## `extra_args` / `env` escape hatch (amicu-planning#150)

Anything past the four typed fields above goes in `extra_args` (raw
`llama-server` flags, appended verbatim after the typed ones, in declaration
order) and `env` (process environment variables). See
`amicu-dsl/references/deployment-examples.md` for a full example
(speculative decoding, vision projector, KV-cache quant, chat template).

Both channels are checked against the same typed-field/denylist tables the
typed fields themselves use, so amicu-owned settings cannot be silently
re-asserted:

- A flag that duplicates a typed field's flag - by any alias llama-server
  accepts for it (`-m`/`--model`, `-c`/`--ctx-size`,
  `-ngl`/`--gpu-layers`/`--n-gpu-layers`, `--port`) - is `AMICU-CAP-002`.
- A flag on the fixed denylist (`--host`, `--api-key`, `--api-key-file`,
  `--ssl-key-file`) is `AMICU-CAP-003`.
- llama-server also reads `LLAMA_ARG_<X>` environment variables as
  equivalent to `--<x>` on the CLI, so an `env` key mirrors these same
  checks: `LLAMA_ARG_CTX_SIZE` is `AMICU-CAP-002`, `LLAMA_ARG_HOST` is
  `AMICU-CAP-003`. A handful of dynamic-loader env vars (`LD_PRELOAD`,
  `DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`, `LD_LIBRARY_PATH`) are
  denylisted outright regardless of the `LLAMA_ARG_` prefix.
- Any non-empty `extra_args` also gets an informational `AMICU-CAP-004`,
  and the plan's cost/performance estimate is marked "unverified: raw
  runtime flags present" - the estimator cannot interpret flags it has no
  typed model for.

## Service supervision

- macOS: a launchd user agent, loaded via `launchctl load -w`.
- Linux: a systemd `--user` unit; status via `systemctl --user is-active`.

`amicu doctor` runs `llama-server --version` to confirm the binary is on PATH.
If it is missing, install llama.cpp (`brew install llama.cpp` on macOS) and
re-run doctor.

## Common issues

- Model fails to load / OOM: lower `n_gpu_layers`, use a smaller quantization,
  or reduce `context_length`.
- Port already in use: change `port` and re-plan.
