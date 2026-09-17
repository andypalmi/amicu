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

## Sampling, KV cache, batching, flash-attn, parallel slots (amicu-planning#151)

All twelve fields above are optional and independent - set only what you need,
the rest fall back to llama-server's own defaults. See
`amicu-dsl/references/deployment-examples.md` for a full example.

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
  `-ub`/`--ubatch-size`, `-np`/`--parallel`, `-fa`/`--flash-attn`) - is
  `AMICU-CAP-002`.
- A flag on the fixed denylist (`--host`, `--api-key`, `--api-key-file`,
  `--ssl-key-file`) is `AMICU-CAP-003`.
- llama-server also reads `LLAMA_ARG_<X>` environment variables as
  equivalent to `--<x>` on the CLI, so an `env` key mirrors these same
  checks: `LLAMA_ARG_CTX_SIZE` is `AMICU-CAP-002`, `LLAMA_ARG_HOST` is
  `AMICU-CAP-003`. Most of the new fields follow that same mechanical
  `LLAMA_ARG_<X>` -> `--<x>` rule (e.g. `LLAMA_ARG_TEMP` -> `--temp`,
  `LLAMA_ARG_CACHE_TYPE_K` -> `--cache-type-k`), but three don't:
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
