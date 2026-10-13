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

## llama.cpp: chat template, jinja, reasoning format, build pin, artifact checksum (amicu-planning#151 PR B)

```hcl
resource "model" "support_chat" {
  # ... source, architecture, artifact ...
  variant {
    quantization = "q4_k_m"
    file         = "support-chat.Q4_K_M.gguf"
    # Verified against the resolved download artifact's checksum before any
    # bytes are fetched - a mismatch fails fast, not after the download.
    sha256       = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a1"
  }
}

resource "deployment" "support_local_chat" {
  model    = model.support_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 32768
    port            = 8000
    n_gpu_layers    = -1

    # Exactly one of chat_template / chat_template_file - setting both is
    # AMICU-CAP-005 and the launch is refused.
    chat_template_file = "/models/support_chat/chat.jinja"

    # Enable Jinja chat-template rendering. A paired bare-flag toggle:
    # true -> --jinja, false -> --no-jinja (explicitly overrides an
    # inherited LLAMA_ARG_JINJA=1), unset -> renders nothing.
    jinja = true

    # How reasoning/thinking blocks are extracted: "none" | "deepseek" |
    # "deepseek_legacy" | "auto". Renders as --reasoning-format on argv;
    # "deepseek_legacy" renders as the hyphenated "deepseek-legacy" (the
    # other three values are spelled the same in HCL and argv).
    reasoning_format = "deepseek"

    # A build tag ("b4500") or git sha. Stored/validated only - not yet
    # rendered into argv or enforced against the detected binary.
    build = "b4500"
  }
}
```

`chat_template` (inline Jinja text) and `chat_template_file` (a path) render
`--chat-template`/`--chat-template-file` respectively; setting both on the
same block is a schema/capability conflict (`AMICU-CAP-005`) - `render_command`
emits neither flag and `plan_launch` refuses to build the launch. All five
fields above are optional and independent of each other and of the
sampling/KV-cache/batching fields from PR A.

## llama.cpp: multimodal projector (amicu-planning#151 PR D)

```hcl
resource "deployment" "vision_chat_local" {
  model    = model.vision_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 32768
    port           = 8000
    n_gpu_layers   = -1

    multimodal {
      # A plain path or HF-style reference to the mmproj GGUF - not a second
      # `model` resource reference in this PR (that form can come later if
      # amicu needs to track the projector as its own downloadable
      # artifact). Not validated for existence at plan time; a bad path
      # surfaces as a load failure at apply time.
      projector = "/models/vision_chat/mmproj-f16.gguf"

      # Whether to offload the projector to the accelerator. llama-server
      # defaults this to enabled; `false` renders --no-mmproj-offload
      # explicitly (overrides an inherited LLAMA_ARG_MMPROJ_OFFLOAD=1).
      offload = true
    }
  }
}
```

`projector` renders `--mmproj <value>`; `offload` is a paired bare-flag
toggle like `jinja` above - `true` -> `--mmproj-offload`, `false` ->
`--no-mmproj-offload`, unset -> renders nothing. Both fields are optional and
independent of each other. `multimodal {}` is llama.cpp-only: it backs the
`Modality::Vision` capability the llama.cpp adapter already declares, and a
`multimodal` block on a `vllm {}` settings block is a schema error at parse
time, not a capability diagnostic.

## llama.cpp: speculative decoding with a draft model (amicu-planning#151 PR C)

The draft model is a **second `model` resource**, referenced by a real graph
edge - not a path string. Its GGUF is resolved and staged exactly like the
primary model's, and its weights count toward the fit check.

```hcl
resource "model" "qwen3_8b" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen3-8B-GGUF"
  }
  architecture = "qwen3"
  artifact { format = "gguf" }
  variant {
    quantization = "q4_k_m"
    file         = "Qwen3-8B-Q4_K_M.gguf"
  }
}

# The draft model: a smaller sibling, declared as its own model resource.
resource "model" "qwen3_0_6b_draft" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen3-0.6B-GGUF"
  }
  architecture = "qwen3"
  artifact { format = "gguf" }
  variant {
    quantization = "q4_k_m"
    file         = "Qwen3-0.6B-Q4_K_M.gguf"
  }
}

resource "deployment" "qwen3_local_spec" {
  model    = model.qwen3_8b.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 32768
    port           = 8000
    n_gpu_layers   = -1

    speculative {
      # Required: a reference to a *different* model resource. Naming this
      # deployment's own model is AMICU-CAP-007 and the launch is refused.
      model = model.qwen3_0_6b_draft.id

      # All four draft parameters are optional and independent.
      n_max        = 8    # --spec-draft-n-max: max tokens drafted per step
      n_min        = 2    # --spec-draft-n-min: min draft tokens to use
      p_min        = 0.75 # --draft-p-min: stop drafting below this probability
      n_gpu_layers = 99   # --n-gpu-layers-draft: draft layers on the accelerator
    }
  }
}
```

Behavior:

- Omitting `speculative {}` is exactly today's behavior - nothing extra is
  resolved, rendered, or counted.
- `speculative.model` is validated like any other reference: a dangling id is
  `AMICU-REF-001`; a non-GGUF draft artifact, or a draft model declaring no
  GGUF variant, is `AMICU-CAP-001`; the deployment's own model is
  `AMICU-CAP-007`.
- The draft model's weights are added to the fit check's weights total, so a
  config that fits alone can correctly stop fitting once a draft model is
  declared. The plan records
  `"speculative: draft model weights included, draft KV cache not estimated"`
  in its assumptions - the draft model's KV cache is *not* estimated, since
  that needs the draft model's own layer/head shape.
- There is no draft context-size field: current upstream llama.cpp exposes no
  draft context-size flag at all (`common_params_speculative_draft` has no
  `n_ctx` member, and `common/arg.cpp` registers no `--ctx-size-draft`). See
  `amicu-local/references/llama-server-args.md` for the full flag/env table
  and the alias/removal details.

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

    # amicu-planning#158: unset here (default) so vLLM captures CUDA graphs
    # for this production deployment; see "Modal fast boot" below for the
    # opposite choice.
    # enforce_eager           = false
  }
}
```

`tensor_parallel_size` should match the hardware profile's accelerator `count`.
`gpu_memory_utilization` is a fraction in `(0, 1]`. `kv_cache_dtype` accepts
`auto`, `fp8`, `fp8_e5m2`, `fp8_e4m3` - vLLM's own value set, distinct from
llama.cpp's `cache_type_k`/`cache_type_v`. `max_num_seqs` is vLLM's rough
equivalent of llama.cpp's `parallel_slots`; `max_num_batched_tokens` of
`batch_size`. vLLM has no sampling-defaults deployment flags, so there is no
`temperature`/`top_p`/... equivalent on `vllm {}`. `enforce_eager` (bool,
amicu-planning#158) toggles CUDA-graph capture at startup: `true` skips it
for a faster cold start at the cost of steady-state throughput (the trade-off
a "fast boot" preset wants), `false` forces capture, and unset leaves vLLM's
own default (captured) in effect.

## Modal autoscaling: `scaling {}` (amicu-planning#180)

Deployment-level, optional, provider-agnostic block; only the `modal` provider
maps it today (others get `AMICU-CAP-009`). Omitted fields keep today's
defaults (min 0, no max, target concurrency 32, scaledown 15m).

```hcl
  scaling {
    min_replicas       = 0
    max_replicas       = 1        # >= 1 and >= min_replicas
    target_concurrency = 32       # >= 1
    scaledown_window   = "15m"    # "90s" | "15m" | "1h"; > 0
  }
```

See `modal-deployment.md` for details (`AMICU-CAP-008` validation, plan output).

## vLLM on Modal (serverless GPU, amicu-planning#158)

Setup, multi-GPU, metrics and state behaviour: `modal-deployment.md`.

```hcl
resource "hardware_profile" "modal_h100" {
  provider = "modal"
  region   = "eu-west"
  accelerator {
    kind   = "gpu"
    model  = "h100"
    count  = 2
    memory = "80GiB"
  }
  memory = "32GiB"
}

resource "deployment" "chat_modal" {
  model    = model.chat_prod.id
  hardware = hardware_profile.modal_h100.id
  runtime  = "vllm"
  api_key  = secret("CHAT_MODAL_API_KEY")   # required off-machine; export it before `apply`
  vllm {
    context_length       = 32768
    port                 = 8000
    tensor_parallel_size = 2

    # Fast boot: skip CUDA-graph capture at startup (renders `--enforce-eager`;
    # `false` renders `--no-enforce-eager`).
    enforce_eager = true
  }
}
```

`model.chat_prod.source.reference` **must be a Hugging Face repo id**
(e.g. `"meta-llama/Llama-3.1-8B-Instruct"`), never a local path: Modal's
container cannot see this machine's filesystem, and `ModalSupervisor`
rejects a local-looking model reference (a leading `/`, `./`, `../`, or `~`)
before making any Modal call.

What actually happens on `amicu apply` for `chat_modal`:
- `amicu-vllm`'s renderer builds the same `vllm serve ...` argv it would for
  a local launch; `ModalSupervisor` rewrites `--host 127.0.0.1` to
  `--host 0.0.0.0` (Modal's proxy reaches the container over its internal
  network, not loopback) and refuses to proceed if the model argument looks
  like a local path.
- The container image is `vllm/vllm-openai:v0.29.0` (a fixed default,
  overridable process-wide via `AMICU_MODAL_VLLM_IMAGE` - there is no HCL
  `modal { image = ... }` block yet).
- Two Modal Volumes are mounted and reused across every amicu deployment on
  the workspace: `amicu-hf-cache` at `/root/.cache/huggingface` and
  `amicu-vllm-cache` at `/root/.cache/vllm`.
- Fixed autoscaling/lifecycle defaults (not yet HCL-configurable):
  scaledown window 900s, startup timeout 600s, container timeout 3600s,
  target concurrency 32, `min_containers = 0` (scales to zero).
- Plain `vllm { env = {...} }` entries are baked into the image at build
  time (they already cannot hold a secret-shaped value - `AMICU-POLICY-006`
  warns, and vLLM's own `env`/`extra_args` denylists block the obvious
  secret-carrying flags). A resolved `secret` reference in the deployment's
  `secret_refs` is instead forwarded through a Modal Secret, injected only
  into the running container - never written to the image. The deployment's
  `api_key` takes the same route, as `VLLM_API_KEY` / `LLAMA_API_KEY`. As a narrow,
  documented bridge until vLLM deployments can declare a secret reference at
  all, `ModalSupervisor` also reads `HF_TOKEN` from the *operator's own*
  local environment (never from the deployment's `env` block) and forwards
  it the same secret-only way, for a gated Hugging Face model.

To try this: export `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` (same credentials
`amicu modal auth-check` uses) before running `amicu apply` - a missing
credential is rejected before any Modal call, with neither variable's value
ever printed.

**Endpoint API key:** `api_key = secret("NAME")` on the deployment is
required for any off-machine provider (`AMICU-POLICY-007`) and optional for
local loopback ones. Create the value with `export NAME=$(openssl rand -hex 32)`
before `apply` (the local resolver reads the environment variable named like the
secret). Details and the post-deploy exposure check: `modal-deployment.md`.

**Calling the endpoint:**

Modal Servers require proxy auth: unauthenticated requests to the
`*.modal.direct` URL get a `401` from Modal's proxy. Create a Proxy Auth
Token (Modal dashboard, Settings -> Proxy Auth Tokens) and export
`MODAL_PROXY_TOKEN_ID` (`wk-...`) and `MODAL_PROXY_TOKEN_SECRET` (`ws-...`);
amicu sends them as `Modal-Key` / `Modal-Secret` on its health/status probes
(never logged). By hand:
`curl -H "Modal-Key: $MODAL_PROXY_TOKEN_ID" -H "Modal-Secret: $MODAL_PROXY_TOKEN_SECRET" "$URL/v1/models"`.
Add `-H "Authorization: Bearer $CHAT_MODAL_API_KEY"` for the deployment's own
API key (the proxy token uses different headers, not `Authorization`).

## llama.cpp on Modal (serverless GPU, amicu-planning#160)

```hcl
resource "model" "gemma_gguf" {
  # source registry must be hugging_face, reference "ggml-org/gemma-3-1b-it-GGUF";
  # ... architecture, artifact (gguf) and variant blocks as usual
}

resource "deployment" "gemma_modal" {
  model    = model.gemma_gguf.id
  hardware = hardware_profile.modal_h100.id   # provider = "modal"
  runtime  = "llama_cpp"
  api_key  = secret("GEMMA_MODAL_API_KEY")
  llama_cpp {
    context_length = 8192
    port           = 8080
    n_gpu_layers   = -1
  }
}
```

The model's `source.registry` **must be `hugging_face`**: the container
downloads the GGUF itself. A model with any other source (or a local path) is
rejected before any Modal call with an error asking for a Hugging Face GGUF
reference.

What actually happens on `amicu apply`:
- The llama.cpp renderer's local `-m models/<id>/<file>` is replaced by
  `llama-server -hf <source.reference> -hff <recommended variant file>`, and
  `--host 0.0.0.0` is appended, same as vLLM.
- The image is `ghcr.io/ggml-org/llama.cpp:server-cuda-b11515` (overridable
  via `AMICU_MODAL_LLAMACPP_IMAGE`). It has no Python, so the build adds
  `python3` + `python-is-python3` with `apt-get` (Modal's runtime and the
  baked shim are Python), puts `llama-server` on `PATH`, and clears the
  image's `ENTRYPOINT` so Modal's runtime is PID 1.
- `LLAMA_CACHE=/root/.cache/llama.cpp` points at the Modal Volume
  `amicu-llamacpp-cache`, so a restart or cold start reuses the downloaded
  GGUF. A `llama_cpp { env = { LLAMA_CACHE = ... } }` entry overrides it.
- `HF_TOKEN` (for gated repos) is forwarded exactly as for vLLM: a resolved
  secret, else the operator's own `HF_TOKEN`, through a Modal Secret only.
- The shim waits for `GET /health` to return 200 (llama-server opens its port
  before the model is loaded), so the deployment is only "ready" once the GGUF
  has downloaded and loaded.
- `-hf` would auto-download the repo's multimodal projector, so `--no-mmproj`
  is appended unless the deployment has a `multimodal {}` block.
- Rejected before any Modal call (local files Modal cannot see):
  `speculative {}` (draft GGUF), `multimodal { projector }` / `--mmproj`/`-mm`,
  `--chat-template-file`, `--lora`, `--lora-scaled`, `--control-vector`,
  `--control-vector-scaled`. Also rejected, because they would bake a secret
  into the image command line: `--hf-token`/`-hft`, `--api-key`,
  `--api-key-file` (use the `HF_TOKEN` secret path instead).
- Autoscaling defaults and the stop path are the same as for vLLM.
- Calling the endpoint needs the same Modal Proxy Auth Token headers as vLLM
  (see "Calling the endpoint" above); amicu's status probe sends them too.

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
    # Sampling, KV-cache dtype, batching, flash-attn, parallel slots, chat
    # template, jinja, reasoning-format, multimodal projector, and
    # speculative decoding are all typed fields (see above) - put them
    # there, not here. Passing any of their flags (including --mmproj or
    # --model-draft and friends) through extra_args is AMICU-CAP-002. What
    # remains untyped after amicu-planning#151 (the speculative *type*/ngram
    # knobs) still goes through extra_args, tracked as a separate PR against
    # the same issue.
    multimodal {
      projector = "/models/vision/mmproj-f16.gguf"
    }
    speculative {
      model = model.vision_chat_draft.id
    }
    extra_args = [
      "--spec-type", "draft-dflash,ngram-map-k4v",
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
  `--parallel`/`-np`, `--flash-attn`/`-fa`, `--chat-template`,
  `--chat-template-file`, `--jinja`/`--no-jinja`, `--reasoning-format`,
  `--mmproj`/`-mm`, `--mmproj-offload`/`--no-mmproj-offload`, and every
  speculative spelling - `--model-draft`/`--spec-draft-model`/`-md`,
  `--spec-draft-n-max`/`--draft-max`/`--draft-n`/`--draft`,
  `--spec-draft-n-min`/`--draft-min`/`--draft-n-min`,
  `--draft-p-min`/`--spec-draft-p-min`,
  `--n-gpu-layers-draft`/`--spec-draft-ngl`/`--gpu-layers-draft`/`-ngld` - in
  either `--foo value` or `--foo=value` form) is rejected with
  `AMICU-CAP-002` - fix the ambiguity by removing one side, not by hoping
  the last one wins. The same check applies to an `env` entry whose key is
  `LLAMA_ARG_<X>` and mirrors one of those flags (e.g. `LLAMA_ARG_TEMP`,
  `LLAMA_ARG_CACHE_TYPE_K`, `LLAMA_ARG_CHAT_TEMPLATE`, `LLAMA_ARG_MMPROJ`,
  `LLAMA_ARG_MMPROJ_OFFLOAD`, and `LLAMA_ARG_THINK` for `--reasoning-format` -
  not `LLAMA_ARG_REASONING_FORMAT`, which llama-server does not read).
- A denylisted flag (`--host`, `--api-key`, `--api-key-file`,
  `--ssl-key-file`) is rejected with `AMICU-CAP-003`: amicu owns the bind
  address, and the others put secret material - or a path to it - directly
  into argv, which any process listing can read. Put real secrets in `env`
  (or better, a `credential`'s secret-designated `env`), not `extra_args`.
- Any non-empty `extra_args` gets an informational `AMICU-CAP-004`, and the
  plan's cost/performance estimate is marked
  `"unverified: raw runtime flags present"` - the estimator cannot interpret
  flags it doesn't have a typed model for (the speculative type/ngram knobs,
  ...).
- An `env` value that looks like a real secret (an AWS key, an OpenAI-style
  key, a PEM header, a JWT, or a long high-entropy token) gets a
  `AMICU-POLICY-006` warning nudging it toward a `credential` resource
  instead; this is a heuristic, not a hard rejection like `credential.env`'s
  `AMICU-POLICY-004`.
- Setting both `chat_template` and `chat_template_file` on the same
  `llama_cpp {}` block is `AMICU-CAP-005` (amicu-planning#151 PR B): they
  render mutually exclusive flags, so `render_command` emits neither and
  `plan_launch` refuses the launch - fix by removing one.
- A `speculative {}` block whose draft `model` is the deployment's own model
  is `AMICU-CAP-007` (amicu-planning#151 PR C): a draft model must be a
  different, smaller model, or `llama-server` would load the same weights
  twice for no speedup.
- `extra_args`/`env` fields on the `vllm {}` block parse the same way, ahead
  of the vLLM command renderer that will consume them.
- A `multimodal { ... }` block is only valid inside `llama_cpp {}`
  (amicu-planning#151 PR D): putting one inside `vllm {}` is a structural
  schema error at parse time (`VllmSettings` has no field for it), not a
  capability diagnostic - fix by moving it to the `llama_cpp {}` block or
  removing it.

Notes:
- The settings block must match the `runtime`: `llama_cpp {}` for `llama_cpp`,
  `vllm {}` for `vllm`.
- `model` and `hardware` must reference ids that exist, or validation reports
  `AMICU-REF-001`.
