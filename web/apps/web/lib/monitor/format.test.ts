// Pure-logic tests for the monitoring formatters. Uses Node's built-in test
// runner (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  formatNumber,
  formatMeasure,
  formatPercent,
  formatMoney,
  distributionPoints,
  provenanceLabel,
  formatRange,
  estimateAssumptions,
  looksLikeMonitorView,
  type Distribution,
  type EstimateRange,
  type WorkloadEstimate,
} from './format.ts';

test('formatNumber trims floats and leaves integers intact', () => {
  assert.equal(formatNumber(220), '220');
  assert.equal(formatNumber(0.97), '0.97');
  assert.equal(formatNumber(0.85000001), '0.85');
});

test('formatMeasure appends a unit only when given one', () => {
  assert.equal(formatMeasure(220, 'ms'), '220 ms');
  assert.equal(formatMeasure(6), '6');
});

test('formatPercent scales a [0,1] fraction to a percentage', () => {
  assert.equal(formatPercent(0.97), '97%');
  assert.equal(formatPercent(0.5), '50%');
});

test('formatMoney renders amount and currency from the API', () => {
  assert.equal(formatMoney({ amount: 1.5, currency: 'USD' }), '1.5 USD');
});

test('distributionPoints yields P50/P90/P99 in order', () => {
  const dist: Distribution = { p50: 90, p90: 150, p99: 220 };
  const points = distributionPoints(dist, 'ms');
  assert.deepEqual(
    points.map((p) => p.label),
    ['P50', 'P90', 'P99'],
  );
  assert.equal(points[2].value, '220 ms');
});

test('provenanceLabel maps every provenance to a distinct label', () => {
  const labels = (
    ['telemetry_aggregate', 'public_benchmark', 'provider_price', 'formula'] as const
  ).map(provenanceLabel);
  assert.equal(new Set(labels).size, 4);
  assert.equal(provenanceLabel('public_benchmark'), 'Public benchmark');
});

test('formatRange renders a low–high band with a unit', () => {
  const range: EstimateRange = {
    low: 80,
    high: 300,
    provenance: 'public_benchmark',
    confidence: 'estimated',
  };
  assert.equal(formatRange(range, 'ms'), '80–300 ms');
});

const estimate: WorkloadEstimate = {
  assumed_prompt_tokens: 512,
  assumed_generation_tokens: 256,
  assumed_concurrency: 8,
  expected_ttft_ms: { low: 80, high: 300, provenance: 'public_benchmark', confidence: 'estimated' },
  expected_tpot_ms: { low: 10, high: 30, provenance: 'public_benchmark', confidence: 'estimated' },
};

test('estimateAssumptions prefers echoed assumptions when present', () => {
  const withEchoed: WorkloadEstimate = {
    ...estimate,
    assumptions: [{ statement: 'assumed concurrency 8' }],
  };
  assert.deepEqual(estimateAssumptions(withEchoed), ['assumed concurrency 8']);
});

test('estimateAssumptions falls back to the assumed_* fields', () => {
  const lines = estimateAssumptions(estimate);
  assert.equal(lines.length, 2);
  assert.ok(lines[0].includes('512'));
  assert.ok(lines[1].includes('concurrency 8'));
});

test('looksLikeMonitorView accepts a well-formed view and rejects junk', () => {
  const view = {
    metrics: { deployment: 'chat_prod', latency: {} },
    estimate: { assumed_prompt_tokens: 512 },
    suggestions: { deployment: 'chat_prod', recommendations: [] },
  };
  assert.equal(looksLikeMonitorView(view), true);
  assert.equal(looksLikeMonitorView(null), false);
  assert.equal(looksLikeMonitorView({ metrics: {} }), false);
  assert.equal(
    looksLikeMonitorView({ metrics: { deployment: 'd', latency: {} }, estimate: {}, suggestions: {} }),
    false,
  );
});
