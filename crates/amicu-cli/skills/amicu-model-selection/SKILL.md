---
name: amicu-model-selection
description: Use when choosing which model, artifact, and quantization to deploy with Amicu — searching the registry, checking whether a model fits given hardware, and weighing quality against cost and latency. Triggers on "which model should I use", model/artifact/quantization choice, hardware-fit questions, or quality/cost trade-offs.
---

# Amicu Model Selection

Pick a model, artifact format, and quantization that meet the use case and fit
the target hardware. Query capability rather than guessing; propose a primary
choice plus alternatives with their trade-offs.

## Selection workflow

1. Capture requirements: task, quality bar, context length, latency, budget,
   data sensitivity, existing hardware/provider (see `amicu-presets`).
2. Shortlist candidate models by architecture and modality.
3. Choose artifact + quantization for the runtime:
   - Local llama.cpp → GGUF + a `q*` quantization (`amicu-local`).
   - Production vLLM → safetensors (optionally AWQ/GPTQ) (`amicu-production`).
4. Check hardware fit: weights + KV-cache must fit accelerator/host memory.
5. Present primary + alternatives with quality/cost/latency notes and
   confidence.

## Hardware fit (rule of thumb)

- GGUF size ≈ `params × bits / 8` (7B `q4_k_m` ≈ 4 GiB).
- vLLM FP16 weights ≈ `2 × params` bytes (8B ≈ 16 GiB), plus KV-cache.
- Leave headroom for KV-cache, which scales with context length and concurrency.
- Exceeding memory surfaces `AMICU-MEM-*`; validate to confirm fit.

## Command surface

`amicu models search`, `amicu models show`, and `amicu capabilities model
--hardware` are the intended discovery commands but are NOT implemented yet
(future tasks). Until they ship, select from known models and confirm fit with
`amicu validate`. Do not fabricate output for the unimplemented commands.

## Reference

- `references/quality-cost-guide.md` — size classes, quantization trade-offs,
  and how to frame cost/latency ranges with confidence.
