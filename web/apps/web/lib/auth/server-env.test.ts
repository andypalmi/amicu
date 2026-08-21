// Pure-logic tests for the fail-fast server env schema. Uses Node's built-in test
// runner (`node --test`, with type-stripping) so no test framework is added.
// `parseServerEnv` is pure, so we pass crafted env maps instead of mutating the
// real `process.env`. Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseServerEnv, DEV_API_URL } from './server-env.ts';

test('production without API_URL throws a clear boot-time error', () => {
  assert.throws(
    () => parseServerEnv({ NODE_ENV: 'production' } as NodeJS.ProcessEnv),
    (err: unknown) => err instanceof Error && /API_URL/.test(err.message),
  );
});

test('production with an invalid API_URL throws', () => {
  assert.throws(
    () => parseServerEnv({ NODE_ENV: 'production', API_URL: 'not-a-url' } as NodeJS.ProcessEnv),
    (err: unknown) => err instanceof Error && /API_URL/.test(err.message),
  );
});

test('production with a valid API_URL derives JWKS/issuer/audience and strips slashes', () => {
  const env = parseServerEnv({
    NODE_ENV: 'production',
    API_URL: 'https://api.example.com///',
  } as NodeJS.ProcessEnv);

  assert.equal(env.API_URL, 'https://api.example.com');
  assert.equal(env.AUTH_JWKS_URL, 'https://api.example.com/.well-known/jwks.json');
  assert.equal(env.AUTH_ISSUER, 'https://api.example.com');
  assert.equal(env.AUTH_AUDIENCE, 'amicu');
});

test('development applies the localhost default when API_URL is unset', () => {
  const env = parseServerEnv({ NODE_ENV: 'development' } as NodeJS.ProcessEnv);

  assert.equal(env.API_URL, DEV_API_URL);
  assert.equal(env.AUTH_JWKS_URL, `${DEV_API_URL}/.well-known/jwks.json`);
  assert.equal(env.AUTH_ISSUER, DEV_API_URL);
  assert.equal(env.AUTH_AUDIENCE, 'amicu');
});

test('the localhost default is NOT applied in production (prod must be explicit)', () => {
  // Same missing API_URL as the dev case above, but production refuses to default.
  assert.throws(() => parseServerEnv({ NODE_ENV: 'production' } as NodeJS.ProcessEnv));
  // ...and dev, given the SAME input, succeeds with the default — proving the
  // default is gated to non-production only.
  assert.doesNotThrow(() => parseServerEnv({ NODE_ENV: 'development' } as NodeJS.ProcessEnv));
});

test('the next build phase is exempt from the prod requirement (build has no secrets)', () => {
  // NODE_ENV=production but during `next build`, so API_URL is not required and
  // the dev default fills in — the check only bites when actually serving.
  const env = parseServerEnv({
    NODE_ENV: 'production',
    NEXT_PHASE: 'phase-production-build',
  } as NodeJS.ProcessEnv);
  assert.equal(env.API_URL, DEV_API_URL);
});

test('explicit overrides for JWKS/issuer/audience are honored', () => {
  const env = parseServerEnv({
    NODE_ENV: 'production',
    API_URL: 'https://api.example.com',
    AUTH_JWKS_URL: 'https://auth.example.com/jwks.json',
    AUTH_ISSUER: 'https://issuer.example.com',
    AUTH_AUDIENCE: 'custom-aud',
  } as NodeJS.ProcessEnv);

  assert.equal(env.AUTH_JWKS_URL, 'https://auth.example.com/jwks.json');
  assert.equal(env.AUTH_ISSUER, 'https://issuer.example.com');
  assert.equal(env.AUTH_AUDIENCE, 'custom-aud');
});
