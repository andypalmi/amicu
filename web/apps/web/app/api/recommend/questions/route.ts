import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';

// BFF: GET /api/recommend/questions?preset=<id> -> Management API GET
// /recommend/questions?preset=<id>.
// Session-guarded; attaches the bearer token server-side and mirrors the
// upstream status + JSON (200 QuestionDto[], 401/403 auth, 404 unknown preset).
export async function GET(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const preset = new URL(request.url).searchParams.get('preset');
  if (!preset) {
    return Response.json({ error: '`preset` query parameter is required' }, { status: 400 });
  }

  try {
    const { data, error, response } = await engineClient().GET('/recommend/questions', {
      params: { query: { preset } },
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, {
      route: 'GET /api/recommend/questions',
      op: 'recommend.questions',
    });
  }
}
