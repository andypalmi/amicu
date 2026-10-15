# Linux with an NVIDIA GPU

llama.cpp with every layer on the GPU, Qwen3 8B Q4_K_M. Generated for an RTX 4090 (24 GiB); `amicu init --example linux-cuda` sizes it to your own GPU. Needs a CUDA build of `llama-server`. Download the GGUF named in `amicu.hcl` into `models/<model id>/`, then run `amicu validate`, `amicu plan`, `amicu apply`.
