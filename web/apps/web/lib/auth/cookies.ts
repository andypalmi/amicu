import 'server-only';
import { cookies } from 'next/headers';
import type { TokenResponse } from '@amicu/auth/server';
import {
  ACCESS_COOKIE,
  REFRESH_COOKIE,
  REFRESH_MAX_AGE,
  IS_PROD,
} from './config';

// Shared cookie attributes. httpOnly keeps both tokens out of browser JS; SameSite
// Lax is enough for a first-party BFF (top-level navigations carry the cookie).
// Secure is tied to NODE_ENV so local HTTP dev works; note a non-prod build served
// over plain HTTP would omit Secure - any real deployment must run production mode
// behind TLS (it always does here).
const baseAttrs = {
  httpOnly: true,
  secure: IS_PROD,
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
}

/** Clear both session cookies (logout / hard failure). */
export async function clearSessionCookies(): Promise<void> {
  const store = await cookies();
  store.delete(ACCESS_COOKIE);
  store.delete(REFRESH_COOKIE);
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
