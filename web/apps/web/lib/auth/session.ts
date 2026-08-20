import 'server-only';
import { redirect } from 'next/navigation';
import { verifyAccessToken } from '@amicu/auth/server';
import { toSessionUser, type SessionUser } from '@amicu/auth';
import { AUTH_JWKS_URL, AUTH_ISSUER, AUTH_AUDIENCE } from './config';
import { readAccessToken } from './cookies';

/**
 * Resolve the current session by verifying the access-token cookie offline against
 * the issuer JWKS (signature + exp + iss + aud). Returns the app-facing session
 * user (never the raw token) or null when unauthenticated / expired.
 *
 * Read-only and RSC-safe: it does NOT rotate cookies. Proactive silent refresh
 * (which rotates the refresh token) happens in the BFF middleware — see proxy.ts —
 * so by the time a page renders, the access cookie is already fresh.
 */
export async function getSession(): Promise<SessionUser | null> {
  const token = await readAccessToken();
  if (!token) return null;
  const claims = await verifyAccessToken(token, {
    jwksUrl: AUTH_JWKS_URL,
    issuer: AUTH_ISSUER,
    audience: AUTH_AUDIENCE,
  });
  return claims ? toSessionUser(claims) : null;
}

/** Require a session or redirect to /login (optionally preserving a return path). */
export async function requireSession(returnTo?: string): Promise<SessionUser> {
  const session = await getSession();
  if (!session) {
    redirect(returnTo ? `/login?next=${encodeURIComponent(returnTo)}` : '/login');
  }
  return session;
}
