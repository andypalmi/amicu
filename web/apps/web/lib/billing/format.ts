// Pure, framework-free helpers for the billing account section. Kept free of
// React so they can be unit-tested with `node --test` (see format.test.ts) and
// free of runtime cross-imports so the type-stripped runner (which does not
// resolve the `@/` alias or workspace packages at runtime) can execute them -
// only *type* imports from `@amicu/api-client` are used, and those are erased.
//
// These helpers only present what the API confirms; they never invent a plan,
// price, or entitlement. "Current plan" is whatever the API's billing status
// reports (sourced from the user's real tier), never a client-side guess.

import type { components } from '@amicu/api-client';

export type BillingStatus = components['schemas']['BillingStatusResponse'];
export type RedirectResponse = components['schemas']['RedirectResponse'];

/** Presentational labels for the tier names the API returns. Display only - the
 *  API enforces entitlement; this never grants anything. */
const PLAN_LABELS: Record<string, string> = {
  community: 'Community',
  pro: 'Pro',
  enterprise: 'Enterprise',
  hosted: 'Hosted',
};

/** A human label for a tier/plan name, falling back to the raw value. */
export function planLabel(plan: string): string {
  return PLAN_LABELS[plan] ?? plan;
}

/** A 403 means the caller's tier is insufficient - an upsell moment, distinct
 *  from a 401 (expired session). */
export function isTierDenied(status: number): boolean {
  return status === 403;
}

/**
 * What the UI should offer, given a billing status:
 * - `unavailable` - Stripe isn't configured server-side; show no purchase CTA.
 * - `manage` - an active subscription exists; send the user to the portal.
 * - `upgrade` - configured but no subscription; offer checkout.
 */
export function nextAction(status: BillingStatus): 'unavailable' | 'manage' | 'upgrade' {
  if (!status.configured) return 'unavailable';
  return status.has_subscription ? 'manage' : 'upgrade';
}

/** Whether a well-formed body is a BillingStatus we can render. Guards the shape
 *  before any narrowing so the console never does an unchecked `as` cast. */
export function looksLikeBillingStatus(body: unknown): body is BillingStatus {
  if (!body || typeof body !== 'object') return false;
  const b = body as Record<string, unknown>;
  return (
    typeof b.configured === 'boolean' &&
    typeof b.plan === 'string' &&
    typeof b.has_subscription === 'boolean'
  );
}

/** Whether a body carries a usable Stripe redirect URL (checkout/portal reply). */
export function looksLikeRedirect(body: unknown): body is RedirectResponse {
  if (!body || typeof body !== 'object') return false;
  const b = body as Record<string, unknown>;
  return typeof b.url === 'string' && b.url.length > 0;
}
