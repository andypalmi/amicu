# Reference machine: Linux, 16 GiB RAM, no GPU.
# `amicu init --example` sizes the starter to your own machine instead.

resource "model" "qwen35_4b" {
  source {
    registry = "hugging_face"
    reference = "unsloth/Qwen3.5-4B-GGUF"
  }

  architecture = "qwen35"
  modalities = [
    "text"
  ]

  artifact {
    format = "gguf"
    quantization = "q4_k_m"
  }

  context_length = 262144

  variant {
    quantization = "q4_k_m"
    file = "Qwen3.5-4B-Q4_K_M.gguf"
    size = "2740937888B"
  }

  n_layers = 32
  n_kv_heads = 4
  head_dim = 256
  n_kv_layers = 8
}

resource "hardware_profile" "local_dev" {
  provider = "local_cpu"

  cpu {
    model = "amd_ryzen_9_7950x"
    cores = 16
  }

  memory = "16GiB"
}

resource "deployment" "qwen35_4b_local" {
  model = model.qwen35_4b.id
  hardware = hardware_profile.local_dev.id
  runtime = "llama_cpp"

  llama_cpp {
    context_length = 262144
    port = 8000
    n_gpu_layers = 0
    extra_args = [
      "--n-predict",
      "65536"
    ]
  }
}
