// Pure-logic tests for the recommendation formatters. Uses Node's built-in test
// runner (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  confidenceMeta,
  bottleneckLabel,
  directionMeta,
  comparisonSymbol,
  formatNumber,
  formatEvidence,
  isTierDenied,
  type Evidence,
} from './format.ts';

test('confidenceMeta maps every level to a distinct band label', () => {
  const labels = (['high', 'medium', 'low', 'estimated'] as const).map(
    (c) => confidenceMeta(c).label,
  );
  assert.equal(new Set(labels).size, 4);
  assert.equal(confidenceMeta('high').tone, 'default');
});

test('bottleneckLabel gives a distinct human label per class', () => {
  const labels = (
    ['under_provisioned', 'over_provisioned', 'prefill_bound', 'decode_bound', 'preemption_recompute'] as const
  ).map(bottleneckLabel);
  assert.equal(new Set(labels).size, 5);
  assert.equal(bottleneckLabel('under_provisioned'), 'Under-provisioned');
});

test('directionMeta gives a distinct sigil per direction', () => {
  const sigils = (['increase', 'decrease', 'enable', 'replace'] as const).map(
    (d) => directionMeta(d).sigil,
  );
  assert.equal(new Set(sigils).size, 4);
});

test('comparisonSymbol reflects above/below', () => {
  assert.equal(comparisonSymbol('above'), '>');
  assert.equal(comparisonSymbol('below'), '<');
});

test('formatNumber trims floats and leaves integers intact', () => {
  assert.equal(formatNumber(1200), '1200');
  assert.equal(formatNumber(0.95), '0.95');
  assert.equal(formatNumber(0.85000001), '0.85');
});

test('formatEvidence renders metric, observed, comparator, and threshold', () => {
  const ev: Evidence = {
    metric: 'queue.wait_ms.p99',
    observed: 1200,
    threshold: 500,
    comparison: 'above',
  };
  assert.equal(formatEvidence(ev), 'queue.wait_ms.p99 = 1200 > 500');

  const below: Evidence = {
    metric: 'gpu.sm_utilization',
    observed: 0.1,
    threshold: 0.35,
    comparison: 'below',
  };
  assert.equal(formatEvidence(below), 'gpu.sm_utilization = 0.1 < 0.35');
});

test('isTierDenied is true only for 403', () => {
  assert.equal(isTierDenied(403), true);
  assert.equal(isTierDenied(401), false);
  assert.equal(isTierDenied(200), false);
});
