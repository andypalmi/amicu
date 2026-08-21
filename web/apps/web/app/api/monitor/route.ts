import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readMonitorBody } from '@/lib/monitor/bff';

// BFF: POST /api/monitor → Management API POST /monitor.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON (200 normalized metrics + estimate + tier-appropriate advisory
// suggestions, 400 malformed body, 401/403 auth). The API honors tier by content,
// so a Community caller gets a 200 with the metric surface and an empty suggestion
// list rather than a 403.
export async function POST(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readMonitorBody(request);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().POST('/monitor', {
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'POST /api/monitor', op: 'monitor' });
  }
}
