import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readDiffQuery } from '@/lib/configs/bff';

// BFF: GET /api/configs/{id}/diff -> Management API GET /configs/{id}/diff.
// Session-guarded; forwards the `from`/`to` query params and the bearer token,
// mirroring the upstream status + JSON (200 ConfigDiffDto, 400 missing/malformed
// query, 401/403 auth, 404 unknown config or version).
type Ctx = { params: Promise<{ id: string }> };

export async function GET(request: Request, ctx: Ctx): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const query = readDiffQuery(request);
  if ('deny' in query) return query.deny;

  const { id } = await ctx.params;
  try {
    const { data, error, response } = await engineClient().GET('/configs/{id}/diff', {
      params: { path: { id }, query: { from: query.from, to: query.to } },
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'GET /api/configs/{id}/diff', op: 'configs.diff' });
  }
}
