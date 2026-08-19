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
