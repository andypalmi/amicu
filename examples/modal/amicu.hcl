# Reference machine: no local hardware needed.
# `amicu init --example` sizes the starter to your own machine instead.

resource "model" "qwen" {
  source {
    registry = "hugging_face"
    reference = "RedHatAI/Qwen3.5-9B-FP8-dynamic"
  }

  architecture = "qwen3_5"
  modalities = [
    "text"
  ]

  artifact {
    format = "safetensors"
    quantization = "fp8"
  }

  context_length = 262144
  n_layers = 32
  n_kv_heads = 4
  head_dim = 256
  n_kv_layers = 8
}

resource "hardware_profile" "gpu_2xl4" {
  provider = "modal"
  region = "eu-west"

  accelerator {
    kind = "gpu"
    model = "l4"
    count = 2
    memory = "24GiB"
  }

  memory = "64GiB"
}

resource "deployment" "chat" {
  model = model.qwen.id
  hardware = hardware_profile.gpu_2xl4.id
  runtime = "vllm"

  vllm {
    context_length = 262144
    port = 8000
    tensor_parallel_size = 2

    env {
      NCCL_P2P_DISABLE = "1"
    }
  }

  api_key = secret("CHAT_API_KEY")
}
