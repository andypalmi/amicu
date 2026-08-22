import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readWizardAnswersBody } from '@/lib/wizard/bff';

// BFF: POST /api/presets/{id}/emit -> Management API POST /presets/{id}/emit.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON (200 EmitResponseDto, 400 out-of-range answer / malformed body,
// 401/403 auth, 404 unknown id).
type Ctx = { params: Promise<{ id: string }> };

export async function POST(request: Request, ctx: Ctx): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readWizardAnswersBody(request);
  if ('deny' in parsed) return parsed.deny;

  const { id } = await ctx.params;
  try {
    const { data, error, response } = await engineClient().POST('/presets/{id}/emit', {
      params: { path: { id } },
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'POST /api/presets/{id}/emit', op: 'presets.emit' });
  }
}
