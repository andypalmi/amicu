---
name: amicu-production
description: Use for production, GPU-served inference with Amicu - vLLM on NVIDIA GPUs, Kubernetes/Helm delivery, cloud providers, tensor-parallel sharding, GPU memory utilization, and autoscaling. Triggers on vLLM, Kubernetes, Helm, safetensors serving, `tensor_parallel_size`, A100/H100, or multi-GPU/cloud deployment.
---

# Amicu Production

Production deployments use the `vllm` runtime to serve models on NVIDIA GPUs,
typically delivered to Kubernetes via Helm on a cloud provider. Use this for
throughput, multi-GPU sharding, and autoscaling.

## When to use production

- NVIDIA GPU hardware (`accelerator.kind = "gpu"`), single or multi-GPU.
- Safetensors artifacts served with vLLM for high throughput.
- Cloud providers with residency constraints (see `amicu-routing`).

For on-device / Apple Silicon / CPU, use `amicu-local` instead.

## Deployment block

```hcl
resource "deployment" "chat_prod" {
  model    = model.chat_prod.id
  hardware = hardware_profile.eu_a100.id
  runtime  = "vllm"
  vllm {
    context_length         = 32768
    port                   = 8000
    tensor_parallel_size   = 2      # match accelerator.count
    gpu_memory_utilization = 0.90   # fraction in (0, 1]
  }
}
```

## GPU parallelism

- `tensor_parallel_size` shards the model across GPUs; set it to the hardware
  profile's `accelerator.count`. A mismatch wastes GPUs or fails to fit.
- `gpu_memory_utilization` caps the fraction of VRAM vLLM reserves; lower it if
  you co-locate other workloads, raise it (≤ ~0.95) for max KV-cache.
- Large models that do not fit one GPU need `tensor_parallel_size > 1` and
  enough total VRAM across the profile's accelerators.

## Workflow

```bash
amicu validate --output json   # capacity + residency checks
amicu plan --output json       # rendered deployment actions
amicu apply                    # after explicit approval only
```

Never `apply` a production change without a reviewed plan and user approval.

## References

- `references/vllm-args.md` - vLLM engine args and how fields map.
- `references/helm-chart-config.md` - Kubernetes/Helm delivery and autoscaling.
