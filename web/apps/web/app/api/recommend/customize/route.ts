import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readCustomizeBody } from '@/lib/recommend/bff';

// BFF: POST /api/recommend/customize -> Management API POST /recommend/customize.
// Session-guarded; forwards `hcl` + optional `base_graph` and mirrors the
// upstream status + JSON (200 CustomizeResponseDto, 400 malformed base_graph /
// body, 401/403 auth, 422 CustomizeResponseDto carrying the blocking
// DiagnosticReport). Never synthesizes or reshapes the upstream body - the
// client (<CandidateReview>) renders the report/plan it is handed verbatim.
export async function POST(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readCustomizeBody(request);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().POST('/recommend/customize', {
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, {
      route: 'POST /api/recommend/customize',
      op: 'recommend.customize',
    });
  }
}
