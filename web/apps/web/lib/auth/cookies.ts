import 'server-only';
import { cookies } from 'next/headers';
import type { TokenResponse } from '@amicu/auth/server';
import {
  ACCESS_COOKIE,
  REFRESH_COOKIE,
  REFRESH_MAX_AGE,
  COOKIE_SECURE,
  LEGACY_COOKIES,
} from './config';

// Shared cookie attributes. httpOnly keeps both tokens out of browser JS; SameSite
// Lax is enough for a first-party BFF (top-level navigations carry the cookie).
// Secure is an explicit flag (COOKIE_SECURE, see server-env.ts) so local HTTP dev
// works while every other environment defaults to Secure; any real deployment
// must run behind TLS regardless.
const baseAttrs = {
  httpOnly: true,
  secure: COOKIE_SECURE,
  sameSite: 'lax',
  path: '/',
} as const;

/**
 * Persist a fresh token bundle into the two httpOnly cookies. Callable from server
 * actions and route handlers only (where `cookies()` is writable).
 */
export async function setSessionCookies(tokens: TokenResponse): Promise<void> {
  const store = await cookies();
  // Access cookie mirrors the token's own short lifetime; the JWT `exp` remains the
  // real authority (the cookie just avoids sending a known-dead token upstream).
  store.set(ACCESS_COOKIE, tokens.access_token, {
    ...baseAttrs,
    maxAge: tokens.expires_in ?? 60 * 15,
  });
  store.set(REFRESH_COOKIE, tokens.refresh_token, {
    ...baseAttrs,
    maxAge: REFRESH_MAX_AGE,
  });
  await expireLegacy(store);
}

type CookieStore = Awaited<ReturnType<typeof cookies>>;

// Expire by re-setting with the same attributes + maxAge 0: a `__Host-` cookie can
// only be modified by a Secure, Path=/ Set-Cookie, which `store.delete()` omits.
function expire(store: CookieStore, name: string): void {
  store.set(name, '', { ...baseAttrs, maxAge: 0 });
}

async function expireLegacy(store: CookieStore): Promise<void> {
  for (const name of LEGACY_COOKIES) expire(store, name);
}

/** Clear both session cookies (logout / hard failure). */
export async function clearSessionCookies(): Promise<void> {
  const store = await cookies();
  expire(store, ACCESS_COOKIE);
  expire(store, REFRESH_COOKIE);
  await expireLegacy(store);
}

/** Read the raw refresh token from the request cookies, if present. */
export async function readRefreshToken(): Promise<string | undefined> {
  const store = await cookies();
  return store.get(REFRESH_COOKIE)?.value;
}

/** Read the raw access token from the request cookies, if present. */
export async function readAccessToken(): Promise<string | undefined> {
  const store = await cookies();
  return store.get(ACCESS_COOKIE)?.value;
}
