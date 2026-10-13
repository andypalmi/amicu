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
  cta: { label: string; href: string };
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
    cta: { label: 'Sign in', href: '/login' },
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

export interface Provider {
  name: string;
  /** Short qualifier shown under the name. */
  kind: string;
  /** Residency note for this provider. */
  residency: string;
  /** True for European-headquartered providers (drives the EU-native badge). */
  euNative?: boolean;
}

// Adapter coverage, provider-agnostic: one declarative model spans EU-native
// providers, the hyperscalers via their regions, and self-hosted/SSH. EU-only is
// one stack you can pin, alongside air-gapped, cost-optimized, or multi-cloud.
export const providers: Provider[] = [
  {
    name: 'Hetzner',
    kind: 'European cloud & dedicated GPU',
    residency: 'Germany, Finland',
    euNative: true,
  },
  {
    name: 'OVHcloud',
    kind: 'European cloud',
    residency: 'France & EU regions',
    euNative: true,
  },
  {
    name: 'Scaleway',
    kind: 'European cloud & GPU',
    residency: 'France, Netherlands, Poland',
    euNative: true,
  },
  {
    name: 'Nebius',
    kind: 'GPU cloud',
    residency: 'EU regions (Finland)',
    euNative: true,
  },
  {
    name: 'RunPod',
    kind: 'GPU cloud',
    residency: 'EU region selection',
  },
  {
    name: 'Modal',
    kind: 'Serverless GPU cloud',
    residency: 'EU region selection (pin to eu / eu-north)',
  },
  {
    name: 'AWS',
    kind: 'Hyperscaler',
    residency: 'EU regions (Frankfurt, Ireland, Paris, Stockholm…)',
  },
  {
    name: 'Azure',
    kind: 'Hyperscaler',
    residency: 'EU regions (West/North Europe, France, Sweden…)',
  },
  {
    name: 'GCP',
    kind: 'Hyperscaler',
    residency: 'EU regions (Belgium, Frankfurt, Finland…)',
  },
  {
    name: 'Self-hosted / SSH',
    kind: 'Your own hardware',
    residency: 'Wherever you run it - full residency control',
  },
];
