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
/** The README quickstart (build the CLI, `amicu init`, plan, apply). */
export const quickstartUrl = `${githubUrl}#quickstart`;
/**
 * Contact channel until a company contact address exists (see
 * `lib/legal/company.ts`, all fields still null): the repository's issues.
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
  /** Rendered with visual emphasis. */
  featured?: boolean;
  /** Short bullet summary shown on the tier card. */
  highlights: string[];
}

export const tiers: Tier[] = [
  {
    id: 'community',
    name: 'Community',
    tagline: 'Everything a single operator needs.',
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
    priceLabel: 'Paid',
    description:
      'Everything in Community, plus performance-DB-backed recommendations, performance-DB queries and filtered comparison, and shared team state with locking and roles.',
    cta: { label: 'Sign-ups closed', href: null },
    featured: true,
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
    priceLabel: 'Contact sales',
    description:
      'Everything in Pro, plus org / RBAC / SSO / audit, multi-node distributed serving, advanced routing and failover, and a private performance DB.',
    cta: { label: 'Get in touch', href: contactUrl },
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
    tagline: 'Fully managed, we run the GPUs.',
    priceLabel: 'Talk to us',
    description:
      'Amicu-hosted control plane and managed GPU capacity. Pro and Enterprise capabilities apply as your plan allows, with none of the day-2 infrastructure to run yourself.',
    cta: { label: 'Get in touch', href: contactUrl },
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

export type ProviderStatus = 'supported' | 'preview';

export interface Provider {
  name: string;
  /** Category (preview providers are grouped by it) or a one-line description. */
  kind: string;
  /**
   * `supported`: wired into `amicu apply` (this machine, Modal).
   * `preview`: adapter built and tested against mocks only; not live-tested
   * and not wired into apply.
   */
  status: ProviderStatus;
  /** Residency note for this provider. */
  residency: string;
}

// Verified against crates/amicu-cli/src/commands/apply.rs (dispatch is local
// launchd/systemd or Modal) and the catalogs in crates/amicu-providers.
export const providers: Provider[] = [
  {
    name: 'This machine',
    kind: 'launchd or systemd service',
    status: 'supported',
    residency: 'Wherever the machine is',
  },
  {
    name: 'Modal',
    kind: 'Serverless GPU cloud',
    status: 'supported',
    residency: 'Region pinning, EU included (for example eu-west)',
  },
  { name: 'Hetzner', kind: 'European cloud', status: 'preview', residency: 'Germany, Finland' },
  {
    name: 'OVHcloud',
    kind: 'European cloud',
    status: 'preview',
    residency: 'France, Germany, Poland, plus non-EU regions',
  },
  {
    name: 'Scaleway',
    kind: 'European cloud',
    status: 'preview',
    residency: 'France, Netherlands, Poland',
  },
  {
    name: 'Nebius',
    kind: 'GPU cloud',
    status: 'preview',
    residency: 'EU regions, plus a US region',
  },
  {
    name: 'RunPod',
    kind: 'GPU cloud',
    status: 'preview',
    residency: 'EU and EEA data centers, plus US, Canada and APAC',
  },
  {
    name: 'AWS',
    kind: 'Hyperscaler',
    status: 'preview',
    residency: 'Ireland, Frankfurt, Stockholm, Milan, Paris, plus US and APAC',
  },
  {
    name: 'Azure',
    kind: 'Hyperscaler',
    status: 'preview',
    residency: 'West and North Europe, Germany, Sweden, France, plus UK, US and APAC',
  },
  {
    name: 'GCP',
    kind: 'Hyperscaler',
    status: 'preview',
    residency: 'Belgium, Netherlands, Finland, Frankfurt, Paris, plus US and APAC',
  },
  {
    name: 'Self-hosted over SSH',
    kind: 'Self-hosted',
    status: 'preview',
    residency: 'The location you declare for each host',
  },
];
