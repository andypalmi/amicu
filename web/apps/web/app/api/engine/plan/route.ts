import { engineClient } from '@/lib/engine/client';
import { guardEngine, readConfigBody, upstreamUnreachable } from '@/lib/engine/bff';

// BFF: POST /api/engine/plan → Management API POST /plan.
// Session-guarded; forwards `config` plus an optional `prior` (the engine diffs
// against it) and mirrors the upstream status + JSON (200 Plan, 400 parse error,
// 422 DiagnosticReport for valid-but-unplannable, 401/403 auth).
export async function POST(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readConfigBody(request);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().POST('/plan', {
      body: { config: parsed.config, prior: parsed.prior ?? null },
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch {
    return upstreamUnreachable();
  }
}
