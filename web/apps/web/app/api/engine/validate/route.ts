import { engineClient } from '@/lib/engine/client';
import { guardEngine, readConfigBody, upstreamUnreachable } from '@/lib/engine/bff';

// BFF: POST /api/engine/validate → Management API POST /validate.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON (200 DiagnosticReport, 400 parse error, 401/403 auth).
export async function POST(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readConfigBody(request);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().POST('/validate', {
      body: { config: parsed.config },
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch {
    return upstreamUnreachable();
  }
}
