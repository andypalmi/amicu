// proxy.ts with sign-in disabled (the default: NEXT_PUBLIC_SIGNIN_ENABLED unset):
// auth pages and session-gated pages are rewritten to a nonexistent path (real
// 404), marketing/docs/api pass through. See cookie-refresh.unit.test.ts for why
// this runs under tsx + react-server and imports proxy.ts dynamically.
import { test, before } from 'node:test';
import assert from 'node:assert/strict';

type Proxy = (req: import('next/server').NextRequest) => Promise<Response>;
let proxy: Proxy;
let NextRequest: typeof import('next/server').NextRequest;

before(async () => {
  process.env.API_URL = 'http://127.0.0.1:1';
  process.env.NODE_ENV = 'production';
  process.env.COOKIE_SECURE = 'true';
  delete process.env.NEXT_PUBLIC_SIGNIN_ENABLED;
  ({ default: proxy } = await import('../../proxy.ts'));
  ({ NextRequest } = await import('next/server'));
});

const rewriteOf = (res: Response) => res.headers.get('x-middleware-rewrite');

for (const path of ['/login', '/signup', '/forgot', '/verify', '/app', '/account', '/configs', '/billing', '/org', '/monitor', '/new', '/new/guided', '/wizard', '/recommend']) {
  test(`disabled: ${path} is rewritten to a 404 path`, async () => {
    const res = await proxy(new NextRequest(`http://localhost:3000${path}?next=/app`));
    assert.match(rewriteOf(res) ?? '', /\/__signin-disabled$/);
  });
}

test('disabled: app host root (rewritten to /app) is a 404 too', async () => {
  const res = await proxy(new NextRequest('https://app.amicu.dev/'));
  assert.match(rewriteOf(res) ?? '', /\/__signin-disabled$/);
});

test('disabled: amicu.dev/login is not 301d to the app host', async () => {
  const res = await proxy(new NextRequest('https://amicu.dev/login'));
  assert.match(rewriteOf(res) ?? '', /\/__signin-disabled$/);
  assert.equal(res.headers.get('location'), null);
});

test('disabled: marketing, docs and /api paths are untouched', async () => {
  for (const path of ['/', '/pricing', '/docs/quickstart', '/api/health']) {
    const res = await proxy(new NextRequest(`http://localhost:3000${path}`));
    assert.equal(rewriteOf(res), null, path);
    assert.equal(res.status, 200, path);
  }
});

for (const path of ['/%6Cogin', '/%61pp', '/Login', '//login', '/app%2Fx', '/%E0%A4%A']) {
  test(`disabled: encoded/odd path ${path} is a 404 path`, async () => {
    const res = await proxy(new NextRequest(`http://localhost:3000${path}`));
    assert.match(rewriteOf(res) ?? '', /\/__signin-disabled$/);
  });
}
