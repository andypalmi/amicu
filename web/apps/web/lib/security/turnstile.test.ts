import { test } from 'node:test';
import assert from 'node:assert/strict';
import { verifyTurnstile, turnstileEnabled, SITEVERIFY_URL } from './turnstile.ts';

const json = (body: unknown, status = 200) =>
  (async () => new Response(JSON.stringify(body), { status })) as unknown as typeof fetch;

const base = { secret: 's3cret', action: 'signup', token: 'tok' } as const;

test('disabled when secret unset or blank: skipped, no fetch', async () => {
  const f = (async () => {
    throw new Error('must not be called');
  }) as unknown as typeof fetch;
  for (const secret of [undefined, null, '', '  ']) {
    assert.equal(turnstileEnabled(secret), false);
    const r = await verifyTurnstile({ ...base, secret, token: null, fetchImpl: f });
    assert.deepEqual(r, { ok: true, skipped: true });
  }
});

test('missing / empty / oversized token rejected when enabled', async () => {
  for (const token of [null, undefined, '', 'x'.repeat(2049), new File([], 'a')]) {
    const r = await verifyTurnstile({ ...base, token });
    assert.deepEqual(r, { ok: false, reason: 'missing-token' });
  }
});

test('success: posts secret/response/remoteip and checks action + hostname', async () => {
  let seen: { url: unknown; body: URLSearchParams } | undefined;
  const f = (async (url: unknown, init: RequestInit) => {
    seen = { url, body: init.body as URLSearchParams };
    return new Response(JSON.stringify({ success: true, action: 'signup', hostname: 'amicu.app' }));
  }) as unknown as typeof fetch;
  const r = await verifyTurnstile({
    ...base,
    remoteIp: '1.2.3.4',
    expectedHostname: 'amicu.app',
    fetchImpl: f,
  });
  assert.deepEqual(r, { ok: true, skipped: false });
  assert.equal(seen?.url, SITEVERIFY_URL);
  assert.equal(seen?.body.get('secret'), 's3cret');
  assert.equal(seen?.body.get('response'), 'tok');
  assert.equal(seen?.body.get('remoteip'), '1.2.3.4');
});

test('failure: success=false, wrong action, wrong hostname', async () => {
  const rejected = { ok: false, reason: 'rejected' };
  assert.deepEqual(
    await verifyTurnstile({ ...base, fetchImpl: json({ success: false, 'error-codes': ['invalid-input-response'] }) }),
    rejected,
  );
  assert.deepEqual(
    await verifyTurnstile({ ...base, fetchImpl: json({ success: true, action: 'forgot' }) }),
    rejected,
  );
  assert.deepEqual(
    await verifyTurnstile({
      ...base,
      expectedHostname: 'amicu.app',
      fetchImpl: json({ success: true, action: 'signup', hostname: 'evil.test' }),
    }),
    rejected,
  );
});

test('hostname check skipped when no expected hostname (dev)', async () => {
  const r = await verifyTurnstile({
    ...base,
    fetchImpl: json({ success: true, action: 'signup', hostname: 'localhost' }),
  });
  assert.deepEqual(r, { ok: true, skipped: false });
});

test('fail-closed: timeout, network error, non-2xx, bad JSON', async () => {
  const unavailable = { ok: false, reason: 'unavailable' };
  const hang = ((_u: unknown, init: RequestInit) =>
    new Promise((_res, rej) => {
      // AbortSignal.timeout timers are unref'd; keep the loop alive until abort.
      const keepAlive = setInterval(() => {}, 1000);
      init.signal?.addEventListener('abort', () => {
        clearInterval(keepAlive);
        rej(new Error('aborted'));
      });
    })) as unknown as typeof fetch;
  assert.deepEqual(await verifyTurnstile({ ...base, fetchImpl: hang, timeoutMs: 20 }), unavailable);
  const boom = (async () => {
    throw new Error('net');
  }) as unknown as typeof fetch;
  assert.deepEqual(await verifyTurnstile({ ...base, fetchImpl: boom }), unavailable);
  assert.deepEqual(await verifyTurnstile({ ...base, fetchImpl: json({}, 503) }), unavailable);
  const bad = (async () => new Response('<html>', { status: 200 })) as unknown as typeof fetch;
  assert.deepEqual(await verifyTurnstile({ ...base, fetchImpl: bad }), unavailable);
});
