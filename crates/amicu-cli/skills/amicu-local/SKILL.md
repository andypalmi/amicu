---
name: amicu-local
description: Use for local, on-device inference with Amicu - serving GGUF models via llama.cpp on Apple Silicon or CPU, choosing GPU-offload and context settings, and running deployments as launchd (macOS) or systemd (Linux) user services. Triggers on llama.cpp, GGUF, Apple Silicon, Metal, `local_apple`/`local_cpu`, or "run it on my machine".
---

# Amicu Local

Local deployments use the `llama_cpp` runtime to serve GGUF models on Apple
Silicon or CPU, supervised as a per-user service (launchd on macOS, systemd on
Linux). This is the default path for development and on-device workloads.

## When to use local

- Apple Silicon (`hardware_profile` with `accelerator.kind = "apple_silicon"`)
  or CPU-only (`provider = "local_cpu"`).
- GGUF artifacts (`artifact.format = "gguf"`) with a chosen quantization.
- Privacy-sensitive data that must stay on-device.

For GPU clusters, Kubernetes, or vLLM, use `amicu-production` instead.

## Deployment block

```hcl
resource "deployment" "support_local" {
  model    = model.support_chat.id
  hardware = hardware_profile.dev_mac.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1   # -1 = offload all layers; 0 = CPU-only
  }
}
```

## Workflow

```bash
amicu validate --output json   # confirms the GGUF fits accelerator/host memory
amicu plan --output json       # shows the llama-server the apply will start
amicu apply                    # installs + starts the launchd/systemd service
amicu doctor --output json     # verifies llama-server binary + service health
```

Apply spawns `llama-server` (or reuses a matching running one) and registers it
with the host supervisor; `doctor` reports whether the binary is discoverable.

## Choosing settings

- `n_gpu_layers = -1` on Apple Silicon uses Metal for all layers; drop toward
  `0` only if memory is tight.
- Keep `context_length` at or below the model's `context_length`; larger
  contexts cost more KV-cache memory.
- Quantization trades quality for size - see
  `references/gguf-quantization-guide.md`.

## Safety

- Ports bind locally (`--host 127.0.0.1`); do not expose a local deployment publicly without intent.
- A local deployment may add `api_key = secret("NAME")` so other local processes need a bearer key too. `apply` reads it from the environment variable `NAME` and gives it to the service through its private (0600) environment file or plist as `LLAMA_API_KEY`, never argv, then verifies the endpoint rejects anonymous requests.
- Never commit downloaded model files or secrets into the repo.

## References

- `references/llama-server-args.md` - how deployment fields map to CLI args.
- `references/gguf-quantization-guide.md` - quantization vs size/quality.
