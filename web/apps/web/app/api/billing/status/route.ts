import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';

// BFF: GET /api/billing/status → Management API GET /billing/status.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON ({ configured, plan, has_subscription }). Lets the browser learn
// whether billing is live and the user's real current plan without ever seeing
// the token or the API base.
export async function GET(): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  try {
    const { data, error, response } = await engineClient().GET('/billing/status', {
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'GET /api/billing/status', op: 'billing.status' });
  }
}
