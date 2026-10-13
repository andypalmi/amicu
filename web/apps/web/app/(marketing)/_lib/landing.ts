// Copy and real artifacts for the marketing home page.
//
// Terminal transcripts are real output of `amicu` against the matching HCL,
// trimmed (relative paths, elided detail) but never invented. The residency
// failure uses the exact message formats from crates/amicu-engine/src/checks.rs
// and the `Cannot plan` renderer in crates/amicu-cli/src/commands/plan.rs.
// If the CLI's output format changes, update these strings to match.

/**
 * One line of terminal output. `tone` maps to a semantic token only:
 * `cmd` is the prompt line, `add` a created resource, `muted` secondary detail.
 */
export interface TermLine {
  text: string;
  tone?: 'cmd' | 'add' | 'muted';
}

/**
 * Excerpt of the config that produces the apply output (the `model "qwen"`
 * block, RedHatAI/Qwen3.5-9B-FP8-dynamic, is left out for length). The provider
 * lives on the hardware profile, so it is shown here.
 */
export const DEPLOYMENT_HCL = `resource "hardware_profile" "gpu" {
  provider = "modal"
  region   = "eu-west"
  accelerator {
    kind   = "gpu"
    model  = "l4"
    count  = 1
    memory = "24GiB"
  }
  memory = "32GiB"
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

/** `amicu apply` for the same deployment. */
export const APPLY_OUTPUT: TermLine[] = [
  { text: '$ amicu apply', tone: 'cmd' },
  { text: 'Apply these changes? [y/N] y' },
  { text: 'Applied 1 deployment(s):' },
  { text: '  deployment.chat -> https://<workspace>--amicu-chat-server.<region>.modal.direct', tone: 'muted' },
];

export type DiffTone = 'add' | 'del' | 'ctx';

export interface DiffLine {
  tone: DiffTone;
  text: string;
}

/** Pointing a hardware profile at a laptop or a cloud GPU provider. */
export const PROVIDER_DIFF: DiffLine[] = [
  { tone: 'ctx', text: 'resource "hardware_profile" "main" {' },
  { tone: 'del', text: '  provider = "local"  # your machine' },
  { tone: 'add', text: '  provider = "modal"  # cloud GPU (Modal)' },
  { tone: 'add', text: '  region   = "eu-west"' },
  { tone: 'ctx', text: '  ...' },
  { tone: 'ctx', text: '}' },
];

/** `amicu plan` for the provider switch above (same `Plan:` block as apply). */
export const PROVIDER_PLAN: TermLine[] = [
  { text: '$ amicu plan', tone: 'cmd' },
  { text: 'Plan: 1 action(s).' },
  { text: ' ' },
  { text: '  + deployment.chat', tone: 'add' },
  { text: '      action:   create' },
  { text: '      hardware: gpu.l4' },
  { text: '      target:   modal (region eu-west)' },
];

/** A governance rule that rejects a region outside the EU. */
export const RESIDENCY_HCL = `resource "data_class" "customer" {
  residency = ["eu"]
}

resource "governance_rule" "eu_only" {
  effect          = "allow"
  allowed_regions = ["eu", "eu-west"]
}

resource "hardware_profile" "gpu" {
  provider = "modal"
  region   = "us-east"
}
`;

/** `amicu plan` when the hardware profile names `us-east`. */
export const RESIDENCY_PLAN: TermLine[] = [
  { text: '$ amicu plan', tone: 'cmd' },
  { text: 'Cannot plan: configuration has errors.' },
  { text: "  error [AMICU-POLICY-002] region 'us-east' (us) violates the data residency allowlist" },
  { text: "  error [AMICU-POLICY-002] region 'us-east' violates the allowed-regions list" },
];

export interface FeatureLink {
  label: string;
  href: string;
}

export const CONFIG_LINKS: FeatureLink[] = [
  { label: 'Configuration', href: '/docs/configuration' },
  { label: 'Hardware profiles', href: '/docs/configuration/hardware-profile' },
  { label: 'Plan and apply', href: '/docs/concepts/plan-apply-state' },
  { label: 'CLI reference', href: '/docs/reference/cli' },
];

export const PROVIDER_SWITCH_LINKS: FeatureLink[] = [
  { label: 'Local', href: '/docs/providers/local' },
  { label: 'Providers', href: '/docs/providers' },
  { label: 'Deployments', href: '/docs/configuration/deployment' },
];

export const POLICY_LINKS: FeatureLink[] = [
  { label: 'Residency', href: '/docs/concepts/residency' },
  { label: 'Governance rules', href: '/docs/configuration/governance-rule' },
  { label: 'Endpoint auth', href: '/docs/security/endpoint-auth' },
  { label: 'Secrets', href: '/docs/security/secrets' },
];

export interface Tile {
  title: string;
  body: string;
  /** Mono line-art graphic, one string per line. */
  graphic: string[];
  href: string;
}

/** Shipped features only; each maps to a docs page. */
export const TILES: Tile[] = [
  {
    title: 'Endpoint auth, tested',
    body: 'After apply, amicu checks the endpoint rejects anonymous requests.',
    graphic: ['GET /v1/models', '  no key    401', '  with key  200'],
    href: '/docs/security/endpoint-auth',
  },
  {
    title: 'Agent skills',
    body: 'Teach your coding agent the amicu workflow and HCL schema.',
    graphic: ['$ amicu skill export \\', '    --target claude-code'],
    href: '/docs/reference/agent-skills',
  },
  {
    title: 'Health checks',
    body: 'Check the host, config and state directory in one command.',
    graphic: ['$ amicu doctor', '$ amicu doctor --output json'],
    href: '/docs/operate/monitoring',
  },
];
