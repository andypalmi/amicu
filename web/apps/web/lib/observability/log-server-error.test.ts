// Pure-logic tests for the server error logger. Uses Node's built-in test runner
// (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  logServerError,
  buildServerErrorReport,
  type ServerErrorReport,
} from './log-server-error.ts';

// Capture the single console.error the transport emits (NOT console.log - the
// logger must never write structured data to stdout) and return what it was handed.
function capture(fn: () => void): { tag: unknown; report: ServerErrorReport; stdoutCalls: number } {
  const originalError = console.error;
  const originalLog = console.log;
  let tag: unknown;
  let report!: ServerErrorReport;
  let stdoutCalls = 0;
  console.error = (t: unknown, r: unknown) => {
    tag = t;
    report = r as ServerErrorReport;
  };
  console.log = () => {
    stdoutCalls += 1;
  };
  try {
    fn();
  } finally {
    console.error = originalError;
    console.log = originalLog;
  }
  return { tag, report, stdoutCalls };
}

test('logServerError emits a single tagged, structured report to stderr (not stdout)', () => {
  const err = new Error('upstream refused');
  const { tag, report, stdoutCalls } = capture(() =>
    logServerError(err, { route: 'POST /api/engine/plan', op: 'engine.plan' }),
  );

  assert.equal(tag, '[amicu:server-error]');
  assert.equal(report.message, 'upstream refused');
  assert.equal(report.name, 'Error');
  assert.equal(report.route, 'POST /api/engine/plan');
  assert.equal(report.op, 'engine.plan');
  assert.equal(stdoutCalls, 0);
  assert.ok(!Number.isNaN(Date.parse(report.timestamp)));
});

test('logServerError stringifies a non-Error throwable without throwing', () => {
  assert.doesNotThrow(() => {
    capture(() => logServerError({ code: 'ECONNREFUSED' }, { op: 'monitor' }));
  });
  const { report } = capture(() => logServerError({ code: 'ECONNREFUSED' }, { op: 'monitor' }));
  assert.equal(report.message, '{"code":"ECONNREFUSED"}');
  assert.equal(report.name, undefined);
  assert.equal(report.op, 'monitor');
});

test('buildServerErrorReport tolerates string, null, and circular throwables', () => {
  assert.equal(buildServerErrorReport('boom').message, 'boom');
  assert.equal(buildServerErrorReport(null).message, 'null');

  const circular: Record<string, unknown> = {};
  circular.self = circular;
  // Must not throw on a value JSON.stringify cannot serialize.
  const report = buildServerErrorReport(circular, { route: 'POST /api/recommend' });
  assert.equal(typeof report.message, 'string');
  assert.equal(report.route, 'POST /api/recommend');
});

test('org and billing call-site tags flow through to the report verbatim', () => {
  const org = buildServerErrorReport(new Error('down'), {
    route: 'GET /api/org/members',
    op: 'org.members.list',
  });
  assert.equal(org.route, 'GET /api/org/members');
  assert.equal(org.op, 'org.members.list');

  const billing = buildServerErrorReport(new Error('down'), {
    route: 'POST /api/billing/checkout',
    op: 'billing.checkout',
  });
  assert.equal(billing.route, 'POST /api/billing/checkout');
  assert.equal(billing.op, 'billing.checkout');
});

test('buildServerErrorReport omits tags when no context is given', () => {
  const report = buildServerErrorReport(new Error('x'));
  assert.equal(report.route, undefined);
  assert.equal(report.op, undefined);
});

test('registered sink receives errors and cannot break logging', async () => {
  const { registerServerErrorSink, logServerError } = await import('./log-server-error.ts');
  const seen: unknown[] = [];
  const original = console.error;
  console.error = () => {};
  try {
    registerServerErrorSink((e) => seen.push(e));
    logServerError(new Error('x'), { route: 'r' });
    registerServerErrorSink(() => {
      throw new Error('sink down');
    });
    logServerError(new Error('y'));
  } finally {
    registerServerErrorSink(null);
    console.error = original;
  }
  assert.equal(seen.length, 1);
});
