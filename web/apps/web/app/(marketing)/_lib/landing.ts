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
    title: 'Declare the stack',
    body: 'Write the model, the hardware it runs on and the serving runtime as HCL resources. Start from a scaffold, or answer a few questions in the web console (context length, concurrency, budget, latency, EU residency) and get ranked candidate stacks written out as HCL.',
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
    title: 'Check it and read the plan',
    body: 'Before anything starts, amicu checks references, whether the model fits the memory you gave it, and every residency and governance rule. The plan then shows each change against recorded state, with the exact launch command it will run.',
    transcript: HERO_PLAN,
  },
  {
    id: 'apply',
    title: 'Apply with approval',
    body: 'Apply shows the plan again and waits for a yes. It then starts llama.cpp or vLLM as a launchd or systemd service on your machine, or as an app on Modal’s serverless GPUs, and records the endpoint in state.',
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
    id: 'observe',
    title: 'Watch it run',
    body: 'Check status, logs and metrics from the CLI. Give the tuning engine a metrics snapshot and it compares measured throughput and latency with the plan’s estimates, then suggests changes with the evidence behind each one. Suggestions are advisory: nothing changes until you edit the file and apply.',
    transcript: [
      { text: '$ amicu modal status amicu-chat', tone: 'cmd' },
      { text: 'app amicu-chat (ap-…): deployed', tone: 'muted' },
    ],
  },
];

/**
 * Illustrative chat session against the endpoint `apply` prints. amicu has no
 * chat command; this is any OpenAI-compatible client, and the answer is an
 * example, not recorded model output.
 */
export const CHAT_SESSION: TermLine[] = [
  { text: 'RedHatAI/Qwen3.5-9B-FP8-dynamic', tone: 'cmd' },
  { text: 'https://<workspace>--amicu-chat-server.<region>.modal.direct/v1', tone: 'muted' },
  { text: '' },
  { text: '> Draft a retention notice for our EU customers.' },
  { text: '' },
  { text: 'We keep customer data only in EU regions, and delete it 30 days after you close your account.▍' },
  { text: '' },
  { text: '> ▍', tone: 'muted' },
];

/** Same model resource, two hardware profiles: real launch lines from `plan`. */
export const LAPTOP_LAUNCH =
  'llama-server -m models/starter_chat/qwen3.5-8b-instruct-q4_k_m.gguf --port 8000 --ctx-size 100000 --n-gpu-layers -1 --metrics';
export const GPU_LAUNCH =
  'vllm serve RedHatAI/Qwen3.5-9B-FP8-dynamic --port 8000 --host 0.0.0.0 --max-model-len 200000';

export interface Outcome {
  title: string;
  body: string;
}

/** What you get. Each claim maps to shipped behaviour in the CLI/engine. */
export const OUTCOMES: Outcome[] = [
  {
    title: 'Residency is checked, not promised',
    body: 'Declare that a data class must stay in the EU (or the UK, US, Canada, APAC), or allow and deny specific providers and regions. A deployment that would land outside those rules fails the plan with a named error instead of starting.',
  },
  {
    title: 'No guessing about what will run',
    body: 'The plan prints the launch command, target and region, and an estimate of peak memory. If the model will not fit the hardware you declared, you find out at plan time, not after a cold start.',
  },
  {
    title: 'Reviewable changes',
    body: 'The stack is a text file in your repository. Changes go through code review like anything else, and every apply is a diff against recorded state.',
  },
  {
    title: 'Recommendations that show their working',
    body: 'The wizard ranks candidate stacks against your constraints, and the tuning advice names the bottleneck, the evidence and the alternatives it ruled out.',
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
    value: 'Your own machine, as a launchd or systemd service. Modal serverless GPUs, with region pinning and autoscaling caps.',
  },
  {
    label: 'Residency and pricing data',
    value: 'Region and pricing data for Hetzner, OVHcloud, Scaleway, Nebius, RunPod, Modal, AWS, Azure and GCP feed the residency checks and recommendations. Provisioning adapters for Hetzner, OVHcloud, Scaleway, RunPod and SSH hosts exist but are untested live and not wired into apply; AWS, Azure, GCP and Nebius have none yet.',
  },
  {
    label: 'Models',
    value: 'Open-weight models from Hugging Face, as GGUF or safetensors.',
  },
];
