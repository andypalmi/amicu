# Reference machine: no local hardware needed.
# `amicu init --example` sizes the starter to your own machine instead.

resource "model" "qwen" {
  source {
    registry = "hugging_face"
    reference = "Qwen/Qwen3-8B"
  }

  architecture = "qwen3"
  modalities = [
    "text"
  ]

  artifact {
    format = "safetensors"
  }

  context_length = 32768
}

resource "hardware_profile" "gpu_l4" {
  provider = "modal"
  region = "eu-west"

  accelerator {
    kind = "gpu"
    model = "l4"
    count = 1
    memory = "24GiB"
  }

  memory = "32GiB"
}

resource "deployment" "chat" {
  model = model.qwen.id
  hardware = hardware_profile.gpu_l4.id
  runtime = "vllm"

  vllm {
    context_length = 8192
    port = 8000
  }

  api_key = secret("CHAT_API_KEY")
}
