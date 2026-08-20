import 'server-only';
import type { AddMemberRequest, ChangeRoleRequest } from '@amicu/api-client';

// Body validation for the /api/org/* BFF routes. Session guarding and the
// upstream-unreachable helper are shared with the engine routes (see
// lib/engine/bff.ts); this only checks request shapes. Role validity and every
// RBAC rule are the Management API's job — the BFF forwards, it does not decide.

/**
 * Parse a JSON body for `POST /api/org/members`: require a non-empty string
 * `user_id` and a non-empty string `role`. Returns the typed body or a 400
 * Response mirroring the API's own error shape.
 */
export async function readAddMemberBody(
  request: Request,
): Promise<{ body: AddMemberRequest } | { deny: Response }> {
  let raw: unknown;
  try {
    raw = await request.json();
  } catch {
    return { deny: Response.json({ error: 'request body must be JSON' }, { status: 400 }) };
  }
  const userId = (raw as { user_id?: unknown })?.user_id;
  if (typeof userId !== 'string' || userId.trim() === '') {
    return { deny: Response.json({ error: '`user_id` (string) is required' }, { status: 400 }) };
  }
  const role = (raw as { role?: unknown })?.role;
  if (typeof role !== 'string' || role.trim() === '') {
    return { deny: Response.json({ error: '`role` (string) is required' }, { status: 400 }) };
  }
  return { body: { user_id: userId, role } };
}

/**
 * Parse a JSON body for `PATCH /api/org/members/{id}`: require a non-empty string
 * `role`. Returns the typed body or a 400 Response.
 */
export async function readChangeRoleBody(
  request: Request,
): Promise<{ body: ChangeRoleRequest } | { deny: Response }> {
  let raw: unknown;
  try {
    raw = await request.json();
  } catch {
    return { deny: Response.json({ error: 'request body must be JSON' }, { status: 400 }) };
  }
  const role = (raw as { role?: unknown })?.role;
  if (typeof role !== 'string' || role.trim() === '') {
    return { deny: Response.json({ error: '`role` (string) is required' }, { status: 400 }) };
  }
  return { body: { role } };
}
