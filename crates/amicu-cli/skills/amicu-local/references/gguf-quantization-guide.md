# GGUF quantization guide

Quantization shrinks a GGUF model's weights, trading quality for memory and
speed. Pick the smallest quantization that keeps quality acceptable and fits
your accelerator/host memory.

| Quantization | Rel. size | Quality | Use when |
|---|---|---|---|
| `q4_k_m` | ~4-bit | Good default | General local chat; best size/quality balance. |
| `q5_k_m` | ~5-bit | Better | A bit more headroom, noticeably better than q4. |
| `q6_k` | ~6-bit | High | Quality matters and memory allows. |
| `q8_0` | ~8-bit | Near-FP16 | Maximum quality in GGUF; large. |
| `f16` / `bf16` | 16-bit | Full | Reference quality; rarely needed locally. |

## Sizing rule of thumb

Approximate file size ≈ `params × bits / 8`. A 7B model at `q4_k_m` is ~4 GiB;
at `q8_0` ~7.5 GiB. Add KV-cache memory that grows with `context_length` and
concurrency. Keep total below the accelerator/unified `memory` in the hardware
profile or expect `AMICU-MEM-*` diagnostics.

## Declaring variants

```hcl
artifact {
  format       = "gguf"
  quantization = "q4_k_m"
}
variant {
  quantization = "q4_k_m"
  file         = "model-q4_k_m.gguf"
  size         = "4GiB"
}
```

Declare one `variant` per quantization you plan to serve; the deployment's
`artifact.quantization` selects which file is used.
