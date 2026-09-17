# Resource types

Every resource is declared as `resource "<kind>" "<name>" { ... }`. Reference
another resource by its id: `model.<name>.id`, `hardware_profile.<name>.id`,
`deployment.<name>.id`.

| Kind | Purpose | Key fields |
|---|---|---|
| `model` | A servable model + its artifact | `source { registry, reference }`, `architecture`, `modalities`, `artifact { format, quantization }`, `context_length`, `variant { quantization, file, size }` |
| `hardware_profile` | Where a deployment runs | `provider`, `region`, `accelerator { kind, model, count, memory }`, `cpu { model, cores }`, `memory`, `price { per_hour }` |
| `deployment` | Binds a model to hardware + runtime | `model`, `hardware`, `runtime` (`llama_cpp`/`vllm`), plus a `llama_cpp {}` or `vllm {}` settings block (both also take an `extra_args` array and an `env {}` block - the raw-flag escape hatch, amicu-planning#150) |
| `routing_policy` | Traffic routing across deployments | `aliases`, `targets { deployment, weight }`, `failover`, `optimize_for` |
| `data_class` | Sensitivity + residency | `sensitivity`, `residency` (jurisdictions: `eu`/`uk`/`us`/`ca`/`apac`), `allow_external_providers`, `handling` |
| `governance_rule` | Allow/deny by policy | `effect`, `data_classes`, `allowed_providers`, `allowed_regions` |
| `budget` | Spend limits | `tenant`, `limit`, `period`, `alert_thresholds` |
| `tenant` | Logical consumer | `display_name`, `quota { max_requests_per_minute, max_concurrent }`, `labels` |
| `use_case` | Requirement descriptor | `modalities`, `min_context_length`, `max_latency_ms`, `required_data_class`, `priority` |

Enums seen in validation: runtimes `llama_cpp`, `vllm`; artifact formats
include `gguf` and `safetensors`; quantizations include `q4_k_m`, `q5_k_m`,
`q6_k`, `q8_0`, `int8`, `int4`, `f16`, `bf16`, `f32`; accelerator kinds include
`apple_silicon` and `nvidia_gpu`.
