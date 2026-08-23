import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readAppendVersionBody } from '@/lib/configs/bff';

// BFF: /api/configs/{id}/versions -> Management API /configs/{id}/versions.
// Session-guarded; forwards the bearer token and mirrors the upstream status +
// JSON.
type Ctx = { params: Promise<{ id: string }> };

// GET - list every version, oldest first (200 VersionMetaDto[], empty for an
// unknown config id, 401/403 auth).
export async function GET(_request: Request, ctx: Ctx): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const { id } = await ctx.params;
  try {
    const { data, error, response } = await engineClient().GET('/configs/{id}/versions', {
      params: { path: { id } },
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, {
      route: 'GET /api/configs/{id}/versions',
      op: 'configs.versions.list',
    });
  }
}

// POST - append a new version (201 VersionMetaDto, 400 HCL parse error, 401/403
// auth, 404 unknown config id).
export async function POST(request: Request, ctx: Ctx): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readAppendVersionBody(request);
  if ('deny' in parsed) return parsed.deny;

  const { id } = await ctx.params;
  try {
    const { data, error, response } = await engineClient().POST('/configs/{id}/versions', {
      params: { path: { id } },
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, {
      route: 'POST /api/configs/{id}/versions',
      op: 'configs.versions.append',
    });
  }
}
