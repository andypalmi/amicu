import 'server-only';
import type { NextRequest } from 'next/server';
import { decodeExpUnsafe, type TokenResponse } from '@amicu/auth/server';
import { authClient } from './client';
import { ACCESS_COOKIE, REFRESH_COOKIE } from './config';

// Silent refresh, performed in BFF middleware — the one request phase where the
// rotating refresh token can be swapped and written back as cookies (server
// actions/route handlers are the others; RSC render is read-only). Runs only for
// protected routes.
//
// Cheap trigger: decode (do NOT verify) the access token's `exp`. Real trust comes
// later from `getSession()` verifying the signature against the JWKS. If the access
// token is missing or within the skew window, we rotate using the refresh cookie.

const REFRESH_SKEW_SECONDS = 60;

/** Outcome of a refresh attempt, applied to cookies by the caller (proxy.ts). */
export type RefreshOutcome =
  | { kind: 'fresh' } // access token still valid — nothing to do
  | { kind: 'none' } // no session to refresh
  | { kind: 'rotated'; tokens: TokenResponse } // new bundle to write
  | { kind: 'revoked' }; // refresh rejected — clear cookies

function accessFresh(token: string | undefined): boolean {
  if (!token) return false;
  const exp = decodeExpUnsafe(token);
  if (exp === null) return false;
  return exp - REFRESH_SKEW_SECONDS > Math.floor(Date.now() / 1000);
}

/**
 * Decide whether the session needs refreshing and, if so, rotate it. Best-effort:
 * a network failure (e.g. no backend in dev) returns `none` rather than throwing,
 * so the request proceeds and page-level `requireSession` handles redirects.
 */
export async function attemptRefresh(req: NextRequest): Promise<RefreshOutcome> {
  const access = req.cookies.get(ACCESS_COOKIE)?.value;
  if (accessFresh(access)) return { kind: 'fresh' };

  const refresh = req.cookies.get(REFRESH_COOKIE)?.value;
  if (!refresh) return { kind: 'none' };

  const result = await authClient.refresh(refresh);
  if (result.ok) return { kind: 'rotated', tokens: result.data };
  if (result.status === 401 || result.status === 403) return { kind: 'revoked' };
  // Transient/backend-down: leave existing cookies untouched.
  return { kind: 'none' };
}
