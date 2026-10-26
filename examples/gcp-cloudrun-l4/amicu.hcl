# Reference machine: no local hardware needed.
# Qwen3-0.6B in fp16 on a Cloud Run service with one L4 per instance in
# europe-west4. It scales to zero: no instance, no GPU bill, while idle. The
# first request after an idle period starts an instance and loads the model, so
# give clients a long timeout on it.

resource "model" "qwen" {
  source {
    registry = "hugging_face"
    reference = "Qwen/Qwen3-0.6B"
  }

  architecture = "qwen3"
  modalities = [
    "text"
  ]

  artifact {
    format = "safetensors"
    quantization = "f16"
  }

  context_length = 4096
  n_layers = 28
  n_kv_heads = 8
  head_dim = 128
}

resource "hardware_profile" "gcp_cloudrun_l4" {
  provider = "gcp"
  region = "europe-west4"

  accelerator {
    kind = "gpu"
    model = "l4"
    count = 1
    memory = "24GiB"
  }

  # Cloud Run's minimum for an L4 instance (4 vCPU, 16 GiB), which is what
  # apply asks for.
  memory = "16GiB"

  gcp {
    project = "your-gcp-project-id"
    backend = "cloud_run"

    # The account the service runs as. It reads the API key and the Hugging
    # Face token from Secret Manager; amicu grants it access to those secrets
    # only. Create it first (see the GCP provider docs) and replace the
    # placeholder.
    service_account = "amicu-run@your-gcp-project-id.iam.gserviceaccount.com"
  }
}

resource "deployment" "chat" {
  model = model.qwen.id
  hardware = hardware_profile.gcp_cloudrun_l4.id
  runtime = "vllm"

  vllm {
    context_length = 4096
    port = 8000
  }

  # At most one billed L4 at a time, and none while idle.
  scaling {
    min_replicas = 0
    max_replicas = 1
  }

  api_key = secret("CHAT_API_KEY")
}
