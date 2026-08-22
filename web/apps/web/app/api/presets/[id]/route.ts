import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';

// BFF: GET /api/presets/{id} -> Management API GET /presets/{id}.
// Session-guarded; forwards the bearer token and mirrors the upstream status +
// JSON (200 PresetDetailDto, 401/403 auth, 404 unknown id).
type Ctx = { params: Promise<{ id: string }> };

export async function GET(_request: Request, ctx: Ctx): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const { id } = await ctx.params;
  try {
    const { data, error, response } = await engineClient().GET('/presets/{id}', {
      params: { path: { id } },
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'GET /api/presets/{id}', op: 'presets.get' });
  }
}
