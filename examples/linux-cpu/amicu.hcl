# Reference machine: Linux, 16 GiB RAM, no GPU.
# `amicu init --example` sizes the starter to your own machine instead.

resource "model" "qwen3_4b" {
  source {
    registry = "hugging_face"
    reference = "Qwen/Qwen3-4B-GGUF"
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
    file = "Qwen3-4B-Q4_K_M.gguf"
    size = "2497280256B"
  }
}

resource "hardware_profile" "local_dev" {
  provider = "local_cpu"

  cpu {
    model = "amd_ryzen_9_7950x"
    cores = 16
  }

  memory = "16GiB"
}

resource "deployment" "qwen3_4b_local" {
  model = model.qwen3_4b.id
  hardware = hardware_profile.local_dev.id
  runtime = "llama_cpp"

  llama_cpp {
    context_length = 8192
    port = 8000
    n_gpu_layers = 0
  }
}
