// Copy and real artifacts for the marketing landing page.
//
// Every HCL snippet here validates with the current CLI, and every terminal
// transcript is real output of `amicu` (built from main) run against that
// snippet - trimmed (relative paths, elided plan detail) but never invented.
// If the CLI's output format changes, re-run the commands and update these
// strings rather than editing them by hand.

/** Hero example: one model, served by vLLM on a Modal L4 GPU in eu-west, with a 200k-token context. */
export const HERO_HCL = `resource "model" "qwen" {
  source {
    registry  = "hugging_face"
    reference = "RedHatAI/Qwen3.5-9B-FP8-dynamic"
  }
  architecture = "qwen3"
  artifact {
    format       = "safetensors"
    quantization = "fp8"
  }
}

resource "hardware_profile" "gpu" {
  provider = "modal"
  region   = "eu-west"
  memory   = "32GiB"
  accelerator {
    kind   = "gpu"
    model  = "l4"
    count  = 1
    memory = "24GiB"
  }
}

resource "deployment" "chat" {
  model    = model.qwen.id
  hardware = hardware_profile.gpu.id
  runtime  = "vllm"
  api_key  = secret("CHAT_API_KEY")
  vllm {
    context_length = 200000
    port           = 8000
  }
}
`;

/**
 * One line of terminal output. `tone` maps to a semantic token only:
 * `cmd` is the prompt line, `add` a created resource, `muted` secondary detail.
 */
export interface TermLine {
  text: string;
  tone?: 'cmd' | 'add' | 'muted';
}

/** `amicu plan` against HERO_HCL. */
export const HERO_PLAN: TermLine[] = [
  { text: '$ amicu plan', tone: 'cmd' },
  { text: 'Plan: 1 action(s).' },
  { text: '' },
  { text: '  + deployment.chat', tone: 'add' },
  { text: '      action:   create' },
  { text: '      hardware: gpu.l4' },
  { text: '      target:   modal (region eu-west)' },
  { text: '      api_key:  ****** (secret "CHAT_API_KEY", resolved at apply)' },
  {
    text: '      launch:   vllm serve RedHatAI/Qwen3.5-9B-FP8-dynamic --port 8000 --host 0.0.0.0 --max-model-len 200000',
  },
];

/**
 * A request to the endpoint `apply` prints. The endpoint is OpenAI-compatible
 * and protected by the deployment's `api_key`; Modal endpoints also need the
 * workspace proxy-token headers.
 */
export const CURL_REQUEST: TermLine[] = [
  { text: '$ curl "$ENDPOINT/v1/chat/completions" \\', tone: 'cmd' },
  { text: '    -H "Authorization: Bearer $CHAT_API_KEY" \\' },
  { text: '    -H "Modal-Key: $MODAL_PROXY_TOKEN_ID" \\' },
  { text: '    -H "Modal-Secret: $MODAL_PROXY_TOKEN_SECRET" \\' },
  { text: '    -H "Content-Type: application/json" \\' },
  {
    text: `    -d '{"model": "RedHatAI/Qwen3.5-9B-FP8-dynamic", "messages": [{"role": "user", "content": "Hello"}]}'`,
  },
];

export interface Step {
  /** Anchor-safe id. */
  id: string;
  title: string;
  body: string;
  /** A short, real transcript that shows the step's artifact. */
  transcript: TermLine[];
}

export const STEPS: Step[] = [
  {
    id: 'declare',
    title: 'Write the config',
    body: 'Describe the model, hardware and runtime as HCL, or start from amicu init.',
    transcript: [
      { text: '$ amicu init', tone: 'cmd' },
      { text: '  create amicu.hcl' },
      { text: '  create variables.amicu.hcl' },
      { text: '  create .amicu/' },
      { text: '  create .amicu/config.yaml' },
      { text: '  create .amicu/state/' },
      { text: '  create .amicu/state/.gitkeep' },
      { text: 'Initialized Amicu project (6 item(s) created).', tone: 'muted' },
    ],
  },
  {
    id: 'plan',
    title: 'Read the plan',
    body: 'See every change and the exact launch command before anything starts.',
    transcript: HERO_PLAN,
  },
  {
    id: 'apply',
    title: 'Apply it',
    body: 'Approve the plan and amicu starts the service, locally or on Modal.',
    transcript: [
      { text: '$ amicu apply', tone: 'cmd' },
      { text: 'Plan: 1 action(s).' },
      { text: '' },
      { text: '  + deployment.chat', tone: 'add' },
      { text: '      target:   modal (region eu-west)', tone: 'muted' },
      { text: '' },
      { text: 'Apply these changes? [y/N] y' },
      { text: '' },
      { text: 'Applied 1 deployment(s):' },
      { text: '  deployment.chat -> https://<workspace>--amicu-chat-server.<region>.modal.direct', tone: 'muted' },
    ],
  },
  {
    id: 'call',
    title: 'Call it',
    body: 'Send requests with your API key. Modal endpoints also need proxy-token headers.',
    transcript: CURL_REQUEST,
  },
];

/** Same model resource, two hardware profiles: real launch lines from `plan`. */
export const LAPTOP_LAUNCH =
  'llama-server -m models/qwen3_8b/Qwen3-8B-Q4_K_M.gguf --port 8000 --ctx-size 100000 --n-gpu-layers -1 --metrics';
export const GPU_LAUNCH =
  'vllm serve RedHatAI/Qwen3.5-9B-FP8-dynamic --port 8000 --host 0.0.0.0 --max-model-len 200000';

export interface Outcome {
  title: string;
  body: string;
}

/** What you get. Each claim maps to shipped behaviour in the CLI/engine. */
export const OUTCOMES: Outcome[] = [
  {
    title: 'Checked at plan time',
    body: 'A model that does not fit the hardware, or a deployment outside the regions you allow, fails the plan with a named error.',
  },
  {
    title: 'Reviewable changes',
    body: 'The stack is a text file in your repository. Every apply is a diff against recorded state.',
  },
  {
    title: 'Recommendations with evidence',
    body: 'The wizard ranks candidate stacks against your constraints. Tuning advice names the bottleneck and the evidence.',
  },
];

export interface Support {
  label: string;
  value: string;
}

/** Where it runs today. Keep this strictly to shipped behaviour. */
export const SUPPORT: Support[] = [
  {
    label: 'Runtimes',
    value: 'llama.cpp (GGUF) on Apple Silicon and Linux. vLLM on NVIDIA and AMD GPUs.',
  },
  {
    label: 'Apply targets',
    value: 'Your own machine, as a launchd or systemd service. Modal serverless GPUs, with region pinning.',
  },
  {
    label: 'Models',
    value: 'Open-weight models from Hugging Face, as GGUF or safetensors.',
  },
];
