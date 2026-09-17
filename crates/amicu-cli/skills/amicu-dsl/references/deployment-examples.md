# Deployment examples

## llama.cpp deployment (local)

```hcl
resource "deployment" "support_local" {
  model    = model.support_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}
```

`n_gpu_layers = -1` offloads all layers to the accelerator; `0` is CPU-only.

## llama.cpp: sampling, KV cache, batching, flash attention, parallel slots (amicu-planning#151)

```hcl
resource "deployment" "support_local_tuned" {
  model    = model.support_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length  = 32768
    port            = 8000
    n_gpu_layers    = -1

    # Sampling defaults
    temperature     = 0.6
    top_p           = 0.9
    top_k           = 40
    min_p           = 0.05
    repeat_penalty  = 1.1

    # KV cache dtype (independent per tensor; unset defaults to llama-server's
    # own `f16` default for that tensor)
    cache_type_k    = "q8_0"
    cache_type_v    = "q8_0"

    # Batching and concurrency
    batch_size      = 2048
    ubatch_size     = 512
    parallel_slots  = 4

    # "on" | "off" | "auto" - not a boolean
    flash_attention = "auto"
  }
}
```

All of the above are optional and independent - set only the ones you care
about. `cache_type_k`/`cache_type_v` accept `f16`, `bf16`, `q8_0`, `q4_0`,
`q4_1`, `q5_0`, `q5_1`, `iq4_nl` (see `resource-types.md`). Setting either one
lets the KV-cache estimator run (see `llama-server-args.md`) provided the
referenced `model` also declares `n_layers`/`n_kv_heads`/`head_dim`:

```hcl
resource "model" "support_chat" {
  # ... source, architecture, artifact, variant ...
  n_layers   = 32
  n_kv_heads = 8
  head_dim   = 128
}
```

These three fields are declared, not derived - amicu has no GGUF header
parser - so leave them unset if you don't know them; the estimator falls back
to the flat overhead band exactly as before.

## vLLM deployment (production)

```hcl
resource "deployment" "chat_prod" {
  model    = model.chat_prod.id
  hardware = hardware_profile.eu_a100.id
  runtime  = "vllm"
  vllm {
    context_length         = 32768
    port                   = 8000
    tensor_parallel_size   = 2
    gpu_memory_utilization = 0.90

    # amicu-planning#151: vLLM's own KV-cache-dtype and batching knobs.
    # No vLLM renderer exists yet - these parse and round-trip but nothing
    # consumes them until the renderer lands.
    kv_cache_dtype           = "fp8"
    max_num_seqs             = 4
    max_num_batched_tokens   = 8192
  }
}
```

`tensor_parallel_size` should match the hardware profile's accelerator `count`.
`gpu_memory_utilization` is a fraction in `(0, 1]`. `kv_cache_dtype` accepts
`auto`, `fp8`, `fp8_e5m2`, `fp8_e4m3` - vLLM's own value set, distinct from
llama.cpp's `cache_type_k`/`cache_type_v`. `max_num_seqs` is vLLM's rough
equivalent of llama.cpp's `parallel_slots`; `max_num_batched_tokens` of
`batch_size`. vLLM has no sampling-defaults deployment flags, so there is no
`temperature`/`top_p`/... equivalent on `vllm {}`.

## llama.cpp escape hatch: `extra_args` + `env` (amicu-planning#150)

Typed fields cover a small slice of `llama-server`'s flag surface. `extra_args`
passes raw flags straight through to the renderer, appended verbatim after the
typed flags in declaration order - it *is* argv order. `env` sets process
environment variables at launch, merged with (and never overridden by)
resolved secrets.

```hcl
resource "deployment" "vision_spec_local" {
  model    = model.vision_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 32768
    port           = 8000
    n_gpu_layers   = -1
    # Sampling, KV-cache dtype, batching, flash-attn and parallel slots are
    # typed fields (see above) - put them there, not here. What remains
    # untyped after amicu-planning#151's PR A (speculative decoding,
    # multimodal projector, chat template/pins) still goes through
    # extra_args, tracked as separate PRs against the same issue.
    extra_args = [
      "--mmproj", "/models/vision/mmproj-f16.gguf",
      "--model-draft", "/models/vision/draft-q4.gguf",
      "--spec-type", "draft-dflash,ngram-map-k4v",
      "--jinja", "--chat-template-file", "/models/vision/chat.jinja",
    ]
    env {
      GGML_CUDA_DISABLE_GRAPHS = "1"
    }
  }
}
```

Rules and caveats:
- A flag in `extra_args` that duplicates a typed field's flag (`--port`,
  `--ctx-size`, `--n-gpu-layers`, `--temp`/`--temperature`, `--top-p`,
  `--top-k`, `--min-p`, `--repeat-penalty`, `--cache-type-k`/`-ctk`,
  `--cache-type-v`/`-ctv`, `--batch-size`/`-b`, `--ubatch-size`/`-ub`,
  `--parallel`/`-np`, `--flash-attn`/`-fa`, in either `--foo value` or
  `--foo=value` form) is rejected with `AMICU-CAP-002` - fix the ambiguity by
  removing one side, not by hoping the last one wins. The same check applies
  to an `env` entry whose key is `LLAMA_ARG_<X>` and mirrors one of those
  flags (e.g. `LLAMA_ARG_TEMP`, `LLAMA_ARG_CACHE_TYPE_K`).
- A denylisted flag (`--host`, `--api-key`, `--api-key-file`,
  `--ssl-key-file`) is rejected with `AMICU-CAP-003`: amicu owns the bind
  address, and the others put secret material - or a path to it - directly
  into argv, which any process listing can read. Put real secrets in `env`
  (or better, a `credential`'s secret-designated `env`), not `extra_args`.
- Any non-empty `extra_args` gets an informational `AMICU-CAP-004`, and the
  plan's cost/performance estimate is marked
  `"unverified: raw runtime flags present"` - the estimator cannot interpret
  flags it doesn't have a typed model for (speculative decoding, multimodal
  projector, chat template, ...).
- An `env` value that looks like a real secret (an AWS key, an OpenAI-style
  key, a PEM header, a JWT, or a long high-entropy token) gets a
  `AMICU-POLICY-006` warning nudging it toward a `credential` resource
  instead; this is a heuristic, not a hard rejection like `credential.env`'s
  `AMICU-POLICY-004`.
- `extra_args`/`env` fields on the `vllm {}` block parse the same way, ahead
  of the vLLM command renderer that will consume them.

Notes:
- The settings block must match the `runtime`: `llama_cpp {}` for `llama_cpp`,
  `vllm {}` for `vllm`.
- `model` and `hardware` must reference ids that exist, or validation reports
  `AMICU-REF-001`.
