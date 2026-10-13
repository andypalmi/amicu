# Hardware profile examples

## Apple Silicon (local)

```hcl
resource "hardware_profile" "dev_mac" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m3_pro"
    count  = 1
    memory = "36GiB"
  }
  memory = "36GiB"
}
```

## CPU-only (local)

```hcl
resource "hardware_profile" "cpu_box" {
  provider = "local_cpu"
  cpu {
    model = "ryzen_9_7950x"
    cores = 16
  }
  memory = "64GiB"
}
```

## NVIDIA GPU (cloud, EU region)

Plan-only: `amicu apply` supports only local providers (`local`, `local_apple`,
`local_cpu`, `on_prem`) and `modal`; any other provider fails at apply.

```hcl
resource "hardware_profile" "eu_a100" {
  provider = "hetzner"
  region   = "eu-central"
  accelerator {
    kind   = "nvidia_gpu"
    model  = "a100_80gb"
    count  = 2
    memory = "160GiB"
  }
  memory = "256GiB"
  price { per_hour = "3.20 EUR" }
}
```

Notes:
- `accelerator.memory` is the accelerator's own memory (VRAM / unified); the
  top-level `memory` is host RAM. Capacity checks (`AMICU-MEM-*`) compare model
  size against these.
- Set `region` and `provider` to satisfy `data_class` residency rules.

## NVIDIA GPU (Modal, catalog only)

```hcl
resource "hardware_profile" "modal_h100" {
  provider = "modal"
  region   = "eu-west"
  accelerator {
    kind   = "nvidia_gpu"
    model  = "H100"
    count  = 1
    memory = "80GiB"
  }
  memory = "64GiB"
  price { per_hour = "3.9492 USD" }
}
```

Notes:
- Modal (`amicu_providers::modal`) is genuinely regionalized: broad regions
  (`"us"`, `"eu"`, `"ap"`) and narrow ones (`"eu-west"`, `"us-west"`, `"uk"`,
  `"jp"`, ...) are all valid `region` values, each tagged with its real
  residency jurisdiction - `eu`/`eu-west`/`eu-north`/`eu-south` satisfy an
  `eu` `data_class` residency constraint; a region Modal does not serve
  fails closed. Pinning to a region multiplies the base price shown here
  (1.15x broad, 1.75x narrow - not reflected in `price` above); see
  `amicu_providers::modal`'s module docs for the source.
- On Modal the profile's `memory` (and `cpu { cores }` when present) is sent as
  the container's memory/CPU request (MiB / milli-cores), so size it for the
  workload, not just the GPU. Deployments on this provider also need an
  `api_key` (see `modal-deployment.md`).
- The `region` also picks the web endpoint's proxy region (`eu*`/`uk` ->
  `eu-west`, `us-west` -> `us-west`, `ap*`/`jp`/`au` -> `ap-south`, otherwise
  `us-east`). Modal does not allow a routing region to change after the
  initial deployment, so changing `region` on an existing deployment needs
  `amicu modal stop <app>` and then a re-apply.
- The `amicu-providers` catalog `provision`/`status`/`teardown` still reject
  with `AMICU-PROVIDER-015` (permanent - not retryable), but that is not the
  deploy path: `amicu apply` deploys `vllm` and `llama_cpp` deployments on a
  `provider = "modal"` profile through `ModalSupervisor`. See
  `modal-deployment.md`.
- Prices are USD/hr, converted from Modal's per-second billing; see
  `amicu_providers::modal`'s module docs for the verification date and
  source.

## Consumer/on-prem NVIDIA GPU

```hcl
resource "hardware_profile" "home_4090" {
  provider = "on_prem"
  accelerator {
    kind           = "gpu"
    model          = "rtx_4090"
    count          = 1
    memory         = "24GiB"
    bandwidth_gbps = 1008
  }
  memory = "64GiB"
}
```

## Consumer/on-prem Apple Silicon

```hcl
resource "hardware_profile" "studio_m4_max" {
  provider = "on_prem"
  accelerator {
    kind           = "apple_silicon"
    model          = "m4_max_128gb"
    count          = 1
    memory         = "128GiB"
    bandwidth_gbps = 546
  }
  memory = "128GiB"
}
```

## Consumer/on-prem AMD GPU

```hcl
resource "hardware_profile" "home_7900xtx" {
  provider = "on_prem"
  accelerator {
    kind           = "gpu" # AcceleratorKind has no AMD-specific variant
    model          = "rx_7900_xtx"
    count          = 1
    memory         = "24GiB"
    bandwidth_gbps = 960
  }
  memory = "64GiB"
}
```

Notes:
- `bandwidth_gbps` records the accelerator's memory bandwidth (GB/s) for
  planning purposes; it is optional and not yet read by the fit check.
- These three examples match `amicu_providers::consumer`'s `rtx_4090()`,
  `m4_max_128gb()`, and `rx_7900_xtx()` catalog entries. `provider =
  "on_prem"` is the convention for hardware the user already owns - distinct
  from the pre-existing `"local_apple"` / `"local_cpu"` examples above, which
  predate this catalog and are left as-is.

## Modal (serverless GPU, amicu-planning#158)

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
```

Notes:
- Modal Servers require proxy auth: unauthenticated requests to the
  `*.modal.direct` URL get a `401` from Modal's proxy. Create a Proxy Auth
  Token (Modal dashboard, Settings -> Proxy Auth Tokens) and export
  `MODAL_PROXY_TOKEN_ID` (`wk-...`) and `MODAL_PROXY_TOKEN_SECRET` (`ws-...`);
  amicu sends them as `Modal-Key` / `Modal-Secret` on its health/status probes
  (never logged). By hand:
  `curl -H "Modal-Key: $MODAL_PROXY_TOKEN_ID" -H "Modal-Secret: $MODAL_PROXY_TOKEN_SECRET" "$URL/v1/models"`.
  (Not `Authorization: Bearer`, which vLLM's `--api-key` also uses.)
- `provider = "modal"` routes a `vllm` deployment on this profile through
  `amicu apply`'s `ModalSupervisor` (`crates/amicu-modal/src/supervisor.rs`)
  instead of a local process. `accelerator.model` + `accelerator.count`
  become Modal's `gpu=` request string - `"H100:2"` for the profile above
  (upper-cased; the `:<count>` suffix is omitted when `count == 1`;
  `amicu plan` shows `gpu.h100 x2`) - and
  `region` (when set) becomes Modal's region constraint. This mapping is a
  hand-maintained string pass-through, not validated against Modal's GPU
  catalog at parse time (that catalog, `amicu-providers::modal`, lands
  separately) - an unsupported `model` string surfaces as a Modal-side
  rejection at apply time, not a parse-time diagnostic.
- Only GPU accelerators are deployable to Modal today: a `tpu` or
  `apple_silicon` accelerator on a `provider = "modal"` profile is rejected
  before any RPC (Modal has no such offering).
- Both `vllm` and `llama_cpp` deployments on a `provider = "modal"` profile
  route through `ModalSupervisor`. A `llama_cpp` deployment needs a model whose
  source is Hugging Face (the GGUF is fetched in the container with `-hf`). See
  `deployment-examples.md`'s "vLLM on Modal" and "llama.cpp on Modal" sections
  and `modal-deployment.md` for the full examples and defaults.
