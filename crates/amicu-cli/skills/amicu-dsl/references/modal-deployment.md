# Deploying to Modal

Modal (modal.com) is a serverless GPU target. A deployment whose hardware
profile has `provider = "modal"` is deployed by `amicu apply` for both
`vllm` and `llama_cpp` runtimes. The model must come from Hugging Face (the
container downloads it). See `deployment-examples.md` ("vLLM on Modal",
"llama.cpp on Modal") for the base examples; this file is the setup and
operations guide.

## Setup

Environment variables (never pass them as flags; never write real values into
HCL, logs or chat):

| Variable | Needed for | Notes |
|---|---|---|
| `MODAL_TOKEN_ID`, `MODAL_TOKEN_SECRET` | Control plane: `apply`, `plan`/`apply` refresh, every `amicu modal` command | A missing one fails with `MODAL_TOKEN_ID is not set` (or `..._SECRET`) before any Modal call. |
| `MODAL_PROXY_TOKEN_ID`, `MODAL_PROXY_TOKEN_SECRET` | Calling the endpoint, and `amicu modal metrics` | Create in the Modal dashboard (Settings -> Proxy Auth Tokens). Sent as the `Modal-Key` / `Modal-Secret` headers, not `Authorization: Bearer`. |
| `HF_TOKEN` (optional) | Gated or private Hugging Face repos | Read from the operator's environment and sent as a Modal Secret, injected only into the running container. Never baked into the image. |
| `AMICU_MODAL_VLLM_IMAGE`, `AMICU_MODAL_LLAMACPP_IMAGE` (optional) | Override the base image | Defaults: `vllm/vllm-openai:v0.29.0`, `ghcr.io/ggml-org/llama.cpp:server-cuda-b11515`. Process-wide; there is no HCL image field. |

Quick check: `amicu modal auth-check`.

Fixed names and constants (not configurable in HCL today):

- **App name**: `amicu-` + the deployment id lower-cased, with every run of
  characters outside `[a-z0-9]` (so `_` too) replaced by a single `-`. Deployment
  `chat_modal` becomes app `amicu-chat-modal`. Use this name with
  `amicu modal status|logs|metrics|stop`.
- **Cache volumes** (shared by every amicu deployment in the Modal workspace):
  `amicu-hf-cache` at `/root/.cache/huggingface` and `amicu-vllm-cache` at
  `/root/.cache/vllm` (vLLM); `amicu-llamacpp-cache` at `/root/.cache/llama.cpp`
  (llama.cpp, via `LLAMA_CACHE`). Weights and compile caches persist across
  cold starts, which is why only the first run of a model is slow and warm
  starts are fast.
- **Lifecycle**: startup timeout 600s, scaledown window 900s (idle time before
  scale-to-zero), container timeout 3600s, target concurrency 32,
  `min_containers = 0`.

## llama.cpp on Modal: full example

A GGUF variant `file` becomes `-hf <source.reference> -hff <file>`. Amicu also
appends `--host 0.0.0.0` (Modal reaches the container over its internal
network), `--no-mmproj` (unless the deployment has a `multimodal {}` block;
`-hf` would otherwise download the repo's projector) and `--metrics` (so
`amicu modal metrics` works). `variant.size` must be an integer byte size
string such as `"8350MiB"`.

```hcl
resource "model" "qwen35_9b_gguf" {
  source {
    registry  = "hugging_face"
    reference = "unsloth/Qwen3.5-9B-GGUF"
  }
  architecture   = "qwen3"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q6_k"
  }
  context_length = 32768
  variant {
    quantization = "q6_k"
    file         = "Qwen3.5-9B-UD-Q6_K_XL.gguf"
    size         = "8350MiB"
  }
}

resource "hardware_profile" "modal_l4" {
  provider = "modal"
  accelerator {
    kind   = "gpu"
    model  = "l4"
    count  = 1
    memory = "24GiB"
  }
  memory = "32GiB"
}

resource "deployment" "qwen35_modal" {
  model    = model.qwen35_9b_gguf.id
  hardware = hardware_profile.modal_l4.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 32768
    port           = 8080
    n_gpu_layers   = -1
  }
}
```

App name: `amicu-qwen35-modal`. Local-file flags (`--mmproj`, `--lora`,
`--chat-template-file`, a `speculative {}` draft model) and secret-bearing
flags (`--hf-token`, `--api-key`) are rejected before any Modal call.

## Multi-GPU (vLLM)

`accelerator.count` becomes Modal's GPU string (`count = 2` on `l4` gives
`L4:2`; a count of 1 gives plain `L4`). Amicu does not derive
`tensor_parallel_size` from the count: set it yourself in the `vllm` block to
match. `amicu plan` labels the hardware `gpu.l4 x2`.

```hcl
resource "model" "qwen35_fp8" {
  source {
    registry  = "hugging_face"
    reference = "RedHatAI/Qwen3.5-9B-FP8-dynamic"
  }
  architecture   = "qwen3"
  modalities     = ["text"]
  artifact {
    format       = "safetensors"
    quantization = "fp8"
  }
  context_length = 262144
}

resource "hardware_profile" "modal_2xl4" {
  provider = "modal"
  accelerator {
    kind   = "gpu"
    model  = "l4"
    count  = 2
    memory = "24GiB"
  }
  memory = "64GiB"
}

resource "deployment" "qwen35_tp2" {
  model    = model.qwen35_fp8.id
  hardware = hardware_profile.modal_2xl4.id
  runtime  = "vllm"
  vllm {
    context_length       = 262144
    port                 = 8000
    tensor_parallel_size = 2
    env {
      # Workaround, see "Known issue" below.
      NCCL_P2P_DISABLE = "1"
    }
  }
}
```

Both examples pass `amicu validate`; the plan launch lines are
`llama-server -hf unsloth/Qwen3.5-9B-GGUF -hff Qwen3.5-9B-UD-Q6_K_XL.gguf ... --metrics --no-mmproj --host 0.0.0.0`
and `vllm serve RedHatAI/Qwen3.5-9B-FP8-dynamic ... --host 0.0.0.0 --tensor-parallel-size 2`.

**Known issue**: L4 and other GPUs without NVLink can fail intermittently on
Modal with `NCCL error: unhandled cuda error` at the first all-reduce.
Workaround: `env { NCCL_P2P_DISABLE = "1" }` in the `vllm` block (plain `env`
values are baked into the image, which is fine for this flag; never put a
secret there), then re-apply.

**Verify both GPUs are used**: `amicu modal logs <app>` shows
`tensor_parallel_size=2`, `Worker_TP0` and `Worker_TP1`, and two
"Model loading took" lines. The Modal dashboard shows the function as
"L4 x 2".

## Metrics

`amicu modal metrics <app> [--watch] [--interval N]` scrapes `/metrics` through
the app's Modal URL (needs the proxy tokens; a scaled-to-zero app is woken by
the request, so the first call can be slow). The runtime is detected from the
series present.

- vLLM (`vllm:`): `request_success_total`, `num_requests_running`,
  `num_requests_waiting`, `prompt_tokens_total`, `generation_tokens_total`,
  `kv_cache_usage_perc` (older `gpu_cache_usage_perc`), `num_preemptions_total`,
  and the histograms `e2e_request_latency_seconds`,
  `time_to_first_token_seconds`, `request_time_per_output_token_seconds`
  (printed as p50/p90/p99).
- llama.cpp (`llamacpp:`): `requests_processing`, `requests_deferred`,
  `prompt_tokens_total`, `tokens_predicted_total`, `predicted_tokens_seconds`,
  `prompt_tokens_seconds`, and `kv_cache_usage_ratio` on older builds only.
  llama-server serves these only with `--metrics`, which amicu always adds.
  Latency percentiles are `n/a` for llama.cpp.

Limits: tokens/s needs two scrapes, so it is `n/a` without `--watch` (the
default `--interval` is 5s). vLLM percentiles are computed from lifetime
histograms, not a recent window. The coarse runtime-metrics view reports
`memory_used` as 0 because runtimes expose KV fill as a ratio, not bytes.

## State and lifecycle

- `amicu plan` and `amicu apply` first refresh against Modal: for each
  Modal deployment recorded in `.amicu/state/state.json` they look up the app,
  and a stopped or missing app is dropped from state so the diff shows a
  create (it is recreated on apply). A `drift: deployment <id>: app <name> not
  found on Modal, will be recreated` line is printed (to stderr with
  `--output json`, so stdout stays one JSON value).
- If the Modal lookup fails (no credentials, network), they print a `warning:`
  and keep the local state as-is.
- `amicu modal stop <app>` run inside the project also removes that deployment
  and its endpoint from local state ("removed deployment <id> from local
  state"), so the next apply recreates it. Outside a project it only stops
  the app. A state problem warns but never fails the stop.
- `apply` keeps the recorded endpoints of unchanged deployments; only
  deployments it touches get new endpoints.
- `apply` streams the image build to stderr as `build: ...` lines (secret
  values redacted). The first build of an image can take several minutes.
- State writes are atomic (temp file then rename).
- Changing `region` on an existing deployment needs `amicu modal stop <app>`
  and a re-apply (Modal does not allow the routing region to change).

## Calling the endpoint

```bash
curl -H "Modal-Key: $MODAL_PROXY_TOKEN_ID" -H "Modal-Secret: $MODAL_PROXY_TOKEN_SECRET" \
  "$URL/v1/models"
```

`$URL` is the endpoint `apply` printed (or `amicu modal status <app>`). Expect
a `503` while the container cold-starts. Thinking models (Qwen3.5 style) spend
`max_tokens` on reasoning: give a generous `max_tokens` and check
`finish_reason`.
