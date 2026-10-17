import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { afterEach, beforeEach, test } from 'node:test';

import { handle, ipKey, MAX_BODY_BYTES, ROUTE, type Env, type RateLimit } from '../src/handler.ts';

const exampleText = readFileSync(new URL('fixtures/telemetry-batch.v2.example.json', import.meta.url), 'utf8');
const example = JSON.parse(exampleText);
const URL_ = `https://api.amicu.dev${ROUTE}`;
const SECRET = 'sb_secret_test_value';
// "Now" for window checks: just after the example's window.
const NOW = example.reports[0].window.start_unix_s + 600;

type Call = { url: string; init: RequestInit };

/** A mocked Supabase Data API: records calls, answers with `respond`. */
function supabase(respond: () => Response | Promise<Response>) {
  const calls: Call[] = [];
  const fetchImpl = async (url: string, init: RequestInit) => {
    calls.push({ url, init });
    return respond();
  };
  return { calls, fetchImpl };
}

function ok() {
  return supabase(() => Response.json({ duplicate: false, inserted: 1, duplicates: 0, skipped: 0 }));
}

function limiter(allow: (key: string) => boolean): RateLimit & { keys: string[] } {
  const keys: string[] = [];
  return {
    keys,
    async limit({ key }) {
      keys.push(key);
      return { success: allow(key) };
    },
  };
}

function env(extra: Partial<Env> = {}): Env {
  return { SUPABASE_URL: 'https://example.supabase.co', SUPABASE_SECRET_KEY: SECRET, ...extra };
}

function post(body: string | Uint8Array<ArrayBuffer> = exampleText, headers: Record<string, string> = {}) {
  return new Request(URL_, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'idempotency-key': example.batch_id, ...headers },
    body,
  });
}

let logs: string[] = [];
const realLog = console.log;
beforeEach(() => {
  logs = [];
  console.log = (...args: unknown[]) => logs.push(args.join(' '));
});
afterEach(() => {
  console.log = realLog;
});

function assertNoLeak(res: Response, text: string) {
  for (const h of res.headers.keys()) assert.ok(!h.startsWith('access-control-'), `CORS header ${h}`);
  for (const secret of [example.reports[0].install_id, example.reports[0].model.name, SECRET, '203.0.113.7']) {
    assert.ok(!text.includes(secret), 'response echoes input');
    assert.ok(!logs.join('\n').includes(secret), 'log contains input');
  }
}

test('202 on success; one RPC call with the secret key in apikey only', async () => {
  const db = ok();
  const res = await handle(post(exampleText, { 'cf-connecting-ip': '203.0.113.7' }), env(), db.fetchImpl, NOW);
  assert.equal(res.status, 202);
  assertNoLeak(res, await res.text());
  assert.equal(db.calls.length, 1);
  const { url, init } = db.calls[0];
  assert.equal(url, 'https://example.supabase.co/rest/v1/rpc/ingest_rollup_batch');
  const headers = new Headers(init.headers);
  assert.equal(headers.get('apikey'), SECRET);
  assert.equal(headers.get('authorization'), null);
  assert.match(headers.get('user-agent') ?? '', /^amicu-telemetry-ingest\//);
  assert.deepEqual(JSON.parse(init.body as string), { batch: example });
});

test('region: 202 with provider modal, 400 with local or no provider', async () => {
  const db = ok();
  const send = async (provider: string | undefined) => {
    const b = structuredClone(example);
    if (provider === undefined) delete b.reports[0].provider;
    else b.reports[0].provider = provider;
    b.reports[0].region = 'eu-west';
    return (await handle(post(JSON.stringify(b)), env(), db.fetchImpl, NOW)).status;
  };
  assert.equal(await send('local'), 400);
  assert.equal(await send(undefined), 400);
  assert.equal(await send('modal'), 202);
  assert.equal(db.calls.length, 1);
});

test('a retried batch the database reports as duplicate is still 202', async () => {
  const db = supabase(() => Response.json({ duplicate: true, inserted: 0 }));
  const res = await handle(post(), env(), db.fetchImpl, NOW);
  assert.equal(res.status, 202);
});

test('405 for other methods, 404 for other paths', async () => {
  const db = ok();
  for (const method of ['GET', 'PUT', 'DELETE', 'OPTIONS']) {
    const res = await handle(new Request(URL_, { method }), env(), db.fetchImpl, NOW);
    assert.equal(res.status, 405, method);
    assert.equal(res.headers.get('allow'), 'POST');
  }
  const res = await handle(new Request('https://api.amicu.dev/v1/other', { method: 'POST' }), env(), db.fetchImpl, NOW);
  assert.equal(res.status, 404);
  assert.equal(db.calls.length, 0);
});

test('415 without application/json', async () => {
  const db = ok();
  const res = await handle(post(exampleText, { 'content-type': 'text/plain' }), env(), db.fetchImpl, NOW);
  assert.equal(res.status, 415);
  const charset = await handle(post(exampleText, { 'content-type': 'application/json; charset=utf-8' }), env(), db.fetchImpl, NOW);
  assert.equal(charset.status, 202);
});

test('413 above 64 KB, by header or by actual size', async () => {
  const db = ok();
  const big = 'x'.repeat(MAX_BODY_BYTES + 1);
  const res = await handle(post(big), env(), db.fetchImpl, NOW);
  assert.equal(res.status, 413);
  // A body streamed without a usable content-length is cut off at the cap.
  const stream = new ReadableStream({
    start(c) {
      for (let i = 0; i < 70; i++) c.enqueue(new Uint8Array(1024));
      c.close();
    },
  });
  const streamed = new Request(URL_, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: stream,
    duplex: 'half',
  } as RequestInit);
  assert.equal((await handle(streamed, env(), db.fetchImpl, NOW)).status, 413);
  assert.equal(db.calls.length, 0);
});

test('400 for malformed JSON, schema violations and a mismatched idempotency key, without echo', async () => {
  const db = ok();
  const bad = structuredClone(example);
  bad.reports[0].metrics.prompt = 'tell me a secret';
  for (const [body, headers] of [
    ['{not json', {}],
    [new Uint8Array([0xff, 0xfe, 0x7b]), {}],
    [JSON.stringify(bad), {}],
    [exampleText, { 'idempotency-key': '0a0a0a0a-0a0a-4a0a-8a0a-0a0a0a0a0a0a' }],
  ] as [string | Uint8Array<ArrayBuffer>, Record<string, string>][]) {
    const res = await handle(post(body, headers), env(), db.fetchImpl, NOW);
    assert.equal(res.status, 400);
    const text = await res.text();
    assert.ok(!text.includes('secret'));
    assertNoLeak(res, text);
  }
  assert.equal(db.calls.length, 0);
});

test('400 when the database rejects the batch (PT400)', async () => {
  const db = supabase(() => Response.json({ code: 'PT400', message: 'invalid telemetry batch: x' }, { status: 400 }));
  assert.equal((await handle(post(), env(), db.fetchImpl, NOW)).status, 400);
});

test('503 when Supabase is down, errors, or the Worker is misconfigured', async () => {
  for (const respond of [
    () => new Response('boom', { status: 500 }),
    () => new Response('', { status: 503 }),
    () => Response.json({ message: 'Invalid API key' }, { status: 401 }),
    () => Response.json({ code: 'PGRST202' }, { status: 404 }),
    () => Response.json({ code: 'PGRST100' }, { status: 400 }),
    () => Promise.reject(new TypeError('network down')),
  ]) {
    const res = await handle(post(), env(), supabase(respond).fetchImpl, NOW);
    assert.equal(res.status, 503);
    assert.equal(res.headers.get('retry-after'), '300');
  }
  const db = ok();
  const res = await handle(post(), env({ SUPABASE_SECRET_KEY: undefined }), db.fetchImpl, NOW);
  assert.equal(res.status, 503);
  assert.equal(db.calls.length, 0);
});

test('429 from the per-IP limiter, keyed by a hash of the IP that is never logged', async () => {
  const db = ok();
  const ip = limiter(() => false);
  const res = await handle(post(exampleText, { 'cf-connecting-ip': '203.0.113.7' }), env({ IP_LIMITER: ip }), db.fetchImpl, NOW);
  assert.equal(res.status, 429);
  assert.deepEqual(ip.keys, [await ipKey('203.0.113.7')]);
  assert.ok(!ip.keys[0].includes('203.0.113'));
  assertNoLeak(res, await res.text());
  assert.equal(db.calls.length, 0);
});

test('400 for a batch mixing install ids, before any limiter or database call', async () => {
  const db = ok();
  const install = limiter(() => true);
  const b = structuredClone(example);
  const other = structuredClone(b.reports[0]);
  other.install_id = '0b0b0b0b-0b0b-4b0b-8b0b-0b0b0b0b0b0b';
  b.reports.push(other);
  const res = await handle(post(JSON.stringify(b)), env({ INSTALL_LIMITER: install }), db.fetchImpl, NOW);
  assert.equal(res.status, 400);
  assert.deepEqual(install.keys, []);
  assert.equal(db.calls.length, 0);
});

test('400 for windows that are stale, in the future or unaligned', async () => {
  const db = ok();
  const start = example.reports[0].window.start_unix_s;
  for (const now of [start + 8 * 86400 + 1, start - 601]) {
    assert.equal((await handle(post(), env(), db.fetchImpl, now)).status, 400);
  }
  for (const window of [{ start_unix_s: start + 1, duration_s: 300 }, { start_unix_s: start, duration_s: 60 }]) {
    const b = structuredClone(example);
    b.reports[0].window = window;
    assert.equal((await handle(post(JSON.stringify(b)), env(), db.fetchImpl, NOW)).status, 400);
  }
  assert.equal(db.calls.length, 0);
});

test('429 from the per-install limiter, keyed by install id', async () => {
  const db = ok();
  const install = limiter(() => false);
  const res = await handle(post(), env({ INSTALL_LIMITER: install }), db.fetchImpl, NOW);
  assert.equal(res.status, 429);
  assert.deepEqual(install.keys, [example.reports[0].install_id]);
  assert.equal(db.calls.length, 0);
});

test('429 from the global limiter, before the body is read', async () => {
  const db = ok();
  const global = limiter(() => false);
  const res = await handle(post('x'.repeat(MAX_BODY_BYTES + 10)), env({ GLOBAL_LIMITER: global }), db.fetchImpl, NOW);
  assert.equal(res.status, 429);
  assert.deepEqual(global.keys, ['global']);
});

test('limiters that allow, or that fail, do not block ingestion', async () => {
  const db = ok();
  const broken: RateLimit = {
    limit: () => Promise.reject(new Error('limiter down')),
  };
  const res = await handle(
    post(),
    env({ INSTALL_LIMITER: limiter(() => true), GLOBAL_LIMITER: broken }),
    db.fetchImpl,
    NOW,
  );
  assert.equal(res.status, 202);
});
