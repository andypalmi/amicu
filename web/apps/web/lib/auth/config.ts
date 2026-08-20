import 'server-only';

// Server-only auth/BFF configuration. All values come from the environment with
// sensible dev defaults so the app boots without a running backend. Nothing here
// is exposed to the browser (no NEXT_PUBLIC_*): the browser talks only to the BFF,
// which in turn talks to the Management API / auth issuer.

/** Base URL of the Rust Management API / TASK-078 auth issuer. */
export const API_URL = (process.env.API_URL ?? 'http://localhost:8080').replace(/\/+$/, '');

/** JWKS endpoint used to verify access tokens offline. */
export const AUTH_JWKS_URL = process.env.AUTH_JWKS_URL ?? `${API_URL}/.well-known/jwks.json`;

/** Expected token issuer (`iss`). Defaults to the API origin. */
export const AUTH_ISSUER = process.env.AUTH_ISSUER ?? API_URL;

/** Expected token audience (`aud`). */
export const AUTH_AUDIENCE = process.env.AUTH_AUDIENCE ?? 'amicu';

/** Cookies are marked Secure outside development. */
export const IS_PROD = process.env.NODE_ENV === 'production';

// httpOnly cookie names. BOTH tokens are httpOnly + Secure + SameSite and are held
// by the BFF only — the browser's JS never reads either. The refresh token in
// particular never reaches client code (RFC 0001 §11.6).
export const ACCESS_COOKIE = 'amicu_at';
export const REFRESH_COOKIE = 'amicu_rt';

/** Refresh-cookie lifetime (30 days). The issuer's own revocation is authoritative. */
export const REFRESH_MAX_AGE = 60 * 60 * 24 * 30;
