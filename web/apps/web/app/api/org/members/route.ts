import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readAddMemberBody } from '@/lib/org/bff';

// BFF: /api/org/members → Management API /org/members.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON. The Enterprise tier gate and every RBAC rule live in the API -
// the browser never sees the token or the API base.

// GET - list the organization's members and the caller's own role.
export async function GET(): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  try {
    const { data, error, response } = await engineClient().GET('/org/members', {
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'GET /api/org/members', op: 'org.members.list' });
  }
}

// POST - invite/add a member with a role (200/201, 400 bad input, 403 tier/RBAC,
// 409 already a member).
export async function POST(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readAddMemberBody(request);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().POST('/org/members', {
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'POST /api/org/members', op: 'org.members.create' });
  }
}
