# Amicu sample configuration exercising all nine resource kinds.
# Comments use `#` or `//`.

resource "model" "support_chat" {
  source {
    registry  = "hugging_face"
    reference = "unsloth/Qwen3.5-9B-GGUF"
  }
  architecture   = "qwen35"
  modalities     = ["text"]
  artifact {
    format       = "gguf"
    quantization = "q4_k_m"
  }
  context_length = 8192
  variant {
    quantization = "q4_k_m"
    file         = "Qwen3.5-9B-Q4_K_M.gguf"
    size         = "5680522464B"
  }
}

resource "hardware_profile" "developer_mac" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m4_pro"
    count  = 1
    memory = "48GiB"
  }
  memory = "48GiB" // unified memory on Apple silicon
}

resource "hardware_profile" "a100_cloud" {
  provider = "aws"
  region   = "us-east-1"
  accelerator {
    kind   = "gpu"
    model  = "a100-sxm4-80gb"
    count  = 2
    memory = "80GiB"
  }
  memory       = "256GiB"
  interconnect = "nvlink"
  price {
    per_hour {
      amount   = 4.5
      currency = "USD"
    }
  }
}

resource "deployment" "support_chat_local" {
  model    = model.support_chat.id
  hardware = hardware_profile.developer_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}

resource "deployment" "support_chat_cloud" {
  model    = model.support_chat.id
  hardware = hardware_profile.a100_cloud.id
  runtime  = "vllm"
  vllm {
    context_length         = 8192
    port                   = 8001
    tensor_parallel_size   = 2
    gpu_memory_utilization = 0.5
  }
}

resource "routing_policy" "support" {
  aliases = ["support_bot", "helpdesk"]
  target {
    deployment = deployment.support_chat_local.id
    weight     = 20
  }
  target {
    deployment = deployment.support_chat_cloud.id
    weight     = 80
  }
  failover {
    enabled                     = true
    health_check_interval_secs  = 30
    max_failures                = 3
  }
  optimize_for = "latency"
}

resource "data_class" "customer_pii" {
  sensitivity              = "restricted"
  residency                = ["eu"]
  allow_external_providers = false
  handling                 = ["no_logging", "encrypt_at_rest"]
}

resource "governance_rule" "eu_residency" {
  description       = "Customer PII must stay on EU infrastructure."
  effect            = "deny"
  data_classes      = [data_class.customer_pii.id]
  allowed_regions   = ["eu-west-1", "eu-central-1"]
}

resource "tenant" "acme" {
  display_name = "ACME Corp"
  quota {
    max_requests_per_minute = 600
    max_concurrent          = 20
  }
  labels {
    cost_center = "cc-42"
    team        = "platform"
  }
}

resource "budget" "acme_monthly" {
  tenant = tenant.acme.id
  limit {
    amount   = 1000.0
    currency = "USD"
  }
  period           = "monthly"
  alert_thresholds = [0.5, 0.75]
}

resource "use_case" "support_desk" {
  description         = "Interactive customer support chat."
  modalities          = ["text"]
  min_context_length  = 4096
  max_latency_ms      = 2000
  required_data_class = data_class.customer_pii.id
  priority            = "high"
}
