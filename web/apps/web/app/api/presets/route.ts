import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';

// BFF: GET /api/presets -> Management API GET /presets.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON (200 PresetSummaryDto[], 401/403 auth). The catalog is a fixed,
// Community-minimum list - the API honors tier by content.
export async function GET(): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  try {
    const { data, error, response } = await engineClient().GET('/presets', {
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'GET /api/presets', op: 'presets.list' });
  }
}
