import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildSentryOptions, redactText, scrubEvent, stripUrlQuery } from './sentry-options.ts';

test('disabled without a DSN', () => {
  for (const dsn of [undefined, '', '   ']) {
    const o = buildSentryOptions(dsn);
    assert.equal(o.enabled, false);
    assert.equal(o.dsn, undefined);
  }
});

test('enabled with a DSN, errors only, no PII', () => {
  const o = buildSentryOptions('https://k@o1.ingest.sentry.io/2');
  assert.equal(o.enabled, true);
  assert.equal(o.sendDefaultPii, false);
  assert.equal(o.tracesSampleRate, 0);
  assert.equal(o.maxBreadcrumbs, 0);
});

test('stripUrlQuery drops query and fragment', () => {
  assert.equal(stripUrlQuery('https://a.dev/verify?token=s3cret#x'), 'https://a.dev/verify');
  assert.equal(stripUrlQuery('https://a.dev/p'), 'https://a.dev/p');
});

test('scrubEvent removes user, ip, cookies, headers, body, query and breadcrumbs', () => {
  const event = scrubEvent({
    message: 'boom',
    user: { id: '1', email: 'a@b.c', ip_address: '1.2.3.4' },
    breadcrumbs: [{ message: 'x' }],
    server_name: 'host',
    request: {
      url: 'https://amicu.dev/verify?token=s3cret',
      method: 'POST',
      headers: { cookie: 'x' },
      cookies: { a: 'b' },
      data: { password: 'p' },
      query_string: 'token=s3cret',
    },
  });
  assert.equal(event.message, 'boom');
  assert.equal(event.user, undefined);
  assert.equal(event.breadcrumbs, undefined);
  assert.equal(event.server_name, undefined);
  assert.deepEqual(event.request, { url: 'https://amicu.dev/verify', method: 'POST' });
});

test("scrubEvent drops extra and non-allowlisted contexts, redacts text", () => {
  const event = scrubEvent({
    message: "failed for a@b.co at https://amicu.dev/verify?token=abc",
    extra: { body: "secret" },
    contexts: { runtime: { name: "node" }, os: { name: "mac" }, state: { x: 1 }, trace: {} },
    exception: { values: [{ value: "bad code=123 for bob@x.io" }] },
  });
  assert.equal(event.extra, undefined);
  assert.deepEqual(Object.keys(event.contexts ?? {}).sort(), ["os", "runtime"]);
  assert.equal(event.message, "failed for [email] at https://amicu.dev/verify");
  assert.equal(event.exception?.values?.[0].value, "bad code=[redacted] for [email]");
});

test("redactText leaves plain text alone", () => {
  assert.equal(redactText("Cannot read x of undefined"), "Cannot read x of undefined");
});
