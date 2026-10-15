// A small, known-valid & plannable amicu HCL config used to seed the editor so
// the console is non-empty (and Validate/Plan succeed) on first load. Mirrors the
// `PLANNABLE` fixture in crates/amicu-api/tests/engine.rs: a llama_cpp deployment
// that fits its Apple-silicon hardware.
export const EXAMPLE_CONFIG = `resource "model" "m" {
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
    file         = "m-q4_k_m.gguf"
    size         = "5680522464B"
  }
}

resource "hardware_profile" "hw" {
  provider = "local_apple"
  accelerator {
    kind   = "apple_silicon"
    model  = "m4_pro"
    count  = 1
    memory = "48GiB"
  }
  memory = "48GiB"
}

resource "deployment" "d" {
  model    = model.m.id
  hardware = hardware_profile.hw.id
  runtime  = "llama_cpp"
  llama_cpp {
    context_length = 8192
    port           = 8000
    n_gpu_layers   = -1
  }
}
`;
