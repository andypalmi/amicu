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
