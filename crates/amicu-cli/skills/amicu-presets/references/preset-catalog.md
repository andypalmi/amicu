# Preset catalog

Initial preset shapes. A preset asks use-case questions, applies hard
constraints and soft preferences, ranks candidates, and emits editable HCL with
explained trade-offs. Treat these as starting points, not fixed recipes.

## Local Dev Chat

- Fit: on-device experimentation, privacy-sensitive data, no cloud spend.
- Constraints: Apple Silicon or CPU; GGUF + `q4_k_m`/`q5_k_m`; llama.cpp.
- Candidate: mid-size (7-9B) model on `local_apple`, `n_gpu_layers = -1`.
- Estimate: zero marginal cost; latency depends on the Mac; high confidence.

## Private EU Production

- Fit: confidential data with EU residency and throughput needs.
- Constraints: EU-only providers (hetzner/ovhcloud/scaleway/nebius),
  `allow_external_providers = false`; vLLM on NVIDIA GPUs.
- Candidate: 8-70B safetensors, `tensor_parallel_size` = GPU count, governance
  rule pinning `allowed_regions` to EU.
- Estimate: cost from `price.per_hour` × GPUs × time; medium confidence pending
  concurrency data.

## Cost-Optimized Batch

- Fit: throughput-first, latency-tolerant workloads.
- Constraints: quantized weights (AWQ/GPTQ or GGUF `q4`), high
  `gpu_memory_utilization`, cheaper hardware.
- Candidate: mid-size model, autoscaled replicas for concurrency.
- Estimate: lowest cost/token; note quality risk from aggressive quantization.

## Using a preset

1. Confirm the use case matches. 2. Fill the requirement gaps.
3. Adjust hard constraints (residency, budget). 4. Generate HCL and validate.
5. Present alternatives + confidence before applying.
