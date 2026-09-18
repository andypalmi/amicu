# Resource types

Every resource is declared as `resource "<kind>" "<name>" { ... }`. Reference
another resource by its id: `model.<name>.id`, `hardware_profile.<name>.id`,
`deployment.<name>.id`.

| Kind | Purpose | Key fields |
|---|---|---|
| `model` | A servable model + its artifact | `source { registry, reference }`, `architecture`, `modalities`, `artifact { format, quantization }`, `context_length`, `variant { quantization, file, size, sha256 }` (`sha256` verifies the resolved download artifact before fetch, amicu-planning#151), optional shape metadata `n_layers`, `n_kv_heads`, `head_dim` (declared, not derived - feeds the KV-cache estimator, amicu-planning#151) |
| `hardware_profile` | Where a deployment runs | `provider`, `region`, `accelerator { kind, model, count, memory }`, `cpu { model, cores }`, `memory`, `price { per_hour }` |
| `deployment` | Binds a model to hardware + runtime | `model`, `hardware`, `runtime` (`llama_cpp`/`vllm`), plus a `llama_cpp {}` or `vllm {}` settings block (both also take an `extra_args` array and an `env {}` block - the raw-flag escape hatch, amicu-planning#150). `llama_cpp {}` additionally takes `temperature`, `top_p`, `top_k`, `min_p`, `repeat_penalty`, `cache_type_k`/`cache_type_v`, `batch_size`, `ubatch_size`, `parallel_slots`, `flash_attention` (amicu-planning#151 PR A), and `chat_template`/`chat_template_file` (mutually exclusive, `AMICU-CAP-005` if both are set), `jinja`, `reasoning_format`, `build` (stored/validated only, not yet rendered - amicu-planning#151 PR B), and a nested `speculative {}` block (amicu-planning#151 PR C: a required `model` *reference to a second model resource* - the draft model, a real graph edge - plus optional `n_max`, `n_min`, `p_min`, `n_gpu_layers`; drafting with the deployment's own model is `AMICU-CAP-007`). See `deployment-examples.md`. `vllm {}` additionally takes `kv_cache_dtype`, `max_num_seqs`, `max_num_batched_tokens`, `enforce_eager` (bool; `true` → `--enforce-eager`/no CUDA graphs, `false` → `--no-enforce-eager`, unset → vLLM's own default - amicu-planning#158; same issue, no sampling-defaults equivalent) |
| `routing_policy` | Traffic routing across deployments | `aliases`, `targets { deployment, weight }`, `failover`, `optimize_for` |
| `data_class` | Sensitivity + residency | `sensitivity`, `residency` (jurisdictions: `eu`/`uk`/`us`/`ca`/`apac`), `allow_external_providers`, `handling` |
| `governance_rule` | Allow/deny by policy | `effect`, `data_classes`, `allowed_providers`, `allowed_regions` |
| `budget` | Spend limits | `tenant`, `limit`, `period`, `alert_thresholds` |
| `tenant` | Logical consumer | `display_name`, `quota { max_requests_per_minute, max_concurrent }`, `labels` |
| `use_case` | Requirement descriptor | `modalities`, `min_context_length`, `max_latency_ms`, `required_data_class`, `priority` |

Enums seen in validation: runtimes `llama_cpp`, `vllm`; artifact formats
include `gguf` and `safetensors`; quantizations include `q4_k_m`, `q5_k_m`,
`q6_k`, `q8_0`, `int8`, `int4`, `f16`, `bf16`, `f32`; accelerator kinds include
`apple_silicon` and `nvidia_gpu`; `cache_type_k`/`cache_type_v` (llama.cpp)
accept `f16`, `bf16`, `q8_0`, `q4_0`, `q4_1`, `q5_0`, `q5_1`, `iq4_nl`;
`flash_attention` accepts `on`, `off`, `auto` (not a boolean); `reasoning_format`
(llama.cpp, amicu-planning#151) accepts `none`, `deepseek`, `deepseek_legacy`,
`auto`; vLLM's `kv_cache_dtype` accepts `auto`, `fp8`, `fp8_e5m2`, `fp8_e4m3` -
a separate, narrower value set from llama.cpp's.

`variant.sha256` (amicu-planning#151) must be exactly 64 hex characters
(case-insensitive on input, normalized to lowercase); `llama_cpp.build` must
be non-empty with no whitespace.

`llama_cpp.speculative.model` (amicu-planning#151 PR C) is a reference
expression (`model.<id>.id`), parsed and checked like the deployment's own
`model` edge: it appears in `Graph::missing_references`, and an id no `model`
declares is `AMICU-REF-001`.
