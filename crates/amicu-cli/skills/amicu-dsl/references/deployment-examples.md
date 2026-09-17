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
  }
}
```

`tensor_parallel_size` should match the hardware profile's accelerator `count`.
`gpu_memory_utilization` is a fraction in `(0, 1]`.

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
    extra_args = [
      "--mmproj", "/models/vision/mmproj-f16.gguf",
      "--model-draft", "/models/vision/draft-q4.gguf",
      "--spec-type", "draft-dflash,ngram-map-k4v",
      "--cache-type-k", "q8_0",
      "--flash-attn", "on",
      "--jinja", "--chat-template-file", "/models/vision/chat.jinja",
      "--temp", "0.6",
    ]
    env {
      GGML_CUDA_DISABLE_GRAPHS = "1"
    }
  }
}
```

Rules and caveats:
- A flag in `extra_args` that duplicates a typed field's flag (`--port`,
  `--ctx-size`, `--n-gpu-layers`, in either `--foo value` or `--foo=value`
  form) is rejected with `AMICU-CAP-002` - fix the ambiguity by removing one
  side, not by hoping the last one wins.
- A denylisted flag (`--host`, `--api-key`, `--api-key-file`,
  `--ssl-key-file`) is rejected with `AMICU-CAP-003`: amicu owns the bind
  address, and the others put secret material - or a path to it - directly
  into argv, which any process listing can read. Put real secrets in `env`
  (or better, a `credential`'s secret-designated `env`), not `extra_args`.
- Any non-empty `extra_args` gets an informational `AMICU-CAP-004`, and the
  plan's cost/performance estimate is marked
  `"unverified: raw runtime flags present"` - the estimator cannot interpret
  flags it doesn't have a typed model for (KV-cache dtype, speculative
  decoding, ...).
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
