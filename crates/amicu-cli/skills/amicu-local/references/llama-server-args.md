# llama-server argument mapping

Amicu builds the `llama-server` argv from the `llama_cpp {}` block. You do not
pass these by hand; this table explains what each field becomes so you can
reason about behavior and diagnostics.

| HCL field | llama-server arg | Notes |
|---|---|---|
| `variant.file` (from the model) | `-m <path>` | Resolved GGUF file path. |
| `port` | `--port <n>` | Local bind port. |
| `context_length` | `--ctx-size <n>` | KV-cache sizing; ≤ model context. |
| `n_gpu_layers` | `--n-gpu-layers <n>` | `-1` = all layers, `0` = CPU-only. |

## Service supervision

- macOS: a launchd user agent, loaded via `launchctl load -w`.
- Linux: a systemd `--user` unit; status via `systemctl --user is-active`.

`amicu doctor` runs `llama-server --version` to confirm the binary is on PATH.
If it is missing, install llama.cpp (`brew install llama.cpp` on macOS) and
re-run doctor.

## Common issues

- Model fails to load / OOM: lower `n_gpu_layers`, use a smaller quantization,
  or reduce `context_length`.
- Port already in use: change `port` and re-plan.
