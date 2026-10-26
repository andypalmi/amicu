# Reference machine: no local hardware needed.
# Qwen3-0.6B in fp16 on one T4 Spot VM (n1-standard-4) in europe-west4, deleted
# by Compute Engine after two hours whatever else happens.

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

resource "hardware_profile" "gcp_t4" {
  provider = "gcp"
  region = "europe-west4"

  accelerator {
    kind = "gpu"
    model = "t4"
    count = 1
    memory = "16GiB"
  }

  memory = "15GiB"

  gcp {
    project = "your-gcp-project-id"
    backend = "compute"
    machine_type = "n1-standard-4"
    provisioning = "spot"
    max_run_duration = "2h"

    # Optional: power the VM off after 30 minutes without a request. The VM is
    # kept (powered off) until the next `amicu apply` or `amicu destroy`
    # deletes it. Leave it out to keep serving until max_run_duration.
    idle_shutdown = "30m"

    # The account the VM runs as. It reads the API key, the Hugging Face token
    # and the TLS key from Secret Manager at boot; amicu grants it access to
    # those secrets only. Create it first (see the GCP provider docs) and replace
    # the placeholder. Setting `insecure_metadata_secrets = true` instead puts the
    # secrets in instance metadata, readable by anyone who can read the VM: do
    # not use it for anything you care about.
    service_account = "amicu-vm@your-gcp-project-id.iam.gserviceaccount.com"
  }
}

resource "deployment" "chat" {
  model = model.qwen.id
  hardware = hardware_profile.gcp_t4.id
  runtime = "vllm"

  vllm {
    context_length = 4096
    port = 8000
  }

  api_key = secret("CHAT_API_KEY")
}
