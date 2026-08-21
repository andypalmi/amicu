import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readRecommendBody } from '@/lib/recommend/bff';

// BFF: POST /api/recommend → Management API POST /recommend.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON (200 tier-appropriate TuningReport, 400 malformed body, 401/403
// auth). The API honors tier by content, so a Community caller gets a 200 with an
// empty report rather than a 403.
export async function POST(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readRecommendBody(request);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().POST('/recommend', {
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'POST /api/recommend', op: 'recommend' });
  }
}
