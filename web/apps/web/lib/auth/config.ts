import 'server-only';
import { parseServerEnv } from '@/lib/auth/server-env';

// Server-only auth/BFF configuration. All values come from the environment with
// sensible dev defaults so the app boots without a running backend. Nothing here
// is exposed to the browser (no NEXT_PUBLIC_*): the browser talks only to the BFF,
// which in turn talks to the Management API / auth issuer.
//
// Validation is fail-fast: `parseServerEnv` runs at module load and THROWS a clear
// boot-time error if a required variable (API_URL, at minimum) is missing/invalid
// in production. The dev-friendly localhost default is gated to non-production, so
// a prod deploy can no longer silently target localhost. See lib/auth/server-env.ts.
const env = parseServerEnv(process.env);

/** Base URL of the Rust Management API / TASK-078 auth issuer. */
export const API_URL = env.API_URL;

/** JWKS endpoint used to verify access tokens offline. */
export const AUTH_JWKS_URL = env.AUTH_JWKS_URL;

/** Expected token issuer (`iss`). Defaults to the API origin. */
export const AUTH_ISSUER = env.AUTH_ISSUER;

/** Expected token audience (`aud`). */
export const AUTH_AUDIENCE = env.AUTH_AUDIENCE;

/** Cookies are marked Secure outside development. */
export const IS_PROD = process.env.NODE_ENV === 'production';

// httpOnly cookie names. BOTH tokens are httpOnly + Secure + SameSite and are held
// by the BFF only — the browser's JS never reads either. The refresh token in
// particular never reaches client code (RFC 0001 §11.6).
export const ACCESS_COOKIE = 'amicu_at';
export const REFRESH_COOKIE = 'amicu_rt';

/** Refresh-cookie lifetime (30 days). The issuer's own revocation is authoritative. */
export const REFRESH_MAX_AGE = 60 * 60 * 24 * 30;
