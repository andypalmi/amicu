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
| `temperature` | `--temp <n>` | Sampling temperature. |
| `top_p` | `--top-p <n>` | Nucleus sampling cutoff. |
| `top_k` | `--top-k <n>` | Top-k sampling cutoff. |
| `min_p` | `--min-p <n>` | Min-p sampling cutoff. |
| `repeat_penalty` | `--repeat-penalty <n>` | Repetition penalty. |
| `cache_type_k` | `--cache-type-k <type>` | KV-cache dtype for K; llama-server defaults to `f16` when unset. |
| `cache_type_v` | `--cache-type-v <type>` | KV-cache dtype for V; llama-server defaults to `f16` when unset. |
| `batch_size` | `--batch-size <n>` | Logical (prompt-processing) batch size. |
| `ubatch_size` | `--ubatch-size <n>` | Physical (micro-)batch size. |
| `parallel_slots` | `--parallel <n>` | Concurrent request slots. `--ctx-size` is the *total* context shared across slots, not per-slot. |
| `flash_attention` | `--flash-attn <on\|off\|auto>` | Three-way toggle, not a boolean. |
| `chat_template` | `--chat-template <text>` | Inline Jinja chat template. Mutually exclusive with `chat_template_file` (`AMICU-CAP-005` if both are set). |
| `chat_template_file` | `--chat-template-file <path>` | Path to a Jinja chat template file. Mutually exclusive with `chat_template`. |
| `jinja` | `--jinja` | Bare flag (no value); renders only when `true`. No `--no-jinja` exists, so `false`/unset both render nothing. |
| `reasoning_format` | `--reasoning-format <none\|deepseek\|deepseek_legacy\|auto>` | Reasoning/thinking-block extraction format. |
| `build` (model-level: `variant.sha256`) | *(not rendered)* | `build` is a stored/validated-only runtime build pin (amicu-planning#151 PR B); `sha256` verifies the resolved download artifact's checksum in `download_gguf`, before any bytes are fetched - neither appears in argv. |

## Sampling, KV cache, batching, flash-attn, parallel slots (amicu-planning#151)

All twelve fields above are optional and independent - set only what you need,
the rest fall back to llama-server's own defaults. See
`amicu-dsl/references/deployment-examples.md` for a full example.

## Chat template, jinja, reasoning format, build pin, artifact checksum (amicu-planning#151 PR B)

`chat_template`/`chat_template_file`, `jinja`, and `reasoning_format` are
optional and independent of each other and of the PR A fields above -
`chat_template` and `chat_template_file` are the one pair that is *not*
independent: setting both is `AMICU-CAP-005`, and `render_command` emits
neither flag when that happens (`plan_launch` refuses to build the launch at
all; `validate` reports the diagnostic without refusing).

`build` (on `llama_cpp {}`) and `sha256` (on a model's `variant {}`) are
validated at parse time but do not affect argv:

- `build` must be non-empty with no whitespace (e.g. `"b4500"` or a git sha).
  It is stored for future use - amicu does not yet render it into argv or
  check it against the detected `llama-server` binary.
- `sha256` must be exactly 64 hex characters (case-insensitive on input,
  normalized to lowercase on parse). When a deployment's download path calls
  `amicu_llamacpp::download::download_gguf` with `expected_sha256 = Some(&variant.sha256)`,
  it is checked against the resolved `ArtifactRef.checksum` (an optional
  `"sha256:"` prefix is stripped, case-insensitively) *before* any bytes are
  fetched; a mismatch fails fast as `LlamaCppError::ChecksumMismatch`
  (`AMICU-CAP-001`) rather than downloading a file already known to be wrong.

### KV-cache estimator

When the deployment sets `cache_type_k` and/or `cache_type_v`, *and* the
referenced `model` declares all three of `n_layers`, `n_kv_heads`, `head_dim`,
`amicu_registry::fit::kv_cache_bytes` computes:

```
context_length × n_layers × n_kv_heads × head_dim × (bytes_per_element(k) + bytes_per_element(v))
```

one K vector and one V vector per attention head per layer per cached token,
at the configured dtype (`bytes_per_element` is llama.cpp's per-type block
size ÷ 32 elements - `f16`/`bf16` = 2.0, `q8_0` ≈ 1.0625, `q4_0`/`iq4_nl` ≈
0.5625, `q4_1` ≈ 0.625, `q5_0` ≈ 0.6875, `q5_1` ≈ 0.75). This does **not**
multiply by `parallel_slots`: `--ctx-size` is the total context shared across
slots, not sized per slot, so the KV pool stays `context_length`-sized
regardless of how many slots split it.

The result appears as `FitReport.kv_cache`, feeds the plan's
`estimated_memory` (in place of the flat overhead band, which is otherwise
the only memory-beyond-weights figure), and shows up in the plan's
`cost_performance.assumptions` as a line like
`"KV cache: 4GiB at q8_0 for 32768 ctx"`. Leave the model's shape fields
unset (there is no GGUF header parser in-tree to derive them) to keep the
flat overhead band - nothing else about the fit check changes.

## `extra_args` / `env` escape hatch (amicu-planning#150)

Anything past the typed fields above goes in `extra_args` (raw
`llama-server` flags, appended verbatim after the typed ones, in declaration
order) and `env` (process environment variables). See
`amicu-dsl/references/deployment-examples.md` for a full example
(speculative decoding, vision projector, chat template - still untyped as of
amicu-planning#151's PR A).

Both channels are checked against the same typed-field/denylist tables the
typed fields themselves use, so amicu-owned settings cannot be silently
re-asserted:

- A flag that duplicates a typed field's flag - by any alias llama-server
  accepts for it (`-m`/`--model`, `-c`/`--ctx-size`,
  `-ngl`/`--gpu-layers`/`--n-gpu-layers`, `--port`, `--temp`/`--temperature`,
  `--top-p`, `--top-k`, `--min-p`, `--repeat-penalty`,
  `-ctk`/`--cache-type-k`, `-ctv`/`--cache-type-v`, `-b`/`--batch-size`,
  `-ub`/`--ubatch-size`, `-np`/`--parallel`, `-fa`/`--flash-attn`,
  `--chat-template`, `--chat-template-file`, `--jinja`,
  `--reasoning-format`) - is `AMICU-CAP-002`.
- A flag on the fixed denylist (`--host`, `--api-key`, `--api-key-file`,
  `--ssl-key-file`) is `AMICU-CAP-003`.
- llama-server also reads `LLAMA_ARG_<X>` environment variables as
  equivalent to `--<x>` on the CLI, so an `env` key mirrors these same
  checks: `LLAMA_ARG_CTX_SIZE` is `AMICU-CAP-002`, `LLAMA_ARG_HOST` is
  `AMICU-CAP-003`. Most of the new fields follow that same mechanical
  `LLAMA_ARG_<X>` -> `--<x>` rule (e.g. `LLAMA_ARG_TEMP` -> `--temp`,
  `LLAMA_ARG_CACHE_TYPE_K` -> `--cache-type-k`, `LLAMA_ARG_CHAT_TEMPLATE` ->
  `--chat-template`, `LLAMA_ARG_JINJA` -> `--jinja`,
  `LLAMA_ARG_REASONING_FORMAT` -> `--reasoning-format`), but three don't:
  `LLAMA_ARG_BATCH` mirrors `--batch-size`, `LLAMA_ARG_UBATCH` mirrors
  `--ubatch-size`, and `LLAMA_ARG_N_PARALLEL` mirrors `--parallel` - amicu
  special-cases these three so `AMICU-CAP-002` still catches them. A
  handful of dynamic-loader env vars (`LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`,
  `DYLD_LIBRARY_PATH`, `LD_LIBRARY_PATH`) are denylisted outright regardless
  of the `LLAMA_ARG_` prefix.
- Any non-empty `extra_args` also gets an informational `AMICU-CAP-004`,
  and the plan's cost/performance estimate is marked "unverified: raw
  runtime flags present" - the estimator cannot interpret flags it has no
  typed model for.
- Setting both `chat_template` and `chat_template_file` is `AMICU-CAP-005`
  (amicu-planning#151 PR B) - see above.

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
