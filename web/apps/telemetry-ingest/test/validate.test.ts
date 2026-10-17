import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { test } from 'node:test';

import { HISTOGRAM_BOUNDS_MS, InvalidBatch, looksLikeCredential, looksLikeIpv4, validateBatch } from '../src/validate.ts';

const here = new URL('.', import.meta.url);
const example = JSON.parse(readFileSync(new URL('fixtures/telemetry-batch.v2.example.json', here), 'utf8'));

function clone() {
  return structuredClone(example);
}

function rejects(batch: unknown, what: string) {
  assert.throws(() => validateBatch(batch as never), InvalidBatch, what);
}

test('the golden example from the sender validates', () => {
  const ok = validateBatch(clone());
  assert.equal(ok.batch_id, example.batch_id);
  assert.equal(ok.reports.length, 1);
});

test('the bundled schema is the sender schema, byte for byte, when both are present', () => {
  const crate = new URL('../../../../crates/amicu-telemetry/schema/', here);
  for (const [bundled, upstream] of [
    ['../src/schema/telemetry-batch.v2.schema.json', 'telemetry-batch.v2.schema.json'],
    ['fixtures/telemetry-batch.v2.example.json', 'telemetry-batch.v2.example.json'],
  ]) {
    const up = new URL(upstream, crate);
    if (!existsSync(up)) continue; // the sender crate is not on this branch yet
    assert.equal(readFileSync(new URL(bundled, here), 'utf8'), readFileSync(up, 'utf8'), `${upstream} drifted`);
  }
});

test('unknown fields are rejected at every level', () => {
  rejects({ ...clone(), extra: 1 }, 'batch');
  for (const path of [[], ['hardware'], ['model'], ['runtime'], ['window'], ['metrics'], ['metrics', 'ttft_ms']]) {
    const b = clone();
    let node = b.reports[0];
    for (const key of path) node = node[key];
    node.client_ip = '203.0.113.9';
    rejects(b, path.join('.') || 'report');
  }
});

test('structural violations are rejected', () => {
  const cases: [string, (b: ReturnType<typeof clone>) => void][] = [
    ['schema_version', (b) => (b.schema_version = 1)],
    ['report schema_version', (b) => (b.reports[0].schema_version = 3)],
    ['batch_id not uuid v4', (b) => (b.batch_id = '03030303-0303-1303-8303-030303030303')],
    ['empty reports', (b) => (b.reports = [])],
    ['too many reports', (b) => (b.reports = Array.from({ length: 289 }, () => b.reports[0]))],
    ['missing metrics', (b) => delete b.reports[0].metrics],
    ['label charset', (b) => (b.reports[0].model.name = 'two words')],
    ['label length', (b) => (b.reports[0].model.name = 'a'.repeat(65))],
    ['negative count', (b) => (b.reports[0].metrics.requests = -1)],
    ['fractional count', (b) => (b.reports[0].metrics.requests = 1.5)],
    ['fraction > 1', (b) => (b.reports[0].metrics.gpu_utilization = 1.01)],
    ['window too old', (b) => (b.reports[0].window.start_unix_s = 1)],
    ['window too long', (b) => (b.reports[0].window.duration_s = 86401)],
    ['string number', (b) => (b.reports[0].metrics.requests = '120')],
    ['null field', (b) => (b.reports[0].region = null)],
  ];
  for (const [what, mutate] of cases) {
    const b = clone();
    mutate(b);
    rejects(b, what);
  }
});

test('semantic checks mirror report.rs', () => {
  const cases: [string, (b: ReturnType<typeof clone>) => void][] = [
    ['histogram length', (b) => (b.reports[0].metrics.ttft_ms.counts = [1, 2, 3])],
    ['histogram sum_ms too large', (b) => (b.reports[0].metrics.ttft_ms.sum_ms = 1e15)],
    ['histogram count too large', (b) => (b.reports[0].metrics.e2e_ms.counts[0] = 1e11)],
    ['errors > requests', (b) => (b.reports[0].metrics.errors = 121)],
    ['errors without requests', (b) => delete b.reports[0].metrics.requests],
    ['requests too large', (b) => (b.reports[0].metrics.requests = 1e11)],
    ['bytes too large', (b) => (b.reports[0].hardware.vram_bytes = 2 ** 51)],
    ['accelerator without count', (b) => (b.reports[0].hardware.accelerator_count = 0)],
    ['ip-shaped label', (b) => (b.reports[0].region = '10.1.2.3')],
    ['credential label', (b) => (b.reports[0].model.name = 'hf_abcdefghijklmnop')],
  ];
  for (const [what, mutate] of cases) {
    const b = clone();
    mutate(b);
    rejects(b, what);
  }
});

test('histogram layouts come from the schema x-amicu key', () => {
  assert.equal(HISTOGRAM_BOUNDS_MS.ttft_ms.length + 1, example.reports[0].metrics.ttft_ms.counts.length);
  assert.equal(HISTOGRAM_BOUNDS_MS.e2e_ms.length + 1, example.reports[0].metrics.e2e_ms.counts.length);
  assert.equal(HISTOGRAM_BOUNDS_MS.tpot_ms.length + 1, example.reports[0].metrics.tpot_ms.counts.length);
});

test('label heuristics match the Rust ones', () => {
  assert.ok(looksLikeIpv4('192.168.1.10'));
  assert.ok(looksLikeIpv4('host-8.8.8.8'));
  assert.ok(!looksLikeIpv4('0.6.3.1'));
  assert.ok(!looksLikeIpv4('0.11.0'));
  assert.ok(!looksLikeIpv4('300.1.1.1'));
  assert.ok(looksLikeCredential('org/sk-something'));
  assert.ok(looksLikeCredential('a'.repeat(16) + '0123456789abcdef'));
  assert.ok(!looksLikeCredential('meta-llama/Llama-3.1-8B-Instruct'));
  assert.ok(!looksLikeCredential('h100-80gb'));
});

test('errors carry a field path, never an input value', () => {
  const b = clone();
  b.reports[0].region = 'secret value here';
  try {
    validateBatch(b);
    assert.fail('should reject');
  } catch (e) {
    assert.ok(e instanceof InvalidBatch);
    assert.ok(!e.message.includes('secret'));
  }
});
