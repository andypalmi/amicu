# Linux, CPU only

llama.cpp on the CPU, Qwen3.5 4B Q4_K_M at the full 262144-token context, output capped at 65536 tokens. Generated for a 16 GiB machine; `amicu init --example linux-cpu` sizes it to your own. Download the GGUF named in `amicu.hcl` into `models/<model id>/`, then run `amicu validate`, `amicu plan`, `amicu apply`.
