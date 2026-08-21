import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';

// BFF: GET /api/org/roles → Management API GET /org/roles.
// Session-guarded; returns the RBAC catalog (roles + their permissions) so the
// console can render the real permission model. The token and API base never
// reach the browser.
export async function GET(): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  try {
    const { data, error, response } = await engineClient().GET('/org/roles', {
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'GET /api/org/roles', op: 'org.roles' });
  }
}
