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

Notes:
- The settings block must match the `runtime`: `llama_cpp {}` for `llama_cpp`,
  `vllm {}` for `vllm`.
- `model` and `hardware` must reference ids that exist, or validation reports
  `AMICU-REF-001`.
