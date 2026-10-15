# Reference machine: Apple M4 Pro, 48 GiB unified memory.
# `amicu init --example` sizes the starter to your own machine instead.

resource "model" "qwen35_27b" {
  source {
    registry = "hugging_face"
    reference = "unsloth/Qwen3.5-27B-GGUF"
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
    file = "Qwen3.5-27B-Q4_K_M.gguf"
    size = "16740812704B"
  }

  n_layers = 64
  n_kv_heads = 4
  head_dim = 256
  n_kv_layers = 16
}

resource "hardware_profile" "local_dev" {
  provider = "local_apple"

  accelerator {
    kind = "apple_silicon"
    model = "m4_pro"
    count = 1
    memory = "48GiB"
  }

  memory = "48GiB"
}

resource "deployment" "qwen35_27b_local" {
  model = model.qwen35_27b.id
  hardware = hardware_profile.local_dev.id
  runtime = "llama_cpp"

  llama_cpp {
    context_length = 262144
    port = 8000
    n_gpu_layers = -1
    extra_args = [
      "--n-predict",
      "65536"
    ]
  }
}
