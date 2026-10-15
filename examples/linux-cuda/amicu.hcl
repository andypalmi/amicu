# Reference machine: Linux, 64 GiB RAM, one NVIDIA RTX 4090 (24 GiB VRAM).
# `amicu init --example` sizes the starter to your own machine instead.

resource "model" "qwen35_9b" {
  source {
    registry = "hugging_face"
    reference = "unsloth/Qwen3.5-9B-GGUF"
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
    file = "Qwen3.5-9B-Q4_K_M.gguf"
    size = "5680522464B"
  }

  n_layers = 32
  n_kv_heads = 4
  head_dim = 256
  n_kv_layers = 8
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

resource "deployment" "qwen35_9b_local" {
  model = model.qwen35_9b.id
  hardware = hardware_profile.local_gpu.id
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
