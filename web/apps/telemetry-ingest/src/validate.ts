// Validation of an uploaded telemetry batch, in two layers:
//
// 1. Structural: a small interpreter for the JSON Schema subset that
//    schema/telemetry-batch.v2.schema.json uses (a copy of the sender's
//    crates/amicu-telemetry/schema/telemetry-batch.v2.schema.json, bundled at
//    build time; nothing is fetched at runtime). An unsupported keyword fails
//    at module load, so a schema change can never silently weaken validation.
// 2. Semantic: the plausibility checks of TelemetryReport::validate() in
//    crates/amicu-telemetry/src/report.rs that JSON Schema cannot express
//    (histogram lengths, upper bounds, errors <= requests, IP- and
//    credential-shaped labels).
//
// Errors carry a field path only, never a value from the input.

import schemaJson from './schema/telemetry-batch.v2.schema.json' with { type: 'json' };

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Schema = { [key: string]: unknown };

export class InvalidBatch extends Error {}

const schema = schemaJson as unknown as Schema;
const defs = (schema.$defs ?? {}) as Record<string, Schema>;

// Keywords the interpreter enforces, and annotations it may ignore.
const ENFORCED = new Set([
  'type', 'const', 'required', 'properties', 'additionalProperties', 'items', '$ref',
  'pattern', 'minLength', 'maxLength', 'minimum', 'maximum', 'minItems', 'maxItems',
  'multipleOf',
]);
const ANNOTATIONS = new Set(['$schema', '$id', '$defs', 'title', 'description', 'x-amicu']);

function checkKeywords(node: Schema, path: string): void {
  for (const [key, value] of Object.entries(node)) {
    if (!ENFORCED.has(key) && !ANNOTATIONS.has(key)) {
      throw new Error(`telemetry schema keyword not supported by the validator: ${key} at ${path}`);
    }
    if (key === 'additionalProperties' && value !== false) {
      throw new Error(`telemetry schema: additionalProperties must be false at ${path}`);
    }
  }
  for (const [name, child] of Object.entries((node.properties ?? {}) as Record<string, Schema>)) {
    checkKeywords(child, `${path}.${name}`);
  }
  if (node.items) checkKeywords(node.items as Schema, `${path}[]`);
}
checkKeywords(schema, '#');
for (const [name, def] of Object.entries(defs)) checkKeywords(def, `#/$defs/${name}`);

// Patterns compiled once.
const patterns = new Map<string, RegExp>();
function regex(source: string): RegExp {
  let re = patterns.get(source);
  if (!re) {
    re = new RegExp(source, 'u');
    patterns.set(source, re);
  }
  return re;
}

function isObject(v: Json | undefined): v is { [key: string]: Json } {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function typeOk(type: string, v: Json): boolean {
  switch (type) {
    case 'object':
      return isObject(v);
    case 'array':
      return Array.isArray(v);
    case 'string':
      return typeof v === 'string';
    case 'integer':
      return typeof v === 'number' && Number.isInteger(v);
    case 'number':
      return typeof v === 'number' && Number.isFinite(v);
    case 'boolean':
      return typeof v === 'boolean';
    default:
      throw new Error(`telemetry schema type not supported: ${type}`);
  }
}

function fail(path: string): never {
  throw new InvalidBatch(path);
}

function check(node: Schema, v: Json, path: string): void {
  if (typeof node.$ref === 'string') {
    const name = node.$ref.replace(/^#\/\$defs\//, '');
    const target = defs[name];
    if (!target) throw new Error(`telemetry schema: unresolved $ref ${node.$ref}`);
    check(target, v, path);
  }
  if (typeof node.type === 'string' && !typeOk(node.type, v)) fail(path);
  if ('const' in node && v !== node.const) fail(path);
  if (typeof v === 'number') {
    if (typeof node.minimum === 'number' && v < node.minimum) fail(path);
    if (typeof node.maximum === 'number' && v > node.maximum) fail(path);
    if (typeof node.multipleOf === 'number' && v % node.multipleOf !== 0) fail(path);
  }
  if (typeof v === 'string') {
    const len = [...v].length;
    if (typeof node.minLength === 'number' && len < node.minLength) fail(path);
    if (typeof node.maxLength === 'number' && len > node.maxLength) fail(path);
    if (typeof node.pattern === 'string' && !regex(node.pattern).test(v)) fail(path);
  }
  if (Array.isArray(v)) {
    if (typeof node.minItems === 'number' && v.length < node.minItems) fail(path);
    if (typeof node.maxItems === 'number' && v.length > node.maxItems) fail(path);
    if (node.items) v.forEach((item, i) => check(node.items as Schema, item, `${path}[${i}]`));
  }
  if (isObject(v)) {
    const props = (node.properties ?? {}) as Record<string, Schema>;
    for (const key of (node.required ?? []) as string[]) {
      if (!Object.hasOwn(v, key)) fail(`${path}.${key}`);
    }
    for (const [key, child] of Object.entries(v)) {
      // Own properties only: `__proto__` or `constructor` must not resolve
      // through Object.prototype to something that looks like a sub-schema.
      const sub = Object.hasOwn(props, key) ? props[key] : undefined;
      if (!sub) {
        if (node.additionalProperties === false) fail(`${path}: unknown field`);
        continue;
      }
      check(sub, child, `${path}.${key}`);
    }
  }
}

// ---------------------------------------------------------------------------
// Semantic checks, mirroring crates/amicu-telemetry/src/report.rs.
// ---------------------------------------------------------------------------

const MAX_REQUESTS = 10_000_000_000;
const GIB = 2 ** 30;
/** The sender's rollup window: aligned 5-minute windows only. */
export const WINDOW_S = 300;
/** Oldest accepted window start, relative to now (the sender buffers 7 days). */
export const MAX_WINDOW_AGE_S = 8 * 86_400;
/** Newest accepted window start, relative to now (clock skew). */
export const MAX_WINDOW_LEAD_S = 600;
const MAX_BYTES = 2 ** 50;
const MAX_TOKENS = 2 ** 50;
const MAX_LATENCY_MS = 86_400_000;
const MAX_CONCURRENCY = 2 ** 20;
const U32_MAX = 4_294_967_295;

const layouts = (schema['x-amicu'] ?? {}) as Record<string, unknown>;
function bounds(key: string): number[] {
  const b = layouts[key];
  if (!Array.isArray(b) || b.length === 0) throw new Error(`telemetry schema: missing x-amicu.${key}`);
  return b as number[];
}
export const HISTOGRAM_BOUNDS_MS = {
  ttft_ms: bounds('ttft_bounds_ms'),
  e2e_ms: bounds('e2e_bounds_ms'),
  tpot_ms: bounds('tpot_bounds_ms'),
} as const;

/** Port of `looks_like_ipv4` in report.rs. */
export function looksLikeIpv4(s: string): boolean {
  return s.split(/[^0-9.]/).some((run) => {
    const parts = run.split('.');
    for (let i = 0; i + 4 <= parts.length; i++) {
      const octets = parts.slice(i, i + 4).map((p) =>
        p.length > 0 && p.length <= 3 && /^[0-9]+$/.test(p) && Number(p) <= 255 ? Number(p) : null,
      );
      if (octets.every((o) => o !== null) && octets[0] !== 0) return true;
    }
    return false;
  });
}

const CREDENTIAL_PREFIXES = [
  'hf_', 'sk-', 'sk_', 'ghp_', 'gho_', 'ghu_', 'ghs_', 'ghr_', 'github_pat_', 'glpat-',
  'xoxb-', 'xoxp-', 'akia', 'asia', 'aiza', 'ya29.',
];

/** Port of `looks_like_credential` in report.rs. */
export function looksLikeCredential(s: string): boolean {
  const lower = s.toLowerCase();
  if (lower.split('/').some((seg) => CREDENTIAL_PREFIXES.some((p) => seg.startsWith(p)))) return true;
  if (lower.length >= 32 && /^[0-9a-f]+$/.test(lower)) return true;
  return lower
    .split(/[^a-z0-9]/)
    .some((run) => run.length >= 40 && /[0-9]/.test(run) && /[a-z]/.test(run));
}

function label(v: Json | undefined, path: string): void {
  if (typeof v === 'string' && (looksLikeIpv4(v) || looksLikeCredential(v))) fail(path);
}

function max(v: Json | undefined, limit: number, path: string): void {
  if (typeof v === 'number' && v > limit) fail(path);
}

function histogram(v: Json | undefined, layout: number[], path: string): void {
  if (!isObject(v)) return;
  const counts = v.counts as number[];
  if (counts.length !== layout.length + 1) fail(`${path}.counts`);
  let total = 0;
  for (const c of counts) {
    if (c > MAX_REQUESTS) fail(`${path}.counts`);
    total += c;
  }
  if (total > MAX_REQUESTS) fail(`${path}.counts`);
  if ((v.sum_ms as number) > MAX_LATENCY_MS * total) fail(`${path}.sum_ms`);
}

function semantic(batch: { [key: string]: Json }, nowS: number): void {
  const reports = batch.reports as { [key: string]: Json }[];
  // One install per batch (the sender keeps one queue per deployment). Stops
  // a single request from minting hundreds of installs.
  if (reports.some((r) => r.install_id !== reports[0].install_id)) fail('reports.install_id');
  reports.forEach((r, i) => {
    const p = `reports[${i}]`;
    const w = r.window as { start_unix_s: number; duration_s: number };
    if (w.duration_s !== WINDOW_S || w.start_unix_s % WINDOW_S !== 0) fail(`${p}.window`);
    if (w.start_unix_s < nowS - MAX_WINDOW_AGE_S || w.start_unix_s > nowS + MAX_WINDOW_LEAD_S) {
      fail(`${p}.window.start_unix_s`);
    }
    const hw = (r.hardware ?? {}) as { [key: string]: Json };
    const model = r.model as { [key: string]: Json };
    const runtime = r.runtime as { [key: string]: Json };
    const m = r.metrics as { [key: string]: Json };

    label(r.amicu_version, `${p}.amicu_version`);
    label(hw.accelerator, `${p}.hardware.accelerator`);
    label(model.name, `${p}.model.name`);
    label(model.quantization, `${p}.model.quantization`);
    label(runtime.name, `${p}.runtime.name`);
    label(runtime.version, `${p}.runtime.version`);
    label(r.provider, `${p}.provider`);
    label(r.region, `${p}.region`);

    max(hw.vram_bytes, MAX_BYTES, `${p}.hardware.vram_bytes`);
    max(hw.memory_bytes, MAX_BYTES, `${p}.hardware.memory_bytes`);
    // Host memory is sent as a power-of-two number of GiB (anti-fingerprinting).
    if (typeof hw.memory_bytes === 'number' && hw.memory_bytes > 0) {
      const gib = hw.memory_bytes / GIB;
      if (!Number.isInteger(gib) || (gib & (gib - 1)) !== 0 || gib > 2 ** 20) fail(`${p}.hardware.memory_bytes`);
    }
    if (hw.accelerator !== undefined && !((hw.accelerator_count as number) > 0)) {
      fail(`${p}.hardware.accelerator_count`);
    }

    max(m.requests, MAX_REQUESTS, `${p}.metrics.requests`);
    const requests = typeof m.requests === 'number' ? m.requests : 0;
    if (typeof m.errors === 'number' && m.errors > requests) fail(`${p}.metrics.errors`);
    max(m.prompt_tokens, MAX_TOKENS, `${p}.metrics.prompt_tokens`);
    max(m.output_tokens, MAX_TOKENS, `${p}.metrics.output_tokens`);
    max(m.preemptions, MAX_REQUESTS, `${p}.metrics.preemptions`);
    max(m.running_peak, Math.min(MAX_CONCURRENCY, U32_MAX), `${p}.metrics.running_peak`);
    max(m.waiting_peak, Math.min(MAX_CONCURRENCY, U32_MAX), `${p}.metrics.waiting_peak`);
    max(m.gpu_memory_peak_bytes, MAX_BYTES, `${p}.metrics.gpu_memory_peak_bytes`);
    max(m.host_memory_peak_bytes, MAX_BYTES, `${p}.metrics.host_memory_peak_bytes`);
    histogram(m.ttft_ms, HISTOGRAM_BOUNDS_MS.ttft_ms, `${p}.metrics.ttft_ms`);
    histogram(m.e2e_ms, HISTOGRAM_BOUNDS_MS.e2e_ms, `${p}.metrics.e2e_ms`);
    histogram(m.tpot_ms, HISTOGRAM_BOUNDS_MS.tpot_ms, `${p}.metrics.tpot_ms`);
  });
}

export type ValidBatch = {
  schema_version: 2;
  batch_id: string;
  reports: { install_id: string; [key: string]: unknown }[];
};

/**
 * Validate a parsed batch against the schema and the plausibility rules,
 * with window times checked against `nowS` (Unix seconds). Throws
 * InvalidBatch (field path only) on failure.
 */
export function validateBatch(value: Json, nowS: number = Date.now() / 1000): ValidBatch {
  check(schema, value, '$');
  semantic(value as { [key: string]: Json }, nowS);
  return value as unknown as ValidBatch;
}
