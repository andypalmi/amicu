# Reference machine: Linux, 64 GiB RAM, one NVIDIA RTX 4090 (24 GiB VRAM).
# `amicu init --example` sizes the starter to your own machine instead.

resource "model" "qwen3_8b" {
  source {
    registry = "hugging_face"
    reference = "Qwen/Qwen3-8B-GGUF"
  }

  architecture = "qwen3"
  modalities = [
    "text"
  ]

  artifact {
    format = "gguf"
    quantization = "q4_k_m"
  }

  context_length = 32768

  variant {
    quantization = "q4_k_m"
    file = "Qwen3-8B-Q4_K_M.gguf"
    size = "5027783488B"
  }
}

resource "hardware_profile" "local_gpu" {
  provider = "on_prem"

  accelerator {
    kind = "gpu"
    model = "rtx_4090"
    count = 1
    memory = "24GiB"
  }

  memory = "64GiB"
}

resource "deployment" "qwen3_8b_local" {
  model = model.qwen3_8b.id
  hardware = hardware_profile.local_gpu.id
  runtime = "llama_cpp"

  llama_cpp {
    context_length = 8192
    port = 8000
    n_gpu_layers = -1
  }
}
