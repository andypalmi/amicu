import { test } from 'node:test';
import assert from 'node:assert/strict';
import { requestIsSameOrigin } from './csrf.ts';
import { readJsonBody, MAX_BODY_BYTES } from './body.ts';
import { sessionCookieNames } from '../auth/cookie-names.ts';

const URL_ = 'https://app.example.com/api/engine/plan';
const SITE = 'https://app.example.com';
const req = (method: string, headers: Record<string, string> = {}, url = URL_) =>
  new Request(url, { method, headers });

test('cross-origin POST is rejected', () => {
  assert.equal(requestIsSameOrigin(req('POST', { origin: 'https://evil.example' }), SITE), false);
  assert.equal(requestIsSameOrigin(req('POST', { origin: 'null' }), SITE), false);
  assert.equal(requestIsSameOrigin(req('DELETE', { 'sec-fetch-site': 'cross-site' }), SITE), false);
  assert.equal(requestIsSameOrigin(req('POST', { 'sec-fetch-site': 'same-site' }), SITE), false);
});

test('same-origin POST is allowed (url, site URL, and forwarded host)', () => {
  assert.equal(requestIsSameOrigin(req('POST', { origin: SITE }), SITE), true);
  assert.equal(requestIsSameOrigin(req('POST', { 'sec-fetch-site': 'same-origin' }), SITE), true);
  // Worker sees an internal URL; public host arrives via X-Forwarded-*.
  const internal = req(
    'POST',
    { origin: 'https://public.example.org', 'x-forwarded-host': 'public.example.org', 'x-forwarded-proto': 'https' },
    'http://127.0.0.1:8787/api/x',
  );
  assert.equal(requestIsSameOrigin(internal, null), true);
  // Configured site URL is honoured even when the host headers differ.
  assert.equal(
    requestIsSameOrigin(req('POST', { origin: SITE }, 'http://internal/api/x'), SITE),
    true,
  );
});

test('GET/HEAD are unaffected', () => {
  assert.equal(requestIsSameOrigin(req('GET', { origin: 'https://evil.example' }), SITE), true);
  assert.equal(requestIsSameOrigin(req('HEAD', { 'sec-fetch-site': 'cross-site' }), SITE), true);
});

test('cookie names by environment', () => {
  assert.deepEqual(sessionCookieNames(true), {
    access: '__Host-amicu_at',
    refresh: '__Host-amicu_rt',
    legacy: ['amicu_at', 'amicu_rt'],
  });
  assert.deepEqual(sessionCookieNames(false), { access: 'amicu_at', refresh: 'amicu_rt', legacy: [] });
});

test('body reader: ok, invalid, oversized (declared and streamed)', async () => {
  const post = (body: BodyInit, headers: Record<string, string> = {}) =>
    new Request(URL_, { method: 'POST', body, headers });

  const ok = await readJsonBody(post('{"a":1}'));
  assert.deepEqual('value' in ok && ok.value, { a: 1 });

  const bad = await readJsonBody(post('{nope'));
  assert.ok('deny' in bad && bad.deny.status === 400);

  const big = JSON.stringify({ s: 'x'.repeat(MAX_BODY_BYTES) });
  const declared = await readJsonBody(post(big));
  assert.ok('deny' in declared && declared.deny.status === 413);

  // No/lying Content-Length: cap enforced while streaming.
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      c.enqueue(new TextEncoder().encode('"' + 'x'.repeat(100)));
      c.enqueue(new TextEncoder().encode('x'.repeat(100) + '"'));
      c.close();
    },
  });
  const streamed = await readJsonBody(
    new Request(URL_, { method: 'POST', body: stream, duplex: 'half' } as RequestInit),
    150,
  );
  assert.ok('deny' in streamed && streamed.deny.status === 413);
});
