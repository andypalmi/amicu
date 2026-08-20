// Access-token claim shapes and the app-facing session view.
//
// The access token is a short-lived (~15 min) asymmetrically-signed JWT issued by
// the TASK-078 auth service and verified offline against its JWKS. See
// `./server` for verification (`verifyAccessToken`). This module holds only the
// client-safe types and a claims→session projection.

import { isTier, type Tier } from './tier';

/** Roles claim — free-form strings assigned by the auth service. */
export type Role = string;

/**
 * The subset of verified access-token claims the web app reads.
 * Standard JWT registered claims plus the Amicu app claims (`tier`, `roles`).
 */
export interface AccessClaims {
  /** Subject — the user id. */
  sub: string;
  /** Amicu tier claim. */
  tier: Tier;
  /** Amicu roles claim. */
  roles: Role[];
  /** Expiry (seconds since epoch). */
  exp: number;
  /** Issued-at (seconds since epoch). */
  iat?: number;
  /** Issuer. */
  iss?: string;
  /** Audience. */
  aud?: string | string[];
  /** Email, when present in the token. */
  email?: string;
}

/**
 * The session object exposed to the app (server helpers + client context).
 * Deliberately excludes the raw token — components never receive it.
 */
export interface SessionUser {
  sub: string;
  tier: Tier;
  roles: Role[];
  email?: string;
}

/** Project verified claims into the app-facing session user. */
export function toSessionUser(claims: AccessClaims): SessionUser {
  return {
    sub: claims.sub,
    tier: claims.tier,
    roles: claims.roles,
    email: claims.email,
  };
}

/**
 * Normalize a raw JWT payload into typed claims, or return null if it is missing
 * required fields / has an unknown tier. Does NOT verify the signature — callers
 * must have verified first (see `verifyAccessToken`).
 */
export function normalizeClaims(payload: Record<string, unknown>): AccessClaims | null {
  const { sub, tier, exp } = payload;
  if (typeof sub !== 'string' || typeof exp !== 'number' || !isTier(tier)) {
    return null;
  }
  const rolesRaw = payload.roles;
  const roles = Array.isArray(rolesRaw) ? rolesRaw.filter((r): r is string => typeof r === 'string') : [];
  return {
    sub,
    tier,
    roles,
    exp,
    iat: typeof payload.iat === 'number' ? payload.iat : undefined,
    iss: typeof payload.iss === 'string' ? payload.iss : undefined,
    aud: (payload.aud as string | string[] | undefined) ?? undefined,
    email: typeof payload.email === 'string' ? payload.email : undefined,
  };
}
