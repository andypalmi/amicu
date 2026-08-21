import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';

// BFF: POST /api/billing/portal → Management API POST /billing/portal.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON (200 { url } to the Stripe Billing Portal, 401 auth, 409 when the
// user has no billing account yet, 503 when billing is not configured). The
// browser only ever receives the hosted Stripe URL, never a secret.
export async function POST(): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  try {
    const { data, error, response } = await engineClient().POST('/billing/portal', {
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'POST /api/billing/portal', op: 'billing.portal' });
  }
}
