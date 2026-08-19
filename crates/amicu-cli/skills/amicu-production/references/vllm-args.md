# vLLM engine args

The `vllm {}` block maps to vLLM's OpenAI-compatible server engine arguments.

| HCL field | vLLM arg | Notes |
|---|---|---|
| model source/artifact | `--model <ref>` | Hugging Face reference or local path. |
| `port` | `--port <n>` | Server bind port. |
| `context_length` | `--max-model-len <n>` | Upper bound on sequence length. |
| `tensor_parallel_size` | `--tensor-parallel-size <n>` | GPUs per replica; = accelerator count. |
| `gpu_memory_utilization` | `--gpu-memory-utilization <f>` | Fraction of VRAM, `(0, 1]`. |

## Sizing

- FP16/BF16 weights: ~`2 × params` bytes (an 8B model ≈ 16 GiB) before
  KV-cache. KV-cache grows with `context_length` and concurrency and consumes
  the remainder governed by `gpu_memory_utilization`.
- If weights + KV-cache exceed a single GPU, raise `tensor_parallel_size` and
  provision more accelerators in the hardware profile.

## Throughput tips

- vLLM's continuous batching handles concurrency; you do not size per-request.
- Prefer higher `gpu_memory_utilization` (≤ 0.95) on dedicated GPUs for a larger
  KV-cache and better throughput.
- Quantized safetensors (e.g. AWQ/GPTQ) reduce VRAM at some quality cost; see
  amicu-model-selection for the trade-off.
