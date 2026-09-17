// Unit test for amicu-planning#146(a): exercise the silent-refresh cookie-rotation
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
// in `before()`, then dynamically `import()`s `proxy.ts` only afterward.

import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import type { NextRequest as NextRequestType, NextResponse } from 'next/server';

const ACCESS_COOKIE = 'amicu_at';
const REFRESH_COOKIE = 'amicu_rt';

/** A syntactically-valid, unsigned JWT with the given `exp` (seconds since epoch). */
function craftJwt(exp: number): string {
  const b64url = (obj: unknown) =>
    Buffer.from(JSON.stringify(obj)).toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  return `${b64url({ alg: 'none', typ: 'JWT' })}.${b64url({ exp, sub: 'test-user' })}.sig`;
}

const EXPIRED_ACCESS_TOKEN = craftJwt(Math.floor(Date.now() / 1000) - 30);
const OLD_REFRESH_TOKEN = 'unit-test-old-refresh-token';
const NEW_ACCESS_TOKEN = 'unit-test-new-access-token';
const NEW_REFRESH_TOKEN = 'unit-test-new-refresh-token';

let stubApi: http.Server;
let stubApiRequests: Array<{ method: string | undefined; url: string | undefined; body: string }> = [];
let proxy: (request: NextRequestType) => Promise<NextResponse>;
let NextRequest: typeof NextRequestType;

before(async () => {
  // Stand in for the Rust Management API's `POST /token/refresh` - just enough to
  // drive `attemptRefresh()`'s "rotated" outcome (see lib/auth/middleware-refresh.ts).
  stubApi = http.createServer((req, res) => {
    let body = '';
    req.on('data', (chunk) => (body += chunk));
    req.on('end', () => {
      stubApiRequests.push({ method: req.method, url: req.url, body });
      if (req.method === 'POST' && req.url === '/token/refresh') {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(
          JSON.stringify({
            access_token: NEW_ACCESS_TOKEN,
            refresh_token: NEW_REFRESH_TOKEN,
            expires_in: 900,
            token_type: 'Bearer',
          }),
        );
        return;
      }
      res.writeHead(404, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: 'not_found' }));
    });
  });
  await new Promise<void>((resolve) => stubApi.listen(0, '127.0.0.1', resolve));
  const { port } = stubApi.address() as AddressInfo;

  // Set BEFORE the first import of proxy.ts (see file header) - config.ts reads
  // process.env at module-evaluation time.
  process.env.API_URL = `http://127.0.0.1:${port}`;
  process.env.NODE_ENV = 'production';
  process.env.COOKIE_SECURE = 'true';

  ({ default: proxy } = await import('../../proxy.ts'));
  ({ NextRequest } = await import('next/server'));
});

after(async () => {
  await new Promise((resolve) => stubApi.close(resolve));
});

test('rotates the session cookies with HttpOnly/Secure/SameSite=Lax on an expiring access token', async () => {
  stubApiRequests = [];
  const request = new NextRequest('https://amicu.example.test/app', {
    headers: { cookie: `${ACCESS_COOKIE}=${EXPIRED_ACCESS_TOKEN}; ${REFRESH_COOKIE}=${OLD_REFRESH_TOKEN}` },
  });

  const response = await proxy(request);

  // The refresh call actually happened against the stub (proves attemptRefresh()
  // drove a real network round-trip, not a short-circuit).
  assert.equal(stubApiRequests.length, 1);
  assert.equal(stubApiRequests[0].url, '/token/refresh');
  assert.equal(JSON.parse(stubApiRequests[0].body).refresh_token, OLD_REFRESH_TOKEN);

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
    ['amicu_at', at],
    ['amicu_rt', rt],
  ] as const) {
    assert.match(header!, /HttpOnly/i, `${name} must be HttpOnly`);
    assert.match(header!, /Secure/i, `${name} must be Secure (COOKIE_SECURE=true)`);
    assert.match(header!, /SameSite=Lax/i, `${name} must be SameSite=Lax`);
  }

  // The rotated values themselves came from the stub, not stale/echoed input.
  assert.ok(at!.startsWith(`${ACCESS_COOKIE}=${NEW_ACCESS_TOKEN};`));
  assert.ok(rt!.startsWith(`${REFRESH_COOKIE}=${NEW_REFRESH_TOKEN};`));

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
  stubApiRequests = [];
  const freshAccess = craftJwt(Math.floor(Date.now() / 1000) + 60 * 30);
  const request = new NextRequest('https://amicu.example.test/app', {
    headers: { cookie: `${ACCESS_COOKIE}=${freshAccess}; ${REFRESH_COOKIE}=${OLD_REFRESH_TOKEN}` },
  });

  const response = await proxy(request);

  assert.equal(stubApiRequests.length, 0, 'a fresh access token must not trigger a refresh call');
  assert.equal(response.headers.getSetCookie().length, 0);
});

test('a route outside REFRESH_PREFIXES is never routed through attemptRefresh', async () => {
  stubApiRequests = [];
  const request = new NextRequest('https://amicu.example.test/pricing', {
    headers: { cookie: `${ACCESS_COOKIE}=${EXPIRED_ACCESS_TOKEN}; ${REFRESH_COOKIE}=${OLD_REFRESH_TOKEN}` },
  });

  const response = await proxy(request);

  assert.equal(stubApiRequests.length, 0);
  assert.equal(response.headers.getSetCookie().length, 0);
});
