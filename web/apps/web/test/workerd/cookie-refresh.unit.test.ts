// Unit test: exercise the silent-refresh cookie-rotation
// path (proxy.ts's handleAuth() -> attemptRefresh()) with the SAME Request/Response
// classes the Workers runtime (workerd) uses under @opennextjs/cloudflare - Next's
// `next/server` NextRequest/NextResponse are thin wrappers around the standard Web
// Request/Response (the exact interface OpenNext's Worker calls into), so invoking
// `proxy()` (the actual middleware source, not a reimplementation of it) with a
// crafted NextRequest here exercises the real code path.
//
// This is the "always-runs, always-passes" half of #146(a) - see
// `test/workerd/live-preview.test.ts` for the companion test that boots the actual
// built Worker under real workerd (wrangler dev), which is a slower, best-effort,
// self-skipping integration test kept out of the default `pnpm test` run.
//
// Why this needs `tsx` + `--conditions=react-server` (see the `test` script in
// package.json):
//   - `proxy.ts`'s import graph uses the `@/*` path alias and extensionless
//     specifiers that only Next's own bundler (webpack/turbopack) resolves; plain
//     `node --experimental-strip-types` cannot follow them. `tsx`'s loader hooks
//     resolve both (tsconfig `paths` + extensionless TS/JS specifiers) without
//     requiring a full Next.js runtime.
//   - `lib/auth/config.ts` (transitively) imports the `server-only` marker package,
//     which throws unless the `react-server` export condition is active (that's how
//     Next's own bundler enforces "server-only" - it's normally set by the RSC
//     bundler). `--conditions=react-server` supplies the same condition so the
//     marker package resolves to its no-op `empty.js` export instead of throwing,
//     exactly like it does inside Next's real server build.
//
// `API_URL` (and friends) must be set BEFORE `proxy.ts` is first imported, because
// `lib/auth/config.ts` calls `parseServerEnv(process.env)` at module-evaluation time
// (see server-env.ts) - so this file starts a stub HTTP API and sets `process.env`
// in `before()`, then dynamically `import()`s `proxy.ts` only afterward. Because
// `API_URL` is captured once at that first import, ONE stub server instance backs
// every test in this file - tests give it uniquely-named tokens rather than each
// getting a fresh server.
//
// The stub is `fixtures/refresh-stub.ts` - a faithful (single-use, family-scoped
// reuse detection) reimplementation of the real backend's refresh semantics
// (crates/amicu-store/src/refresh.rs `consume()`, crates/amicu-auth/src/service.rs
// `refresh()`), not a "always mint a fresh pair" stub. That fidelity is what makes
// the double-invocation/reuse tests below meaningful instead of assertion theater.

import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import type { NextRequest as NextRequestType, NextResponse } from 'next/server';
import { startStubAuthApi, type StubAuthApi } from './fixtures/refresh-stub.ts';

const ACCESS_COOKIE = '__Host-amicu_at';
const REFRESH_COOKIE = '__Host-amicu_rt';

/** A syntactically-valid, unsigned JWT with the given `exp` (seconds since epoch). */
function craftJwt(exp: number): string {
  const b64url = (obj: unknown) =>
    Buffer.from(JSON.stringify(obj)).toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  return `${b64url({ alg: 'none', typ: 'JWT' })}.${b64url({ exp, sub: 'test-user' })}.sig`;
}

function expiredAccessToken(): string {
  return craftJwt(Math.floor(Date.now() / 1000) - 30);
}

function freshAccessToken(): string {
  return craftJwt(Math.floor(Date.now() / 1000) + 60 * 30);
}

let stubApi: StubAuthApi;
let proxy: (request: NextRequestType) => Promise<NextResponse>;
let NextRequest: typeof NextRequestType;

before(async () => {
  stubApi = await startStubAuthApi();

  // Set BEFORE the first import of proxy.ts (see file header) - config.ts reads
  // process.env at module-evaluation time.
  process.env.API_URL = `http://127.0.0.1:${stubApi.port}`;
  process.env.NODE_ENV = 'production';
  process.env.COOKIE_SECURE = 'true';

  ({ default: proxy } = await import('../../proxy.ts'));
  ({ NextRequest } = await import('next/server'));
});

after(async () => {
  await stubApi.close();
});

test('rotates the session cookies with HttpOnly/Secure/SameSite=Lax on an expiring access token', async () => {
  const REFRESH_TOKEN = 'rotate-test-original-refresh';
  stubApi.seedRefreshToken(REFRESH_TOKEN);
  const before = stubApi.requests.length;

  const request = new NextRequest('https://amicu.example.test/app', {
    headers: { cookie: `${ACCESS_COOKIE}=${expiredAccessToken()}; ${REFRESH_COOKIE}=${REFRESH_TOKEN}` },
  });

  const response = await proxy(request);

  // The refresh call actually happened against the stub (proves attemptRefresh()
  // drove a real network round-trip, not a short-circuit), and won cleanly.
  const calls = stubApi.requests.slice(before);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].refreshToken, REFRESH_TOKEN);
  assert.equal(calls[0].outcome, 'consumed');
  const issued = calls[0].issuedRefreshToken!;

  // `getSetCookie()` (standard Fetch API Headers method) returns each Set-Cookie
  // header individually - unlike `.get('set-cookie')`, which would illegally
  // comma-join them (RFC 6265 cookies aren't comma-combinable).
  const setCookieHeaders = response.headers.getSetCookie();
  assert.equal(setCookieHeaders.length, 2, `expected 2 Set-Cookie headers, got: ${setCookieHeaders.join(' | ')}`);

  const byName = new Map(setCookieHeaders.map((h) => [h.split('=')[0], h]));
  const at = byName.get(ACCESS_COOKIE);
  const rt = byName.get(REFRESH_COOKIE);
  assert.ok(at, 'amicu_at Set-Cookie header missing');
  assert.ok(rt, 'amicu_rt Set-Cookie header missing');

  for (const [name, header] of [
    [ACCESS_COOKIE, at],
    [REFRESH_COOKIE, rt],
  ] as const) {
    assert.match(header!, /HttpOnly/i, `${name} must be HttpOnly`);
    assert.match(header!, /Secure/i, `${name} must be Secure (COOKIE_SECURE=true)`);
    assert.match(header!, /SameSite=Lax/i, `${name} must be SameSite=Lax`);
  }

  // The rotated refresh cookie is exactly what the stub issued for this call.
  assert.ok(rt!.startsWith(`${REFRESH_COOKIE}=${issued};`));
  assert.ok(stubApi.isValid(issued));
  assert.ok(!stubApi.isFamilyCompromised(issued));

  // Not cached: the middleware response must not declare itself cacheable by a
  // shared/CDN cache (Cloudflare in production). It must not carry a `public` or
  // positive `s-maxage`/`max-age` Cache-Control, and must carry no CF-Cache-Status
  // (which only a cache HIT/DYNAMIC would add - absence here means "not served from
  // cache", consistent with a Set-Cookie-bearing response).
  const cacheControl = response.headers.get('cache-control');
  if (cacheControl) {
    assert.doesNotMatch(
      cacheControl,
      /(^|,)\s*public\s*(,|;|$)/i,
      `Set-Cookie response must not be marked publicly cacheable, got: ${cacheControl}`,
    );
  }
  assert.equal(response.headers.get('cf-cache-status'), null);
});

test('a session with a still-fresh access token is left untouched (no rotation, no Set-Cookie)', async () => {
  const REFRESH_TOKEN = 'fresh-access-test-refresh';
  stubApi.seedRefreshToken(REFRESH_TOKEN);
  const before = stubApi.requests.length;

  const request = new NextRequest('https://amicu.example.test/app', {
    headers: { cookie: `${ACCESS_COOKIE}=${freshAccessToken()}; ${REFRESH_COOKIE}=${REFRESH_TOKEN}` },
  });

  const response = await proxy(request);

  assert.equal(stubApi.requests.length, before, 'a fresh access token must not trigger a refresh call');
  assert.equal(response.headers.getSetCookie().length, 0);
});

test('a route outside REFRESH_PREFIXES is never routed through attemptRefresh', async () => {
  const REFRESH_TOKEN = 'outside-prefix-test-refresh';
  stubApi.seedRefreshToken(REFRESH_TOKEN);
  const before = stubApi.requests.length;

  const request = new NextRequest('https://amicu.example.test/pricing', {
    headers: { cookie: `${ACCESS_COOKIE}=${expiredAccessToken()}; ${REFRESH_COOKIE}=${REFRESH_TOKEN}` },
  });

  const response = await proxy(request);

  assert.equal(stubApi.requests.length, before);
  assert.equal(response.headers.getSetCookie().length, 0);
});

// --- Reuse / "double invocation" coverage, added after adversarial review -------
//
// PR #142's live-preview.test.ts observed (with this same faithful stub, against
// the real built Worker under real workerd) that OpenNext's Node.js middleware
// compat layer invokes attemptRefresh()'s network call EXACTLY ONCE per inbound
// request - the apparent "two rotated cookie pairs" seen in the raw response is a
// response-header SERIALIZATION duplicate of that one call's result, not a second
// refresh() round-trip. So the reuse/family-compromise failure mode the review
// raised does not occur under the ACTUAL observed behavior of this build.
//
// The tests below still cover it directly at the proxy.ts level, for two reasons:
// (1) proxy.ts itself has no way to detect "this is the same logical request I
// already handled" across two independently-constructed NextRequests (there is no
// shared session state at the middleware layer) - if a future OpenNext/Next.js
// version ever DID genuinely re-dispatch with a freshly-reconstructed request
// carrying the original (pre-rotation) cookie, this is exactly what would happen,
// and (2) review asked for the previously-missing `revoked` path coverage
// regardless.

test('reusing the SAME (already-mutated) NextRequest for a second proxy() pass is a no-op: proxy.ts mutates request.cookies in place, so the second pass sees the already-rotated (fresh) access token and never calls refresh() again', async () => {
  const REFRESH_TOKEN = 'same-request-reuse-test-refresh';
  stubApi.seedRefreshToken(REFRESH_TOKEN);
  const before = stubApi.requests.length;

  const request = new NextRequest('https://amicu.example.test/app', {
    headers: { cookie: `${ACCESS_COOKIE}=${expiredAccessToken()}; ${REFRESH_COOKIE}=${REFRESH_TOKEN}` },
  });

  const first = await proxy(request);
  const firstCalls = stubApi.requests.slice(before);
  assert.equal(firstCalls.length, 1);
  assert.equal(firstCalls[0].outcome, 'consumed');
  assert.equal(first.headers.getSetCookie().length, 2);

  // Re-invoke proxy() with the SAME request object handleAuth() already mutated
  // (`request.cookies.set(...)` in proxy.ts, done specifically so a same-request
  // downstream read sees the fresh cookie - see proxy.ts's comment on handleAuth).
  const second = await proxy(request);

  assert.equal(
    stubApi.requests.length,
    before + 1,
    'a second pass over the SAME (already-rotated) request object must not call refresh() again',
  );
  assert.equal(second.headers.getSetCookie().length, 0, 'no Set-Cookie on the no-op second pass');
});

test('the revoked path: a rejected/reused refresh token clears both cookies and never 500s', async () => {
  const REFRESH_TOKEN = 'revoked-path-test-refresh';
  // Seed it PRE-CONSUMED (simulating an already-rotated-past token being replayed -
  // the exact shape of crates/amicu-store/src/refresh.rs `consume()`'s reuse branch,
  // which the real backend reaches for an unknown OR an already-revoked/consumed
  // token; both map to the same 401 `{"error":"refresh token invalid"}` - see
  // crates/amicu-api/src/auth.rs's ApiError mapping and AuthError::RefreshInvalid).
  stubApi.seedRefreshToken(REFRESH_TOKEN);
  const winner = new NextRequest('https://amicu.example.test/app', {
    headers: { cookie: `${ACCESS_COOKIE}=${expiredAccessToken()}; ${REFRESH_COOKIE}=${REFRESH_TOKEN}` },
  });
  await proxy(winner); // consumes REFRESH_TOKEN for real, via the stub's normal path

  const before = stubApi.requests.length;
  const replay = new NextRequest('https://amicu.example.test/app', {
    headers: { cookie: `${ACCESS_COOKIE}=${expiredAccessToken()}; ${REFRESH_COOKIE}=${REFRESH_TOKEN}` },
  });
  const response = await proxy(replay);

  const calls = stubApi.requests.slice(before);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].outcome, 'reuse');

  assert.notEqual(response.status, 500, 'a revoked/reused refresh token must never 500 the request');
  // middleware-refresh.ts's `revoked` branch -> proxy.ts clears both cookies rather
  // than setting a rotated pair. A cleared cookie is still a Set-Cookie header (with
  // an expired/zeroed Max-Age), so assert on VALUE, not presence.
  const setCookieHeaders = response.headers.getSetCookie();
  const at = setCookieHeaders.find((c) => c.startsWith(`${ACCESS_COOKIE}=`));
  const rt = setCookieHeaders.find((c) => c.startsWith(`${REFRESH_COOKIE}=`));
  assert.ok(at, 'amicu_at must still be addressed by a Set-Cookie (clearing it)');
  assert.ok(rt, 'amicu_rt must still be addressed by a Set-Cookie (clearing it)');
  assert.match(at!, /^__Host-amicu_at=;/, 'amicu_at must be cleared (empty value), not left with a stale token');
  assert.match(rt!, /^__Host-amicu_rt=;/, 'amicu_rt must be cleared (empty value), not left with a stale token');
});

test('DOCUMENTED RESIDUAL RISK: if a second, independently-constructed request ever replays the pre-rotation refresh cookie after a legitimate rotation, the whole token family is compromised - including the just-issued valid successor', async () => {
  const REFRESH_TOKEN = 'residual-risk-test-refresh';
  stubApi.seedRefreshToken(REFRESH_TOKEN);

  // Call #1: legitimate rotation. Mints a valid successor.
  const call1 = new NextRequest('https://amicu.example.test/app', {
    headers: { cookie: `${ACCESS_COOKIE}=${expiredAccessToken()}; ${REFRESH_COOKIE}=${REFRESH_TOKEN}` },
  });
  const res1 = await proxy(call1);
  const rotated = res1.headers
    .getSetCookie()
    .find((c) => c.startsWith(`${REFRESH_COOKIE}=`))!
    .split(';')[0]
    .split('=')[1];
  assert.ok(stubApi.isValid(rotated), 'the successor from call #1 must start out valid');

  // Call #2: a SEPARATE, freshly-constructed NextRequest presenting the ORIGINAL
  // (now-consumed) cookie - i.e. NOT the same in-memory request object call #1
  // mutated. This is what a genuine double-dispatch with request reconstruction
  // (rather than object reuse) would look like. live-preview.test.ts's faithful-stub
  // assertion (`stubApi.requests.length === 1`) is the evidence that the real
  // OpenNext build does NOT do this - this test exists to characterize the
  // consequence IF it ever did, not to claim it currently happens.
  const call2 = new NextRequest('https://amicu.example.test/app', {
    headers: { cookie: `${ACCESS_COOKIE}=${expiredAccessToken()}; ${REFRESH_COOKIE}=${REFRESH_TOKEN}` },
  });
  await proxy(call2);

  assert.ok(
    stubApi.isFamilyCompromised(rotated),
    'reuse detection compromises the whole family, per crates/amicu-store/src/refresh.rs compromise_family()',
  );
  assert.ok(
    !stubApi.isValid(rotated),
    'the successor token from the LEGITIMATE call #1 - already handed to the browser via Set-Cookie - is now also invalid: ' +
      'this is the real-world impact if a genuine duplicate dispatch with a reconstructed request ever occurred',
  );
});

test('proxy: cross-origin POST to /api/* is rejected with 403; same-origin POST and cross-origin GET pass through', async () => {
  const url = 'https://amicu.example.test/api/engine/plan';
  const cross = await proxy(new NextRequest(url, { method: 'POST', headers: { origin: 'https://evil.example' } }));
  assert.equal(cross.status, 403);
  const crossFetchMeta = await proxy(new NextRequest(url, { method: 'POST', headers: { 'sec-fetch-site': 'cross-site' } }));
  assert.equal(crossFetchMeta.status, 403);
  const same = await proxy(new NextRequest(url, { method: 'POST', headers: { origin: 'https://amicu.example.test' } }));
  assert.notEqual(same.status, 403);
  const get = await proxy(new NextRequest(url, { method: 'GET', headers: { origin: 'https://evil.example' } }));
  assert.notEqual(get.status, 403);
});
