import 'server-only';
import { readJsonBody } from '@/lib/security/body';
import type { CheckoutRequest } from '@amicu/api-client';

// Body validation for the /api/billing/checkout BFF route. Session guarding and
// the upstream-unreachable helper are shared with the engine routes (see
// lib/engine/bff.ts); this only checks the checkout request shape.

/**
 * Parse a JSON body and require a non-empty string `plan` (a tier name the API
 * maps to a Stripe price). Returns the typed body or a 400 Response - mirroring
 * the API's own error shape so the client handles one contract everywhere. The
 * plan→price mapping and its validity are the API's job.
 */
export async function readCheckoutBody(
  request: Request,
): Promise<{ body: CheckoutRequest } | { deny: Response }> {
  const parsed = await readJsonBody(request);
  if ('deny' in parsed) return parsed;
  const raw: unknown = parsed.value;
  const plan = (raw as { plan?: unknown })?.plan;
  if (typeof plan !== 'string' || plan.trim() === '') {
    return { deny: Response.json({ error: '`plan` (string) is required' }, { status: 400 }) };
  }
  return { body: { plan } };
}
