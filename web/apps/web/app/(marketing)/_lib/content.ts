// Marketing content model.
//
// Tier names mirror the source of truth in `crates/amicu-auth` (Tier enum:
// Community / Pro / Enterprise / Hosted). The gating matrix mirrors RFC 0001
// §5 "Tier Gating Map". Enforcement lives in the API - this surface only
// mirrors it for UX. Pricing numbers are intentionally omitted (RFC §1
// non-goal): tiers carry qualitative positioning and a CTA, not a price.

import { gitConfig } from '@/lib/shared';

/** Public repository URL. */
export const githubUrl = `https://github.com/${gitConfig.user}/${gitConfig.repo}`;
/** The docs quickstart (install the CLI, `amicu init`, plan, apply). */
export const quickstartUrl = '/docs/quickstart';
/**
 * Issue tracker, used as a public support channel (general contact is
 * `company.contactEmail` in `lib/legal/company.ts`).
 */
export const contactUrl = `${githubUrl}/issues`;

export type TierId = 'community' | 'pro' | 'enterprise' | 'hosted';

export interface Tier {
  id: TierId;
  name: string;
  /** One-line positioning on the individual-vs-team axis. */
  tagline: string;
  /** Neutral price label - no invented numbers (RFC §1 non-goal). */
  priceLabel: string;
  description: string;
  /** `href: null` renders a disabled button (not purchasable yet). */
  cta: { label: string; href: string | null };
  /** Short bullet summary shown on the tier card. */
  highlights: string[];
}

export const tiers: Tier[] = [
  {
    id: 'community',
    name: 'Community',
    tagline: 'Everything one operator needs.',
    priceLabel: 'Free · AGPL-3.0',
    description:
      'Author and validate HCL, run local llama.cpp and single-node vLLM, use the guided wizard, and keep solo remote state against a self-hosted backend.',
    cta: { label: 'Get started', href: quickstartUrl },
    highlights: [
      'HCL editor, validate, local plan / apply',
      'Guided wizard (preset-based recommendations)',
      'Multi-GPU, single node',
      'Solo remote state (self-hosted backend)',
    ],
  },
  {
    id: 'pro',
    name: 'Pro',
    tagline: 'Advanced recommendations + team collaboration.',
    priceLabel: 'Planned',
    description:
      'Everything in Community, plus performance-DB-backed recommendations, performance-DB queries and filtered comparison, and shared team state with locking and roles.',
    cta: { label: 'Contact us', href: contactUrl },
    highlights: [
      'Advanced perf-DB-backed recommendations',
      'Performance-DB query + filtered comparison',
      'Team state + collaboration (shared state, locking, roles)',
    ],
  },
  {
    id: 'enterprise',
    name: 'Enterprise',
    tagline: 'Governance + production scale.',
    priceLabel: 'Planned',
    description:
      'Everything in Pro, plus org / RBAC / SSO / audit, multi-node distributed serving, advanced routing and failover, and a private performance DB.',
    cta: { label: 'Contact us', href: contactUrl },
    highlights: [
      'Org / RBAC / SSO / audit',
      'Multi-node distributed serving',
      'Advanced routing / multi-provider / failover',
      'Private performance DB',
    ],
  },
  {
    id: 'hosted',
    name: 'Hosted',
    tagline: 'We run the GPUs.',
    priceLabel: 'Planned',
    description:
      'A managed control plane and managed GPU capacity, so you run none of the infrastructure yourself.',
    cta: { label: 'Contact us', href: contactUrl },
    highlights: [
      'Fully managed GPU hosting',
      'Managed control plane',
      'Enterprise capabilities as applicable',
    ],
  },
];

/** Matrix cell state. */
export type Availability = 'yes' | 'locked' | 'asApplicable' | 'no';

export interface FeatureRow {
  feature: string;
  community: Availability;
  pro: Availability;
  enterprise: Availability;
  hosted: Availability;
}

// Verbatim from RFC 0001 §5 Tier Gating Map.
export const featureMatrix: FeatureRow[] = [
  {
    feature: 'HCL editor, validate, local plan / apply',
    community: 'yes',
    pro: 'yes',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Local llama.cpp / single-node vLLM stacks',
    community: 'yes',
    pro: 'yes',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Guided wizard (preset-based recommendations)',
    community: 'yes',
    pro: 'yes',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Multi-GPU, single node',
    community: 'yes',
    pro: 'yes',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Solo remote state (self-hosted backend)',
    community: 'yes',
    pro: 'yes',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Public aggregated performance stats',
    community: 'yes',
    pro: 'yes',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Advanced perf-DB-backed recommendations',
    community: 'locked',
    pro: 'yes',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Performance-DB query + filtered comparison',
    community: 'locked',
    pro: 'yes',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Team state + collaboration (shared state, locking, roles)',
    community: 'locked',
    pro: 'yes',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Advanced routing / multi-provider / failover',
    community: 'locked',
    pro: 'locked',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Multi-node distributed serving',
    community: 'locked',
    pro: 'locked',
    enterprise: 'yes',
    hosted: 'yes',
  },
  {
    feature: 'Private performance DB',
    community: 'locked',
    pro: 'locked',
    enterprise: 'yes',
    hosted: 'asApplicable',
  },
  {
    feature: 'Org / RBAC / SSO / audit',
    community: 'locked',
    pro: 'locked',
    enterprise: 'yes',
    hosted: 'asApplicable',
  },
  {
    feature: 'Fully managed GPU hosting',
    community: 'locked',
    pro: 'locked',
    enterprise: 'locked',
    hosted: 'yes',
  },
];

export type ProviderStatus = 'supported' | 'preview' | 'planned';

export const PROVIDER_STATUS_ORDER: ProviderStatus[] = ['supported', 'preview', 'planned'];

export const PROVIDER_STATUS_LABEL: Record<ProviderStatus, string> = {
  supported: 'Supported',
  preview: 'Preview',
  planned: 'Planned',
};

/** What each status means, shown under its heading on the registry list. */
export const PROVIDER_STATUS_NOTE: Record<ProviderStatus, string> = {
  supported:
    'Wired into amicu apply: this machine (local, local_apple, local_cpu, on_prem) and Modal. Modal is the only one tested end to end.',
  preview: 'An adapter exists and is tested against mocks. Not wired into apply.',
  planned: 'No adapter yet. Region and GPU catalog data only.',
};

export interface Provider {
  /** Docs page: `/docs/providers/<slug>`. */
  slug: string;
  name: string;
  /** Category, shown under the name. */
  kind: string;
  /** One line on what the provider is, for tiles. */
  blurb: string;
  /** Two or three short capability tags, taken from the adapter or apply path. */
  tags: string[];
  /**
   * `supported`: wired into `amicu apply` (this machine, Modal).
   * `preview`: provisioning adapter built (bare machine only), tested against
   * mocks, not live-tested and not wired into apply.
   * `planned`: region and pricing data only; provisioning not built.
   */
  status: ProviderStatus;
  /** Residency note for this provider. */
  residency: string;
}

// Verified against crates/amicu-cli/src/commands/apply.rs (dispatch is local
// launchd/systemd or Modal) and the catalogs in crates/amicu-providers.
export const providers: Provider[] = [
  {
    slug: 'local',
    name: 'This machine',
    kind: 'launchd or systemd service',
    blurb: 'Run llama.cpp or vLLM as a user service on the machine where you invoke amicu.',
    tags: ['macOS and Linux', 'Loopback endpoint', 'Apple Silicon and CPU'],
    status: 'supported',
    residency: 'Wherever the machine is',
  },
  {
    slug: 'modal',
    name: 'Modal',
    kind: 'Serverless GPU cloud',
    blurb: 'Deploy llama.cpp or vLLM to Modal as an autoscaling app.',
    tags: ['Serverless GPU', 'Region pinning', 'Autoscaling'],
    status: 'supported',
    residency: 'Region pinning, EU included (for example eu-west)',
  },
  {
    slug: 'hetzner',
    name: 'Hetzner',
    kind: 'European cloud',
    blurb: 'Dedicated GPU servers in German and Finnish data centers.',
    tags: ['Dedicated servers', 'Monthly billing', 'EU data centers'],
    status: 'preview',
    residency: 'Germany, Finland, plus US and APAC',
  },
  {
    slug: 'ovhcloud',
    name: 'OVHcloud',
    kind: 'European cloud',
    blurb: 'Public Cloud GPU instances from a French sovereign cloud.',
    tags: ['GPU instances', 'On-demand', 'EU data centers'],
    status: 'preview',
    residency: 'France, Germany, Poland, plus non-EU regions',
  },
  {
    slug: 'scaleway',
    name: 'Scaleway',
    kind: 'European cloud',
    blurb: 'Single-GPU instances in French, Dutch and Polish regions.',
    tags: ['GPU instances', 'On-demand', 'EU only'],
    status: 'preview',
    residency: 'France, Netherlands, Poland',
  },
  {
    slug: 'runpod',
    name: 'RunPod',
    kind: 'GPU cloud',
    blurb: 'Provider-managed GPU pods across EU, EEA and worldwide data centers.',
    tags: ['GPU pods', 'On-demand and interruptible', 'EU and EEA'],
    status: 'preview',
    residency: 'EU and EEA data centers, plus US, Canada and APAC',
  },
  {
    slug: 'self-hosted',
    name: 'Self-hosted over SSH',
    kind: 'Self-hosted',
    blurb: 'Hardware you declare and own, reached over SSH.',
    tags: ['Your hardware', 'SSH key auth', 'Declared location'],
    status: 'preview',
    residency: 'The location you declare for each host',
  },
  {
    slug: 'nebius',
    name: 'Nebius',
    kind: 'GPU cloud',
    blurb: 'High-end GPUs in Finland and France, plus a US region.',
    tags: ['High-end GPUs', 'Catalog data only'],
    status: 'planned',
    residency: 'EU regions, plus a US region',
  },
  {
    slug: 'aws',
    name: 'AWS',
    kind: 'Hyperscaler',
    blurb: 'GPU EC2 instances in five EU regions, plus US and APAC.',
    tags: ['GPU EC2', 'Catalog data only'],
    status: 'planned',
    residency: 'Ireland, Frankfurt, Stockholm, Milan, Paris, plus US and APAC',
  },
  {
    slug: 'azure',
    name: 'Azure',
    kind: 'Hyperscaler',
    blurb: 'GPU virtual machines across Europe, the UK, the US and APAC.',
    tags: ['GPU VMs', 'Catalog data only'],
    status: 'planned',
    residency: 'West and North Europe, Germany, Sweden, France, plus UK, US and APAC',
  },
  {
    slug: 'gcp',
    name: 'GCP',
    kind: 'Hyperscaler',
    blurb: 'GPU Compute Engine instances in EU regions, plus US and APAC.',
    tags: ['GPU Compute Engine', 'Catalog data only'],
    status: 'planned',
    residency: 'Belgium, Netherlands, Finland, Frankfurt, Paris, plus US and APAC',
  },
];
