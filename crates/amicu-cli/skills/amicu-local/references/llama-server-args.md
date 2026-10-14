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
| `jinja` | `--jinja` (`true`) / `--no-jinja` (`false`) | Paired bare flags (no value); `Some(false)` renders `--no-jinja` explicitly (overrides an inherited `LLAMA_ARG_JINJA=1`), `None` renders nothing. |
| `reasoning_format` | `--reasoning-format <none\|deepseek\|deepseek-legacy\|auto>` | Reasoning/thinking-block extraction format. HCL value `deepseek_legacy` renders as the hyphenated `deepseek-legacy` on argv - see below. |
| `speculative.model` | `--model-draft <path>` | Resolved GGUF path of the *draft model resource* (a real graph edge), staged under its own model id exactly like the primary model's. |
| `speculative.n_max` | `--spec-draft-n-max <n>` | Max tokens drafted per step. |
| `speculative.n_min` | `--spec-draft-n-min <n>` | Min draft tokens to use. |
| `speculative.p_min` | `--draft-p-min <n>` | Min draft probability, below which drafting stops. |
| `speculative.n_gpu_layers` | `--n-gpu-layers-draft <n>` | Draft-model layers offloaded to the accelerator. |
| `build` (model-level: `variant.sha256`) | *(not rendered)* | `build` is a stored/validated-only runtime build pin; `sha256` verifies the resolved download artifact's checksum in `download_gguf`, before any bytes are fetched - neither appears in argv. |
| `multimodal.projector` | `--mmproj <value>`/`-mm` | Path or HF-style reference to the mmproj GGUF. A plain string, not a second `model` resource reference; not validated for existence at plan time. |
| `multimodal.offload` | `--mmproj-offload` (`true`) / `--no-mmproj-offload` (`false`) | Paired bare flags (no value), like `jinja`; llama-server defaults to enabled. `Some(false)` renders `--no-mmproj-offload` explicitly, `None` renders nothing. |

## Sampling, KV cache, batching, flash-attn, parallel slots

All twelve fields above are optional and independent - set only what you need,
the rest fall back to llama-server's own defaults. See
`amicu-dsl/references/deployment-examples.md` for a full example.

## Chat template, jinja, reasoning format, build pin, artifact checksum

`chat_template`/`chat_template_file`, `jinja`, and `reasoning_format` are
optional and independent of each other and of the PR A fields above -
`chat_template` and `chat_template_file` are the one pair that is *not*
independent: setting both is `AMICU-CAP-005`, and `render_command` emits
neither flag when that happens (`plan_launch` refuses to build the launch at
all; `validate` reports the diagnostic without refusing).

`reasoning_format`'s HCL/schema value `deepseek_legacy` renders as the
hyphenated argv token `deepseek-legacy` - `common/chat.cpp`'s
`common_reasoning_format_name` returns the hyphenated spelling, and
`llama-server` rejects the underscored one with "Unknown reasoning format".
The other three values (`none`, `deepseek`, `auto`) are spelled identically
in HCL and argv.

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

## Multimodal projector

`multimodal { projector, offload }` backs the `Modality::Vision` capability
the llama.cpp adapter already declares (`LlamaCppAdapter::capabilities`).
Both fields are optional and independent. `projector` is always rendered as
`--mmproj <value>` regardless of whether it looks like a local path or an
HF-style reference - amicu does not distinguish the two in this PR (no
`--mmproj-url` handling; that would need a separate field if added later).
Spellings verified against `tools/server/README.md`/`common/arg.cpp` via
context7, not guessed - upstream also exposes `-mmu`/`--mmproj-url`,
`--mmproj-auto`/`--no-mmproj`/`--no-mmproj-auto`, and
`-mmdev`/`--mmproj-device`, none of which amicu types in this PR; they
remain available through `extra_args`.

`multimodal {}` is llama.cpp-only: it is only parsed inside a `llama_cpp {}`
settings block. A `multimodal` block inside `vllm {}` is a structural schema
error at parse time (`amicu_hcl::de`) rather than a capability diagnostic -
`VllmSettings` carries no field for it, so nothing would otherwise read (and
therefore validate) a block placed there.

The plan's `cost_performance.assumptions` gets
`"multimodal: projector weights not estimated"` whenever the block is
present: the estimator only sizes the main model's GGUF variant, never the
projector's, so a multimodal deployment's `estimated_memory` is a strict
undercount relative to what llama-server will actually allocate.

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

## Speculative decoding

`speculative {}` is an optional nested block inside `llama_cpp {}`. Omitting it
is exactly today's behavior: no draft model is resolved, no draft flag is
rendered, and the fit check counts only the primary model's weights.

```hcl
llama_cpp {
  context_length = 8192
  port           = 8080
  n_gpu_layers   = -1

  speculative {
    model        = model.qwen3_0_6b_draft.id   # a second `model` resource
    n_max        = 8
    n_min        = 2
    p_min        = 0.75
    n_gpu_layers = 99
  }
}
```

`model` is the only required attribute; the four draft parameters are
independently optional.

### The draft model is a graph edge, not a path

`speculative.model` points at a second `resource "model"` block, so it is
checked and resolved exactly like the deployment's own `model` attribute:

- a reference to an id no `model` declares is `AMICU-REF-001` (reported by
  `amicu validate` through `Graph::missing_references`, and by the llama.cpp
  adapter as "model '<id>' was not found in the catalog");
- the draft model's GGUF variant is selected by the same `GgufResolver` and
  staged at `models/<draft model id>/<variant file>` - that resolved path is
  what `--model-draft` receives, never the HCL reference;
- a non-GGUF draft artifact, or a draft model declaring no GGUF variant, is
  `AMICU-CAP-001`, the same as for the primary model;
- naming the deployment's **own** model as the draft is **`AMICU-CAP-007`**
  (Error). Speculative decoding is only worth anything with a smaller,
  cheaper draft model: drafting with the target model would load the same
  weights twice for no speedup, and the fit check would count them twice.

### Fit

The draft model's weights are added to the fit check's weights total
(`amicu_registry::fit::check_variant_fit_with_draft`), so a deployment that
fits on its own can correctly stop fitting once a draft model is declared. The
plan records this as the assumption:

```
speculative: draft model weights included, draft KV cache not estimated
```

The draft model's *KV cache* is deliberately not estimated: it needs the draft
model's own layer/head shape, which the registry does not generally carry.

### Upstream flag spellings

Verified against `common/arg.cpp` and `common/common.h` on
`ggml-org/llama.cpp@master`. llama.cpp renamed this family to a
`--spec-draft-*` prefix and kept the older spellings as aliases on the same
`add_opt` - **except** `--draft`/`--draft-n`/`--draft-max` and
`--draft-min`/`--draft-n-min`, which were *removed* and now abort with "the
argument has been removed. use --spec-draft-n-max ...". amicu therefore renders,
per field, the spelling the widest range of `llama-server` builds accepts,
preferring the current one wherever the legacy spelling would hard-error:

| field | rendered | aliases llama-server also accepts | env var |
|---|---|---|---|
| `model` | `--model-draft` | `--spec-draft-model`, `-md` | `LLAMA_ARG_SPEC_DRAFT_MODEL` |
| `n_max` | `--spec-draft-n-max` | *(legacy `--draft`/`--draft-n`/`--draft-max` removed)* | `LLAMA_ARG_SPEC_DRAFT_N_MAX` |
| `n_min` | `--spec-draft-n-min` | *(legacy `--draft-min`/`--draft-n-min` removed)* | `LLAMA_ARG_SPEC_DRAFT_N_MIN` |
| `p_min` | `--draft-p-min` | `--spec-draft-p-min` | `LLAMA_ARG_SPEC_DRAFT_P_MIN` |
| `n_gpu_layers` | `--n-gpu-layers-draft` | `--spec-draft-ngl`, `--gpu-layers-draft`, `-ngld` | `LLAMA_ARG_N_GPU_LAYERS_DRAFT` |

Two deliberate omissions:

- **No draft context-size field.** Current upstream exposes no draft
  context-size flag at all - `common_params_speculative_draft`
  (`common/common.h`) has no `n_ctx` member, and `common/arg.cpp` registers no
  `--ctx-size-draft`/`-cd`/`--spec-draft-c`. A typed field for it could only
  render a flag `llama-server` rejects.
- **`--spec-type` is never rendered.** With the type left at its default,
  `common/arg.cpp` infers the speculative type from the draft GGUF's own
  metadata whenever a draft path is set, so passing the draft model alone is
  how draft-model speculative decoding is turned on.

## `extra_args` / `env` escape hatch

Anything past the typed fields above goes in `extra_args` (raw
`llama-server` flags, appended verbatim after the typed ones, in declaration
order) and `env` (process environment variables). See
`amicu-dsl/references/deployment-examples.md` for a full example (the
speculative type/ngram knobs remain untyped).

Both channels are checked against the same typed-field/denylist tables the
typed fields themselves use, so amicu-owned settings cannot be silently
re-asserted:

- A flag that duplicates a typed field's flag - by any alias llama-server
  accepts for it (`-m`/`--model`, `-c`/`--ctx-size`,
  `-ngl`/`--gpu-layers`/`--n-gpu-layers`, `--port`, `--temp`/`--temperature`,
  `--top-p`, `--top-k`, `--min-p`, `--repeat-penalty`,
  `-ctk`/`--cache-type-k`, `-ctv`/`--cache-type-v`, `-b`/`--batch-size`,
  `-ub`/`--ubatch-size`, `-np`/`--parallel`, `-fa`/`--flash-attn`,
  `--chat-template`, `--chat-template-file`, `--jinja`/`--no-jinja`,
  `--reasoning-format`, `-mm`/`--mmproj`,
  `--mmproj-offload`/`--no-mmproj-offload`, and every speculative spelling:
  `--model-draft`/`--spec-draft-model`/`-md`,
  `--spec-draft-n-max`/`--draft-max`/`--draft-n`/`--draft`,
  `--spec-draft-n-min`/`--draft-min`/`--draft-n-min`,
  `--draft-p-min`/`--spec-draft-p-min`,
  `--n-gpu-layers-draft`/`--spec-draft-ngl`/`--gpu-layers-draft`/`-ngld`) - is
  `AMICU-CAP-002`. The removed `--draft-*` spellings are listed too: they still
  name a setting amicu owns, and reporting the conflict beats letting
  `llama-server` abort at launch.
- A flag on the fixed denylist (`--host`, `--api-key`, `--api-key-file`,
  `--ssl-key-file`) is `AMICU-CAP-003`.
- llama-server also reads `LLAMA_ARG_<X>` environment variables as
  equivalent to `--<x>` on the CLI, so an `env` key mirrors these same
  checks: `LLAMA_ARG_CTX_SIZE` is `AMICU-CAP-002`, `LLAMA_ARG_HOST` is
  `AMICU-CAP-003`. Most of the new fields follow that same mechanical
  `LLAMA_ARG_<X>` -> `--<x>` rule (e.g. `LLAMA_ARG_TEMP` -> `--temp`,
  `LLAMA_ARG_CACHE_TYPE_K` -> `--cache-type-k`, `LLAMA_ARG_CHAT_TEMPLATE` ->
  `--chat-template`, `LLAMA_ARG_JINJA` -> `--jinja`, `LLAMA_ARG_MMPROJ` ->
  `--mmproj`, `LLAMA_ARG_MMPROJ_OFFLOAD` -> `--mmproj-offload`), but four
  don't: `LLAMA_ARG_BATCH` mirrors `--batch-size`, `LLAMA_ARG_UBATCH` mirrors
  `--ubatch-size`, `LLAMA_ARG_N_PARALLEL` mirrors `--parallel`, and
  `LLAMA_ARG_THINK` mirrors `--reasoning-format` (`common/arg.cpp`'s
  `--reasoning-format` `add_opt` calls `.set_env("LLAMA_ARG_THINK")` -
  named after the internal `params.think` field, not the flag; there is no
  `LLAMA_ARG_REASONING_FORMAT`) - amicu special-cases these four so
  `AMICU-CAP-002` still catches them. The five speculative env vars
  (`LLAMA_ARG_SPEC_DRAFT_MODEL`, `LLAMA_ARG_SPEC_DRAFT_N_MAX`,
  `LLAMA_ARG_SPEC_DRAFT_N_MIN`, `LLAMA_ARG_SPEC_DRAFT_P_MIN`,
  `LLAMA_ARG_N_GPU_LAYERS_DRAFT`), plus the two llama-server still registers on
  its removed draft args (`LLAMA_ARG_DRAFT_MAX`, `LLAMA_ARG_DRAFT_MIN`), need
  no special case: each mechanically derives a spelling already listed above.
  A handful of dynamic-loader env vars
  (`LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`,
  `LD_LIBRARY_PATH`) are denylisted outright regardless of the `LLAMA_ARG_`
  prefix.
- Any non-empty `extra_args` also gets an informational `AMICU-CAP-004`,
  and the plan's cost/performance estimate is marked "unverified: raw
  runtime flags present" - the estimator cannot interpret flags it has no
  typed model for.
- Setting both `chat_template` and `chat_template_file` is `AMICU-CAP-005` - see above.
- A `speculative {}` block whose draft `model` is the deployment's own model is
  `AMICU-CAP-007` - see above.
- A `multimodal { ... }` block on the `vllm {}` settings block is a
  structural schema error at parse time, not a
  capability diagnostic - see the "Multimodal projector" section above.

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
