# vLLM engine args

Amicu builds the `vllm serve` argv from the `vllm {}` block
(amicu-planning#152 - `amicu-vllm`'s `render_command`, mirroring
`amicu-llamacpp`'s `render_command`). You do not pass these by hand; this
table explains what each field becomes so you can reason about behavior and
diagnostics.

| HCL field | vLLM arg | Notes |
|---|---|---|
| model source/artifact | `vllm serve <ref>` | Hugging Face reference or local path, positional (not a flag). |
| `port` | `--port <n>` | Server bind port. |
| — | `--host 127.0.0.1` | Always rendered; amicu owns the bind address (see denylist below). |
| `context_length` | `--max-model-len <n>` | Upper bound on sequence length. |
| `tensor_parallel_size` | `--tensor-parallel-size <n>` | GPUs per replica; defaults to `1` (vLLM's own default) when unset. |
| `gpu_memory_utilization` | `--gpu-memory-utilization <f>` | Fraction of VRAM, `(0, 1]`; defaults to `0.92` (vLLM's own default) when unset. |
| `kv_cache_dtype` | `--kv-cache-dtype <auto\|fp8\|fp8_e5m2\|fp8_e4m3>` | KV-cache quantization; vLLM's own value set (distinct from llama.cpp's `cache_type_k`/`cache_type_v`). |
| `max_num_seqs` | `--max-num-seqs <n>` | Maximum concurrent sequences; vLLM's rough equivalent of llama.cpp's `parallel_slots`. |
| `max_num_batched_tokens` | `--max-num-batched-tokens <n>` | Maximum batched tokens per iteration; vLLM's rough equivalent of llama.cpp's `batch_size`. |

No typed `dtype` (model weight dtype) field exists yet in `VllmSettings` - use
`extra_args` (`--dtype bfloat16`, etc.) until it is promoted to a typed field.

## GGUF on vLLM (amicu-planning#152)

vLLM can load a single-file GGUF checkpoint, but the support is experimental
upstream, unlike llama.cpp where GGUF is the native format. A deployment whose
model artifact is GGUF but whose runtime is `vllm` gets a non-blocking
`AMICU-CAP-006` warning at `validate`/plan time - the deployment still plans
and applies, the warning is informational. (Numbered 006, not 005: `AMICU-
CAP-005` is a llama.cpp chat-template conflict registered by
amicu-planning#151's typed-fields PR.)

## `extra_args` / `env` escape hatch (amicu-planning#150, extended to vLLM by #152)

Anything past the typed fields above goes in `extra_args` (raw `vllm serve`
flags, appended verbatim after the typed ones, in declaration order) and
`env` (process environment variables), exactly like the llama.cpp `extra_args`/
`env` escape hatch documented in `amicu-local/references/llama-server-args.md`.

- A flag that duplicates a typed field's flag (`--port`, `--max-model-len`,
  `--tensor-parallel-size`, `--gpu-memory-utilization`, `--kv-cache-dtype`,
  `--max-num-seqs`, `--max-num-batched-tokens`, or the model itself) is
  `AMICU-CAP-002`. Current stable vLLM `serve` CLI docs document no short
  aliases for any of these, unlike llama.cpp.
- A flag on the fixed denylist - `--host` (amicu owns the bind address),
  `--api-key` (puts secret material directly into argv), `--ssl-keyfile`/
  `--ssl-certfile` (amicu does not manage TLS termination for locally
  supervised runtimes), `--root-path` (would desync amicu's own `/health`
  check path) - is `AMICU-CAP-003`.
- Unlike llama.cpp's `LLAMA_ARG_*` convention (an env var that mirrors any CLI
  flag), vLLM's `VLLM_*` environment variables are **not** a documented 1:1
  flag mirror, so `env` only gets a fixed denylist check, not a flag-mapping
  one: `VLLM_API_KEY` (mirrors `--api-key`) and the same dynamic-loader-hook
  names llama.cpp denylists (`LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`,
  `DYLD_LIBRARY_PATH`, `LD_LIBRARY_PATH`) are `AMICU-CAP-003`.
- Any non-empty `extra_args` also gets an informational `AMICU-CAP-004`, and
  the plan's cost/performance estimate is marked "unverified: raw runtime
  flags present".
- An `env` value that looks like a secret (AWS access key, OpenAI-style key,
  PEM header, JWT, high-entropy token) gets `AMICU-POLICY-006`, the same
  heuristic llama.cpp's `env` shares - this check was already runtime-agnostic
  before #152's dispatch fix, since it runs at `report_for`'s graph level, not
  inside a runtime adapter.

## Service supervision

vLLM local launches reuse the exact same `ServiceSupervisor` llama.cpp uses
(launchd on macOS, systemd `--user` on Linux) - see
`amicu-local/references/llama-server-args.md`'s "Service supervision" section.
Health is checked against `GET /health` on the configured port.

## Sizing

- FP16/BF16 weights: ~`2 × params` bytes (an 8B model ≈ 16 GiB) before
  KV-cache. KV-cache grows with `context_length` and concurrency and consumes
  the remainder governed by `gpu_memory_utilization`.
- If weights + KV-cache exceed a single GPU, raise `tensor_parallel_size` and
  provision more accelerators in the hardware profile.
- `amicu validate`/`amicu plan` now actually check this (amicu-planning#152):
  weights (dtype-based; the model's `artifact.quantization`, defaulting to
  `bf16` when unset) plus a KV-cache estimate (when the model declares
  `n_layers`/`n_kv_heads`/`head_dim`) against
  `gpu_memory_utilization × per-GPU memory × tensor_parallel_size`. Unlike
  llama.cpp's fit check, there is no separate flat overhead band on top -
  `gpu_memory_utilization` already *is* vLLM's own usable-fraction knob - so
  the plan's `cost_performance.assumptions` always carries "vllm fit
  estimate: weights + kv cache only" as a reminder that this is a first-order
  estimate, not activation/fragmentation-aware.

## Throughput tips

- vLLM's continuous batching handles concurrency; you do not size per-request.
- Prefer higher `gpu_memory_utilization` (≤ 0.95) on dedicated GPUs for a larger
  KV-cache and better throughput.
- Quantized safetensors (e.g. AWQ/GPTQ) reduce VRAM at some quality cost; see
  amicu-model-selection for the trade-off.

## Testing a Modal deployment (amicu-planning#158)

Modal (modal.com) is a no-Python-on-the-client GPU rental target reached over
gRPC, not an `amicu apply` runtime yet (HCL wiring is a separate,
not-yet-merged PR) - today it is exercised entirely through the
project-independent `amicu modal` commands, which read `MODAL_TOKEN_ID`/
`MODAL_TOKEN_SECRET` from the environment and never accept them as flags:

```bash
amicu modal auth-check              # confirms the token is accepted (ClientHello)
amicu modal status <app-name>       # app state + each Function's url/running/backlog
amicu modal logs <app-name>         # prints currently available logs, then returns
amicu modal logs <app-name> --follow --since <entry-id>   # tails from a resume point
amicu modal stop <app-name>         # AppStop; terminates the app's containers
```

A typical check-in-on-a-deploy loop: `auth-check` first if anything looks
wrong, then `status` to see whether the Function is up and what URL it
published, then `logs` (add `--follow` while watching a cold start), then
`stop` once you are done - `status` right after `stop` reports no deployment
by that name (Modal keeps the app itself in a `"stopped"` state internally,
but its deployment *name* no longer resolves to an id, which is what
`status` surfaces). `logs` without `--follow` is a single poll: useful in a
script or right after `status` shows a Function running, not a substitute for
`--follow` if the app has not produced output yet.
