// Pure-logic tests for the client error reporter. Uses Node's built-in test
// runner (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { reportClientError, type ClientErrorReport } from './report-error.ts';

// Capture the single console.error the stub transport emits and return the
// structured report it was handed.
function capture(fn: () => void): { tag: unknown; report: ClientErrorReport } {
  const original = console.error;
  let tag: unknown;
  let report!: ClientErrorReport;
  console.error = (t: unknown, r: unknown) => {
    tag = t;
    report = r as ClientErrorReport;
  };
  try {
    fn();
  } finally {
    console.error = original;
  }
  return { tag, report };
}

test('reportClientError emits a single tagged, structured report', () => {
  const err = new Error('boom');
  const { tag, report } = capture(() => reportClientError(err, 'route-error'));

  assert.equal(tag, '[amicu:client-error]');
  assert.equal(report.message, 'boom');
  assert.equal(report.context, 'route-error');
  assert.equal(typeof report.timestamp, 'string');
  assert.ok(!Number.isNaN(Date.parse(report.timestamp)));
});

test('reportClientError forwards a Next.js digest when present', () => {
  const err = Object.assign(new Error('catastrophe'), { digest: 'abc123' });
  const { report } = capture(() => reportClientError(err, 'global-error'));

  assert.equal(report.digest, 'abc123');
  assert.equal(report.context, 'global-error');
});

test('reportClientError tolerates a missing context and digest', () => {
  const err = new Error('no context');
  const { report } = capture(() => reportClientError(err));

  assert.equal(report.context, undefined);
  assert.equal(report.digest, undefined);
  assert.equal(report.message, 'no context');
});
