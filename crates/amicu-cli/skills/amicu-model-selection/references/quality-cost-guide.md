# Quality / cost guide

## Size classes

| Class | Params | Typical use | Local feasibility |
|---|---|---|---|
| Small | 1-4B | Classification, routing, drafts | CPU or any Apple Silicon |
| Mid | 7-9B | General chat, RAG, tools | Apple Silicon 16-36 GiB |
| Large | 30-70B | High-quality reasoning | Multi-GPU or high-memory only |

Bigger models raise quality and cost/latency together. Choose the smallest class
that clears the quality bar for the task.

## Quantization trade-off

- Local GGUF: `q4_k_m` is the default balance; step to `q5_k_m`/`q6_k` when
  quality matters and memory allows; `q8_0` for near-FP16 at large size.
- Production: FP16/BF16 for reference quality; AWQ/GPTQ 4-bit roughly halves
  VRAM with a modest quality cost — worth it when VRAM-bound.

## Framing cost and latency

- Cost is driven by hardware `price.per_hour` × time, plus GPU count. Present a
  range, not a point estimate, and state the assumptions (utilization,
  concurrency).
- Latency depends on model size, quantization, context length, and hardware.
  Smaller/quantized models on faster accelerators lower latency.
- Always attach a confidence level and note the main risk (e.g. "quality of a
  4-bit quant on this task is unverified").

## Decision checklist

1. Does it meet the quality bar? 2. Does it fit the hardware memory?
3. Does it satisfy residency/provider constraints? 4. Is the cost/latency range
acceptable? Offer at least one cheaper and one higher-quality alternative.
