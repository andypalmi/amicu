import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readChangeRoleBody } from '@/lib/org/bff';

// BFF: /api/org/members/{id} → Management API /org/members/{id}.
// Session-guarded; forwards the bearer token and mirrors the upstream status +
// JSON. All RBAC (who may change/remove whom, the last-Owner invariant) is
// enforced by the API; the BFF only forwards.

type Ctx = { params: Promise<{ id: string }> };

// PATCH - change a member's role (200, 400 bad role, 403 tier/RBAC, 404 unknown,
// 409 last-Owner invariant).
export async function PATCH(request: Request, ctx: Ctx): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readChangeRoleBody(request);
  if ('deny' in parsed) return parsed.deny;

  const { id } = await ctx.params;
  try {
    const { data, error, response } = await engineClient().PATCH('/org/members/{id}', {
      params: { path: { id } },
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'PATCH /api/org/members/{id}', op: 'org.members.update' });
  }
}

// DELETE - remove a member (200, 403 tier/RBAC, 404 unknown, 409 last-Owner).
export async function DELETE(_request: Request, ctx: Ctx): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const { id } = await ctx.params;
  try {
    const { data, error, response } = await engineClient().DELETE('/org/members/{id}', {
      params: { path: { id } },
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'DELETE /api/org/members/{id}', op: 'org.members.delete' });
  }
}
