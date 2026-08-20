// Tier model — mirrors the API tier enum (RFC 0001 §5, TASK-078 `tier` claim).
//
// IMPORTANT: this is presentational/UX logic only. Tier gating in the UI exists
// solely to show/lock/upsell features. It is NEVER an enforcement boundary — the
// Rust Management API re-checks the tier on every mutation and rejects
// unauthorized actions regardless of what the frontend renders. Do not add any
// security-relevant decision that trusts these values.

/** The tier enum as it appears in the verified access-token `tier` claim (lowercase). */
export const TIERS = ['community', 'pro', 'enterprise', 'hosted'] as const;

export type Tier = (typeof TIERS)[number];

/** Human-facing labels for each tier. */
export const TIER_LABELS: Record<Tier, string> = {
  community: 'Community',
  pro: 'Pro',
  enterprise: 'Enterprise',
  hosted: 'Hosted',
};

/**
 * Ordering used purely for "is tier X at least tier Y" comparisons in the UI.
 *
 * Per RFC 0001 §5 the real gating axis is individual-vs-team and is not a strict
 * linear ladder (e.g. Hosted overlaps Enterprise). This rank captures the common
 * "Community < Pro < Enterprise" upsell ladder that most gates use. For features
 * with non-linear access rules, gate on an explicit allow-list of tiers instead
 * (see `tierSatisfies`'s array form).
 */
export const TIER_ORDER: Record<Tier, number> = {
  community: 0,
  pro: 1,
  enterprise: 2,
  hosted: 3,
};

export function tierRank(tier: Tier): number {
  return TIER_ORDER[tier];
}

/** Type guard for values coming off an untrusted claim payload. */
export function isTier(value: unknown): value is Tier {
  return typeof value === 'string' && (TIERS as readonly string[]).includes(value);
}

/**
 * Whether `current` satisfies a gate requiring `required`.
 *
 * - When `required` is a single tier, this is a "at least this tier" ladder check.
 * - When `required` is an array, it is an explicit allow-list (exact membership),
 *   for non-linear gates.
 *
 * Presentational only — never use the result to authorize a real action.
 */
export function tierSatisfies(current: Tier, required: Tier | readonly Tier[]): boolean {
  if (Array.isArray(required)) {
    return (required as readonly Tier[]).includes(current);
  }
  return tierRank(current) >= tierRank(required as Tier);
}
