import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';

// BFF: GET /api/org/audit → Management API GET /org/audit.
// Session-guarded; returns the append-only audit log (most-recent first). Viewing
// the log requires the view_audit permission (Owner/Admin) — enforced by the API,
// which answers 403 for a role without it. The token and API base never reach the
// browser.
export async function GET(): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  try {
    const { data, error, response } = await engineClient().GET('/org/audit', {
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch {
    return upstreamUnreachable();
  }
}
