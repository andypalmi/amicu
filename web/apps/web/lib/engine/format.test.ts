// Pure-logic tests for the engine result formatters. Uses Node's built-in test
// runner (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  groupBySeverity,
  severityCounts,
  summarizePlan,
  actionKindMeta,
  isTierDenied,
  type Diagnostic,
  type PlannedAction,
} from './format.ts';

const diag = (severity: Diagnostic['severity'], code: string): Diagnostic => ({
  code,
  message: `${code} message`,
  severity,
});

test('groupBySeverity buckets by severity and preserves order', () => {
  const input = [diag('warning', 'W1'), diag('error', 'E1'), diag('warning', 'W2')];
  const g = groupBySeverity(input);
  assert.deepEqual(
    g.error.map((d) => d.code),
    ['E1'],
  );
  assert.deepEqual(
    g.warning.map((d) => d.code),
    ['W1', 'W2'],
  );
  assert.deepEqual(g.info, []);
});

test('severityCounts totals each bucket', () => {
  const counts = severityCounts([diag('error', 'E1'), diag('info', 'I1'), diag('info', 'I2')]);
  assert.deepEqual(counts, { error: 1, warning: 0, info: 2 });
});

test('summarizePlan counts by kind in a stable order', () => {
  const action = (kind: PlannedAction['kind'], address: string): PlannedAction => ({
    address,
    kind,
  });
  const summary = summarizePlan([
    action('destroy', 'a'),
    action('create', 'b'),
    action('create', 'c'),
    action('update', 'd'),
  ]);
  // create, update, replace, destroy - replace absent so it's omitted.
  assert.deepEqual(summary, [
    { kind: 'create', count: 2 },
    { kind: 'update', count: 1 },
    { kind: 'destroy', count: 1 },
  ]);
});

test('actionKindMeta gives a distinct sigil per kind', () => {
  const sigils = (['create', 'update', 'replace', 'destroy'] as const).map(
    (k) => actionKindMeta(k).sigil,
  );
  assert.equal(new Set(sigils).size, 4);
});

test('isTierDenied is true only for 403', () => {
  assert.equal(isTierDenied(403), true);
  assert.equal(isTierDenied(401), false);
  assert.equal(isTierDenied(200), false);
});
