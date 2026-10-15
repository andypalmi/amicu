# Mac (Apple Silicon)

llama.cpp with Metal offload, Qwen3.5 27B Q4_K_M at the full 262144-token context, output capped at 65536 tokens. Generated for a 48 GiB M4 Pro; `amicu init --example mac` sizes it to your own machine. Download the GGUF named in `amicu.hcl` into `models/<model id>/`, then run `amicu validate`, `amicu plan`, `amicu apply`.
