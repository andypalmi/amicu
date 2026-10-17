// POST /v1/telemetry/rollups: accept a schema-v2 telemetry batch from an
// opted-in amicu deployment and store it through the Supabase Data API.
//
// Status codes, as the sender (crates/amicu-telemetry/src/upload.rs) reads
// them: 202 stored; 400 / 413 never retried (the batch is dropped);
// 405 / 415 / 429 / 503 retried with backoff.
//
// Never logged or echoed: the payload, any field of it, and the client IP.
// No CORS headers: the only client is the CLI sidecar, not a browser.

import { InvalidBatch, validateBatch } from './validate.ts';

export const ROUTE = '/v1/telemetry/rollups';
export const MAX_BODY_BYTES = 64 * 1024;
const RPC_PATH = '/rest/v1/rpc/ingest_rollup_batch';
const RPC_TIMEOUT_MS = 10_000;

/** The Workers Rate Limiting binding (the subset used here). */
export interface RateLimit {
  limit(options: { key: string }): Promise<{ success: boolean }>;
}

export interface Env {
  /** Supabase project URL (wrangler.jsonc var, not secret). */
  SUPABASE_URL: string;
  /** Supabase secret key (`sb_secret_...`), a Worker secret. Maps to service_role. */
  SUPABASE_SECRET_KEY?: string;
  /** Per-install limiter. Optional so the Worker still runs if the binding is removed. */
  INSTALL_LIMITER?: RateLimit;
  /** Per-client-IP limiter, keyed by a hash of the IP. */
  IP_LIMITER?: RateLimit;
  /** Coarse limiter over all traffic (per Cloudflare location, approximate). */
  GLOBAL_LIMITER?: RateLimit;
}

type Fetch = (input: string, init: RequestInit) => Promise<Response>;

function reply(status: number, error?: string, extra: Record<string, string> = {}): Response {
  const body = error === undefined ? { status: 'accepted' } : { error };
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      'content-type': 'application/json',
      'cache-control': 'no-store',
      ...extra,
    },
  });
}

function log(event: string, fields: Record<string, string | number | boolean> = {}): void {
  console.log(JSON.stringify({ event, ...fields }));
}

/** Read at most `limit` bytes; `null` when the body is larger. */
async function readCapped(request: Request, limit: number): Promise<Uint8Array | null> {
  if (!request.body) return new Uint8Array();
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > limit) {
      await reader.cancel();
      return null;
    }
    chunks.push(value);
  }
  const out = new Uint8Array(size);
  let offset = 0;
  for (const c of chunks) {
    out.set(c, offset);
    offset += c.byteLength;
  }
  return out;
}

/**
 * Rate-limit key for a client IP: a truncated SHA-256, so the IP itself never
 * reaches the limiter, a log line or storage.
 */
export async function ipKey(ip: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(`amicu-telemetry-rl:${ip}`));
  return 'ip:' + [...new Uint8Array(digest).slice(0, 12)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

async function allowed(limiter: RateLimit | undefined, key: string): Promise<boolean> {
  if (!limiter) return true;
  try {
    return (await limiter.limit({ key })).success;
  } catch {
    // A limiter outage must not take ingestion down; the database is the
    // backstop (one row per deployment and window).
    return true;
  }
}

export async function handle(
  request: Request,
  env: Env,
  fetchImpl: Fetch = fetch,
  nowS: number = Date.now() / 1000,
): Promise<Response> {
  const url = new URL(request.url);
  if (url.pathname !== ROUTE) return reply(404, 'not found');
  if (request.method !== 'POST') return reply(405, 'method not allowed', { allow: 'POST' });

  const contentType = (request.headers.get('content-type') ?? '').split(';')[0].trim().toLowerCase();
  if (contentType !== 'application/json') return reply(415, 'content-type must be application/json');

  if (!(await allowed(env.GLOBAL_LIMITER, 'global'))) {
    log('rate_limited', { scope: 'global' });
    return reply(429, 'rate limited', { 'retry-after': '60' });
  }

  // Cloudflare sets CF-Connecting-IP on every request it proxies.
  const ip = request.headers.get('cf-connecting-ip');
  if (ip && !(await allowed(env.IP_LIMITER, await ipKey(ip)))) {
    log('rate_limited', { scope: 'ip' });
    return reply(429, 'rate limited', { 'retry-after': '60' });
  }

  const declared = Number(request.headers.get('content-length') ?? '0');
  if (declared > MAX_BODY_BYTES) return reply(413, 'body too large');
  const bytes = await readCapped(request, MAX_BODY_BYTES);
  if (bytes === null) return reply(413, 'body too large');

  let text: string;
  let batch;
  try {
    text = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    batch = validateBatch(JSON.parse(text), nowS);
  } catch (e) {
    if (e instanceof InvalidBatch || e instanceof SyntaxError || e instanceof TypeError) {
      log('rejected', { reason: e instanceof InvalidBatch ? 'schema' : 'json' });
      return reply(400, 'invalid telemetry batch');
    }
    throw e;
  }

  const idempotencyKey = request.headers.get('idempotency-key');
  if (idempotencyKey !== null && idempotencyKey !== batch.batch_id) {
    log('rejected', { reason: 'idempotency_key_mismatch' });
    return reply(400, 'idempotency-key does not match batch_id');
  }

  // Every report carries the same install id (checked in validateBatch).
  if (!(await allowed(env.INSTALL_LIMITER, batch.reports[0].install_id))) {
    log('rate_limited', { scope: 'install' });
    return reply(429, 'rate limited', { 'retry-after': '60' });
  }

  if (!env.SUPABASE_URL || !env.SUPABASE_SECRET_KEY) {
    log('misconfigured');
    return reply(503, 'unavailable', { 'retry-after': '300' });
  }

  let rpc: Response;
  try {
    rpc = await fetchImpl(env.SUPABASE_URL.replace(/\/+$/, '') + RPC_PATH, {
      method: 'POST',
      headers: {
        // New-style secret keys go in `apikey` only; they are not JWTs, so no
        // `Authorization: Bearer`. A non-browser User-Agent: secret keys are
        // refused for browser-like clients.
        apikey: env.SUPABASE_SECRET_KEY,
        'content-type': 'application/json',
        accept: 'application/json',
        'user-agent': 'amicu-telemetry-ingest/1',
      },
      // The validated body, forwarded byte for byte as the `batch` argument.
      body: `{"batch":${text}}`,
      signal: AbortSignal.timeout(RPC_TIMEOUT_MS),
    });
  } catch {
    log('upstream_error', { kind: 'network' });
    return reply(503, 'unavailable', { 'retry-after': '300' });
  }

  if (rpc.ok) {
    let duplicate = false;
    try {
      duplicate = Boolean(((await rpc.json()) as { duplicate?: boolean }).duplicate);
    } catch {
      // The row is stored; the summary is only for the log line.
    }
    log('accepted', { reports: batch.reports.length, duplicate });
    return reply(202);
  }
  // The RPC's own validation raises SQLSTATE PT400, answered as HTTP 400 with
  // that code. Any other 4xx (a missing function, a bad key) is our problem,
  // not the sender's, so it is a 503 and the batch is kept for a retry.
  if (rpc.status === 400) {
    let code = '';
    try {
      code = String(((await rpc.json()) as { code?: unknown }).code ?? '');
    } catch {
      // Not a PostgREST error body.
    }
    if (code === 'PT400') {
      log('rejected', { reason: 'database_validation' });
      return reply(400, 'invalid telemetry batch');
    }
  }
  log('upstream_error', { kind: 'status', status: rpc.status });
  return reply(503, 'unavailable', { 'retry-after': '300' });
}
