// Pure, framework-free helpers for the organization console. Kept free of React
// (so they can be unit-tested with `node --test`, see format.test.ts) and free of
// runtime cross-imports so the type-stripped runner can execute them — only
// *type* imports from `@amicu/api-client` are used, and those are erased.
//
// These helpers only present what the API confirms; they never invent a member,
// role, permission, or audit entry. The Rust Management API is the sole authority
// on membership and RBAC — the console renders its responses and nothing more.
//
// Deliberately no Date/timestamp formatting here: `new Date` is avoided in this
// testable, potentially-prerendered module. Audit timestamps are rendered in the
// client component (browser runtime) from the raw Unix seconds the API returns.

import type { components } from '@amicu/api-client';

export type MembersResponse = components['schemas']['MembersResponse'];
export type MemberView = components['schemas']['MemberView'];
export type RolesResponse = components['schemas']['RolesResponse'];
export type RoleView = components['schemas']['RoleView'];
export type AuditResponse = components['schemas']['AuditResponse'];
export type AuditEntryView = components['schemas']['AuditEntryView'];

/** The role names the API's RBAC model uses, privilege-descending. */
export const ROLE_NAMES = ['owner', 'admin', 'member', 'viewer'] as const;
export type RoleName = (typeof ROLE_NAMES)[number];

/** Presentational labels for role names. Display only — never an entitlement. */
const ROLE_LABELS: Record<string, string> = {
  owner: 'Owner',
  admin: 'Admin',
  member: 'Member',
  viewer: 'Viewer',
};

/** A human label for a role name, falling back to the raw value. */
export function roleLabel(role: string): string {
  return ROLE_LABELS[role] ?? role;
}

/** Presentational labels for the audit action names the API emits. */
const ACTION_LABELS: Record<string, string> = {
  'org.bootstrapped': 'Organization created',
  'member.added': 'Member added',
  'member.role_changed': 'Role changed',
  'member.removed': 'Member removed',
};

/** A human label for an audit action name, falling back to the raw value. */
export function actionLabel(action: string): string {
  return ACTION_LABELS[action] ?? action;
}

/** A human label for a permission name (`view_audit` → `View audit`). */
export function permissionLabel(permission: string): string {
  const spaced = permission.replace(/_/g, ' ');
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

/** A 403 means the caller may not do this — either their tier or their org role.
 *  The console gates the tier case with <TierGate>; a 403 from an action here is
 *  an RBAC denial shown inline (never an upsell). */
export function isTierDenied(status: number): boolean {
  return status === 403;
}

/** Whether a body is a well-formed MembersResponse we can render. Guards the
 *  shape before any narrowing so the console never does an unchecked `as` cast. */
export function looksLikeMembersResponse(body: unknown): body is MembersResponse {
  if (!body || typeof body !== 'object') return false;
  const b = body as Record<string, unknown>;
  if (typeof b.caller_role !== 'string' || !Array.isArray(b.members)) return false;
  return b.members.every(
    (m) =>
      !!m &&
      typeof m === 'object' &&
      typeof (m as Record<string, unknown>).user_id === 'string' &&
      typeof (m as Record<string, unknown>).role === 'string',
  );
}

/** Whether a body is a well-formed RolesResponse. */
export function looksLikeRolesResponse(body: unknown): body is RolesResponse {
  if (!body || typeof body !== 'object') return false;
  const b = body as Record<string, unknown>;
  if (!Array.isArray(b.roles)) return false;
  return b.roles.every((r) => {
    if (!r || typeof r !== 'object') return false;
    const rr = r as Record<string, unknown>;
    return (
      typeof rr.role === 'string' &&
      typeof rr.description === 'string' &&
      Array.isArray(rr.permissions) &&
      rr.permissions.every((p) => typeof p === 'string')
    );
  });
}

/** Whether a body is a well-formed AuditResponse. */
export function looksLikeAuditResponse(body: unknown): body is AuditResponse {
  if (!body || typeof body !== 'object') return false;
  const b = body as Record<string, unknown>;
  if (!Array.isArray(b.entries)) return false;
  return b.entries.every((e) => {
    if (!e || typeof e !== 'object') return false;
    const ee = e as Record<string, unknown>;
    return (
      typeof ee.id === 'number' &&
      typeof ee.timestamp === 'number' &&
      typeof ee.actor_id === 'string' &&
      typeof ee.action === 'string'
    );
  });
}

/**
 * The roles the caller may assign, given their own role — so the UI offers only
 * valid choices (the API still enforces). Mirrors the server rule: Owners may
 * assign any role; Admins may assign only the non-privileged Member/Viewer;
 * anyone else may assign nothing.
 */
export function assignableRoles(callerRole: string): RoleName[] {
  if (callerRole === 'owner') return ['owner', 'admin', 'member', 'viewer'];
  if (callerRole === 'admin') return ['member', 'viewer'];
  return [];
}

/** Whether the caller's role may manage membership at all (Owner/Admin). */
export function canManageMembers(callerRole: string): boolean {
  return callerRole === 'owner' || callerRole === 'admin';
}

/** Whether the caller's role may view the audit log (Owner/Admin). */
export function canViewAudit(callerRole: string): boolean {
  return callerRole === 'owner' || callerRole === 'admin';
}

/**
 * Whether the caller may modify or remove a member holding `targetRole`. Mirrors
 * the server rule: an Owner may act on anyone; an Admin may act only on
 * non-privileged (Member/Viewer) members; anyone else may act on no one. The API
 * still enforces this — the UI uses it only to disable controls that would 403.
 */
export function canActOnTarget(callerRole: string, targetRole: string): boolean {
  if (callerRole === 'owner') return true;
  if (callerRole === 'admin') return targetRole !== 'owner' && targetRole !== 'admin';
  return false;
}
