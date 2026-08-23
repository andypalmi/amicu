// Fail-fast validation of the server-only auth/BFF environment.
//
// This module holds NO `server-only` import on purpose: it is a pure, side-effect
// free schema so it can be unit-tested under `node --test` with crafted inputs.
// `lib/auth/config.ts` is the single caller - it invokes `parseServerEnv(process.env)`
// on first module evaluation (middleware / first auth request), so a misconfigured
// production deploy fails LOUDLY instead of silently targeting localhost. Nothing
// here is exposed to the browser.

import { z } from 'zod';

/** Dev-only fallback API origin. Never used when NODE_ENV === 'production'. */
export const DEV_API_URL = 'http://localhost:8080';

/** The resolved, validated server config the app derives its auth wiring from. */
export interface ServerEnv {
  /** Base URL of the Rust Management API / auth issuer (trailing slashes stripped). */
  API_URL: string;
  /** JWKS endpoint used to verify access tokens offline. */
  AUTH_JWKS_URL: string;
  /** Expected token issuer (`iss`). */
  AUTH_ISSUER: string;
  /** Expected token audience (`aud`). */
  AUTH_AUDIENCE: string;
}

/**
 * Validate and resolve the server environment.
 *
 * `API_URL` is the floor: it is REQUIRED and must be a valid URL when serving in
 * production, and throws a clear boot-time error when missing/invalid there.
 * Outside production it defaults to `DEV_API_URL` so the app boots without a
 * backend. The other vars stay optional everywhere - each has a safe derivation
 * from `API_URL` (JWKS/issuer) or a stable default (audience), so forcing them
 * would be needlessly strict.
 *
 * Enforcement is deliberately skipped during `next build` (NEXT_PHASE ===
 * 'phase-production-build'): that phase evaluates route modules with
 * NODE_ENV=production but no runtime secrets, so requiring API_URL there would
 * break the build rather than a real deploy. The check still fires at runtime
 * (`next start` / request handling), which is where a misconfigured deploy bites.
 *
 * Pure: pass any `env` map. `config.ts` passes `process.env`.
 *
 * @throws {Error} when validation fails (e.g. `API_URL` unset/invalid when serving
 *   in production).
 */
export function parseServerEnv(env: NodeJS.ProcessEnv): ServerEnv {
  const isProd = env.NODE_ENV === 'production';
  const isBuildPhase = env.NEXT_PHASE === 'phase-production-build';
  const requireApiUrl = isProd && !isBuildPhase;

  // A valid URL, required only when serving in production. Otherwise it may be
  // omitted and a dev default is substituted below.
  const apiUrl = z.url('API_URL must be a valid URL (e.g. https://api.example.com)');

  const schema = z.object({
    API_URL: requireApiUrl ? apiUrl : apiUrl.optional(),
    AUTH_JWKS_URL: z.url('AUTH_JWKS_URL must be a valid URL').optional(),
    AUTH_ISSUER: z.string().min(1, 'AUTH_ISSUER must be non-empty').optional(),
    AUTH_AUDIENCE: z.string().min(1, 'AUTH_AUDIENCE must be non-empty').optional(),
  });

  const parsed = schema.safeParse({
    API_URL: env.API_URL,
    AUTH_JWKS_URL: env.AUTH_JWKS_URL,
    AUTH_ISSUER: env.AUTH_ISSUER,
    AUTH_AUDIENCE: env.AUTH_AUDIENCE,
  });

  if (!parsed.success) {
    const details = parsed.error.issues
      .map((issue) => `${issue.path.join('.') || '(root)'}: ${issue.message}`)
      .join('; ');
    throw new Error(
      `Invalid server environment for the amicu web BFF: ${details}. ` +
        'Set the required variables (API_URL at minimum) before starting in production.',
    );
  }

  const base = (parsed.data.API_URL ?? DEV_API_URL).replace(/\/+$/, '');

  return {
    API_URL: base,
    AUTH_JWKS_URL: parsed.data.AUTH_JWKS_URL ?? `${base}/.well-known/jwks.json`,
    AUTH_ISSUER: parsed.data.AUTH_ISSUER ?? base,
    AUTH_AUDIENCE: parsed.data.AUTH_AUDIENCE ?? 'amicu',
  };
}
