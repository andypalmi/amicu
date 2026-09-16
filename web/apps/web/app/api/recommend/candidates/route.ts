import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readCandidatesBody } from '@/lib/wizard/bff';

// BFF: POST /api/recommend/candidates -> Management API POST /recommend/candidates.
// Session-guarded; forwards `preset_id` + `answers` and mirrors the upstream
// status + JSON (200 CandidatesResponseDto, 400 out-of-range answer / malformed
// body, 401/403 auth, 404 unknown preset).
export async function POST(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readCandidatesBody(request);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().POST('/recommend/candidates', {
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, {
      route: 'POST /api/recommend/candidates',
      op: 'recommend.candidates',
    });
  }
}
