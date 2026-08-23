import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readVersionNo } from '@/lib/configs/bff';

// BFF: GET /api/configs/{id}/versions/{no} -> Management API
// GET /configs/{id}/versions/{no}.
// Session-guarded; forwards the bearer token and mirrors the upstream status +
// JSON (200 VersionContentDto, 400 malformed `no`, 401/403 auth, 404 unknown
// config or version).
type Ctx = { params: Promise<{ id: string; no: string }> };

export async function GET(_request: Request, ctx: Ctx): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const { id, no: rawNo } = await ctx.params;
  const parsed = readVersionNo(rawNo);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().GET('/configs/{id}/versions/{no}', {
      params: { path: { id, no: parsed.no } },
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, {
      route: 'GET /api/configs/{id}/versions/{no}',
      op: 'configs.versions.get',
    });
  }
}
