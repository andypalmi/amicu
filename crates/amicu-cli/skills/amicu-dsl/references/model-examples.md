# Model examples

## GGUF model (local, llama.cpp)

```hcl
resource "model" "support_chat" {
  source {
    registry  = "hugging_face"
    reference = "Qwen/Qwen2.5-7B-Instruct"
  }
  architecture   = "qwen2"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "qwen2.5-7b-instruct-q4_k_m.gguf"
    size         = "4GiB"
  }
}
```

## Safetensors model (production, vLLM)

```hcl
resource "model" "chat_prod" {
  source {
    registry  = "hugging_face"
    reference = "meta-llama/Llama-3.1-8B-Instruct"
  }
  architecture   = "llama"
  modalities     = ["text"]
  artifact { format = "safetensors" }
  context_length = 32768
}
```

Notes:
- `variant` blocks list concrete GGUF files by quantization; provide one per
  quantization you intend to serve.
- Omit `artifact.quantization` for full-precision safetensors.
- `registry` values include `hugging_face`; `modalities` currently `["text"]`.
