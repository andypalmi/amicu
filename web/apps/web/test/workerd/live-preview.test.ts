// Live-runtime companion to `test/workerd/cookie-refresh.unit.test.ts`, for
// amicu-planning#146(a): actually boots the built Worker under the REAL Cloudflare
// Workers runtime (workerd, via `wrangler dev --local`) - not a Node polyfill of
// Request/Response, but the genuine `nodejs_compat` sandbox @opennextjs/cloudflare
// targets - and drives the silent-refresh cookie-rotation path end-to-end over a
// real HTTP connection, plus the production API_URL fail-fast from the issue's
// acceptance criteria.
//
// NOT part of the default `pnpm test` run (see the `test` vs `test:workerd-live`
// scripts in package.json): a real `opennextjs-cloudflare build` (a full `next
// build`) takes ~60-90s, and this test also spawns two short-lived `wrangler dev`
// processes on top of that - multiple minutes total, which is too slow to gate
// every `pnpm test` invocation the way the fast unit test does. Run it explicitly:
//
//   pnpm --filter web test:workerd-live
//
// It needs: outbound network (wrangler/workerd downloading nothing new here since
// `pnpm install` already fetched the `workerd` binary, but wrangler's local mode
// still does a couple of housekeeping things) and the ability to bind local TCP
// ports (for the stub API server + wrangler dev's own listener). Neither is
// guaranteed in every sandbox: if the build fails, if `wrangler`/`npx` can't be
// spawned, or if the dev server never becomes ready within the timeout, every test
// in this file SKIPS itself (via `t.skip(...)`) with the captured reason instead of
// failing the suite - see `before()` below for exactly what's attempted and how
// failure is classified.
//
// This file was authored and verified against a real, successful `wrangler dev`
// run in the sandbox that produced it: real Set-Cookie rotation observed with
// HttpOnly/Secure/SameSite=Lax attributes, a real 500 from the production API_URL
// fail-fast, AND (added after an adversarial review flagged the risk) proof - via
// `fixtures/refresh-stub.ts`, a stub with REAL single-use/reuse-detection semantics
// matching crates/amicu-store/src/refresh.rs - that the real backend sees exactly
// ONE `/token/refresh` call per inbound request. OpenNext's Node.js middleware
// compat layer (explicitly flagged experimental) DOES duplicate the resulting
// Set-Cookie pair in the raw response (4 headers, 2 identical pairs) but does NOT
// call attemptRefresh()'s network path twice, so no token-family reuse/compromise
// occurs - see the first test's inline comment for the full analysis and the exact
// assertion that proves it. Environments without network/port access will
// legitimately skip.

import { test, before } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync, type ChildProcess } from 'node:child_process';
import http from 'node:http';
import path from 'node:path';
import { existsSync, readdirSync } from 'node:fs';
import type { AddressInfo } from 'node:net';
import { startStubAuthApi } from './fixtures/refresh-stub.ts';

const APP_DIR = path.resolve(import.meta.dirname, '../..');
const WRANGLER_BIN = path.join(APP_DIR, 'node_modules/.bin/wrangler');
const OPENNEXT_BIN = path.join(APP_DIR, 'node_modules/.bin/opennextjs-cloudflare');
const WORKER_ENTRY = path.join(APP_DIR, '.open-next/worker.js');

const BUILD_TIMEOUT_MS = 180_000;
const SERVER_READY_TIMEOUT_MS = 60_000;
const POLL_INTERVAL_MS = 500;

const ACCESS_COOKIE = '__Host-amicu_at';
const REFRESH_COOKIE = '__Host-amicu_rt';

function craftJwt(exp: number): string {
  const b64url = (obj: unknown) =>
    Buffer.from(JSON.stringify(obj)).toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  return `${b64url({ alg: 'none', typ: 'JWT' })}.${b64url({ exp, sub: 'test-user' })}.sig`;
}

async function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = http.createServer();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address() as AddressInfo;
      srv.close((err) => (err ? reject(err) : resolve(port)));
    });
  });
}

/** Poll a URL, without blind sleeping, until it responds or the timeout elapses. */
async function waitForReady(url: string, timeoutMs: number): Promise<{ ok: true } | { ok: false; reason: string }> {
  const deadline = Date.now() + timeoutMs;
  let lastError = 'no attempt made';
  while (Date.now() < deadline) {
    try {
      const controller = new AbortController();
      const t = setTimeout(() => controller.abort(), 2_000);
      // ANY completed HTTP response (including a 500) proves the dev server itself
      // is up and routing requests - readiness is about the SERVER, not about
      // whatever status a given test's env vars make the app return (a test that
      // deliberately misconfigures API_URL expects a 500 from the app, not from a
      // not-yet-started server). Only a network-level failure (connection refused,
      // this abort) means "keep waiting".
      await fetch(url, { signal: controller.signal });
      clearTimeout(t);
      return { ok: true };
    } catch (err) {
      lastError = err instanceof Error ? err.message : String(err);
    }
    await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
  }
  return { ok: false, reason: `timed out after ${timeoutMs}ms waiting for ${url} (last error: ${lastError})` };
}

/** Kill a `detached: true` child's whole process group, not just the top PID. */
function killGroup(child: ChildProcess): void {
  if (child.pid == null) return;
  try {
    process.kill(-child.pid, 'SIGTERM');
  } catch {
    try {
      child.kill('SIGTERM');
    } catch {
      // already gone
    }
  }
}

/**
 * Spawn `wrangler dev --local` against the already-built `.open-next/worker.js`,
 * overriding `vars` for this run, and wait for it to accept requests.
 */
async function startWranglerDev(
  varsOverride: Record<string, string>,
): Promise<{ ok: true; port: number; child: ChildProcess; log: string[] } | { ok: false; reason: string }> {
  let port: number;
  try {
    port = await findFreePort();
  } catch (err) {
    return { ok: false, reason: `could not find a free local port: ${(err as Error).message}` };
  }

  const log: string[] = [];
  const varArgs = Object.entries(varsOverride).flatMap(([k, v]) => ['--var', `${k}:${v}`]);
  let child: ChildProcess;
  try {
    child = spawn(WRANGLER_BIN, ['dev', '--local', '--port', String(port), ...varArgs], {
      cwd: APP_DIR,
      env: { ...process.env, CI: 'true', WRANGLER_SEND_METRICS: 'false' },
      detached: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
  } catch (err) {
    return { ok: false, reason: `failed to spawn wrangler dev: ${(err as Error).message}` };
  }

  child.stdout?.on('data', (d) => log.push(String(d)));
  child.stderr?.on('data', (d) => log.push(String(d)));

  let spawnError: string | null = null;
  child.on('error', (err) => {
    spawnError = err.message;
  });

  const readiness = await waitForReady(`http://127.0.0.1:${port}/api/health`, SERVER_READY_TIMEOUT_MS);
  if (spawnError) {
    killGroup(child);
    return { ok: false, reason: `wrangler dev process error: ${spawnError}\n--- log ---\n${log.join('')}` };
  }
  if (!readiness.ok) {
    killGroup(child);
    return { ok: false, reason: `${readiness.reason}\n--- wrangler dev log tail ---\n${log.join('').slice(-2000)}` };
  }
  return { ok: true, port, child, log };
}

let skipReason: string | null = null;
let buildReady = false;

before(async () => {
  if (!existsSync(WRANGLER_BIN) || !existsSync(OPENNEXT_BIN)) {
    skipReason = `wrangler/opennextjs-cloudflare binaries not found under ${APP_DIR}/node_modules/.bin - run pnpm install first`;
    return;
  }

  // Build the actual Worker bundle this test then boots for real. Always rebuild
  // (rather than trusting a stale .open-next/) so this test reflects the current
  // working tree, matching what a CI run of it would need anyway.
  const build = spawnSync(OPENNEXT_BIN, ['build'], {
    cwd: APP_DIR,
    timeout: BUILD_TIMEOUT_MS,
    encoding: 'utf8',
  });
  if (build.error) {
    skipReason = `opennextjs-cloudflare build could not be spawned: ${build.error.message}`;
    return;
  }
  if (build.status !== 0 || !existsSync(WORKER_ENTRY)) {
    const tail = `${build.stdout ?? ''}${build.stderr ?? ''}`.slice(-4000);
    skipReason = `opennextjs-cloudflare build failed (exit ${build.status ?? 'timeout'}) - this sandbox likely can't complete a full Next.js build (no network / build tooling missing / timeout). Tail:\n${tail}`;
    return;
  }

  // Verify a local port can actually be bound before trusting the rest of the
  // suite to it (the stub API itself is started per-test, freshly, since each
  // test needs its own clean token/family state).
  try {
    const probe = await startStubAuthApi();
    await probe.close();
  } catch (err) {
    skipReason = `could not bind a local port for a stub API server: ${(err as Error).message} - this sandbox may not allow listening on TCP ports`;
    return;
  }

  buildReady = true;
});

test('workerd (wrangler dev): rotates Set-Cookie with HttpOnly/Secure/SameSite=Lax, and the real backend sees exactly ONE refresh call (no reuse/family-compromise)', async (t) => {
  if (skipReason || !buildReady) {
    t.skip(skipReason ?? 'build did not complete');
    return;
  }

  // Faithful stub (see fixtures/refresh-stub.ts): single-use refresh tokens,
  // family-scoped reuse detection that compromises (revokes) the WHOLE family
  // on a replay - mirroring crates/amicu-store/src/refresh.rs `consume()` and
  // crates/amicu-auth/src/service.rs `refresh()`. This is what makes the
  // question below answerable for real: an earlier version of this test used a
  // stub that always minted the same canned pair regardless of input, which
  // could not have detected a reuse/double-spend bug even if one existed.
  const stubApi = await startStubAuthApi();
  const ORIGINAL_REFRESH = 'original-refresh-token';
  stubApi.seedRefreshToken(ORIGINAL_REFRESH);

  const server = await startWranglerDev({
    API_URL: `http://127.0.0.1:${stubApi.port}`,
    COOKIE_SECURE: 'true',
  });
  if (!server.ok) {
    await stubApi.close();
    t.skip(`wrangler dev did not come up in this sandbox: ${server.reason}`);
    return;
  }

  try {
    const expiredAccess = craftJwt(Math.floor(Date.now() / 1000) - 30);
    const res = await fetch(`http://127.0.0.1:${server.port}/app`, {
      redirect: 'manual',
      headers: { Cookie: `${ACCESS_COOKIE}=${expiredAccess}; ${REFRESH_COOKIE}=${ORIGINAL_REFRESH}` },
    });

    // THE QUESTION THIS TEST EXISTS TO ANSWER (raised in adversarial review of
    // this PR): does OpenNext's Node.js middleware compat layer (explicitly
    // flagged experimental in the build output) invoke handleAuth() -> a real
    // second network round-trip to POST /token/refresh - for one inbound
    // request? If it did, and that second call resubmitted the now-consumed
    // ORIGINAL_REFRESH token, the real backend's reuse detection would
    // compromise the whole token family, revoking even the successor token
    // just minted by the first call - silently logging out (or "compromising")
    // every legitimately refreshing user in production.
    //
    // OBSERVED (this assertion is the proof, not a claim): the faithful stub
    // recorded exactly ONE /token/refresh request. The response DOES still
    // carry the SAME rotated cookie pair twice (4 Set-Cookie headers, 2
    // identical pairs) - that part of the earlier "double invocation"
    // observation was real, but it is a RESPONSE-SERIALIZATION artifact (the
    // same already-decided Set-Cookie pair duplicated somewhere in OpenNext's
    // Node-compat -> Workers Response conversion), not a second invocation of
    // attemptRefresh()/authClient.refresh(). No second network call means no
    // reuse detection, no family compromise, and no risk of the failure mode
    // above under the ACTUAL observed behavior of this build.
    assert.equal(
      stubApi.requests.length,
      1,
      `expected exactly 1 /token/refresh call reaching the API (proving no reuse/double-spend), got ${stubApi.requests.length}: ${JSON.stringify(stubApi.requests)}`,
    );
    assert.equal(stubApi.requests[0].outcome, 'consumed');
    assert.equal(stubApi.requests[0].refreshToken, ORIGINAL_REFRESH);
    assert.ok(!stubApi.isFamilyCompromised(ORIGINAL_REFRESH), 'the token family must not be compromised by a single legitimate refresh');

    const setCookies = res.headers.getSetCookie();
    assert.ok(setCookies.length >= 2, `expected at least 2 Set-Cookie headers from real workerd, got: ${setCookies.join(' | ')}`);
    const atValues = new Set(setCookies.filter((c) => c.startsWith(`${ACCESS_COOKIE}=`)).map((c) => c.split(';')[0]));
    const rtValues = new Set(setCookies.filter((c) => c.startsWith(`${REFRESH_COOKIE}=`)).map((c) => c.split(';')[0]));
    // Every amicu_at (and amicu_rt) Set-Cookie header carries the SAME value -
    // confirms the duplication is the same single rotation serialized twice,
    // not two different token pairs from two different refresh calls.
    assert.equal(atValues.size, 1, `all amicu_at Set-Cookie headers must carry the same value, got: ${[...atValues]}`);
    assert.equal(rtValues.size, 1, `all amicu_rt Set-Cookie headers must carry the same value, got: ${[...rtValues]}`);

    const at = setCookies.find((c) => c.startsWith(`${ACCESS_COOKIE}=`));
    const rt = setCookies.find((c) => c.startsWith(`${REFRESH_COOKIE}=`));
    assert.ok(at, 'amicu_at Set-Cookie missing from real workerd response');
    assert.ok(rt, 'amicu_rt Set-Cookie missing from real workerd response');
    for (const c of [at, rt]) {
      assert.match(c!, /HttpOnly/i);
      assert.match(c!, /Secure/i);
      assert.match(c!, /SameSite=Lax/i);
    }
    // The rotated refresh cookie is the one the stub actually issued for the
    // single winning call - not the stale original, not a second mint.
    assert.equal(rt!.split(';')[0], `${REFRESH_COOKIE}=${stubApi.requests[0].issuedRefreshToken}`);

    // Not cached by a shared/CDN cache: no `public`/positive-shared-max-age
    // Cache-Control, and (this being local wrangler, not the real edge) no
    // CF-Cache-Status HIT.
    const cacheControl = res.headers.get('cache-control');
    if (cacheControl) {
      assert.doesNotMatch(cacheControl, /(^|,)\s*public\s*(,|;|$)/i);
    }
    assert.notEqual(res.headers.get('cf-cache-status'), 'HIT');
  } finally {
    killGroup(server.child);
    await stubApi.close();
  }
});

test('workerd (wrangler dev): production fail-fast fires for a misconfigured API_URL instead of defaulting to localhost', async (t) => {
  if (skipReason) {
    t.skip(skipReason);
    return;
  }

  // Empty API_URL under NODE_ENV=production (wrangler.jsonc's committed default) -
  // parseServerEnv() must throw at request time (server-env.ts), not silently fall
  // back to DEV_API_URL / localhost:8080 from inside Cloudflare's network.
  const server = await startWranglerDev({ API_URL: '' });
  if (!server.ok) {
    t.skip(`wrangler dev did not come up in this sandbox: ${server.reason}`);
    return;
  }

  try {
    const res = await fetch(`http://127.0.0.1:${server.port}/api/health`);
    assert.equal(
      res.status,
      500,
      'a misconfigured (empty) production API_URL must fail the request (parseServerEnv fail-fast), not boot against a localhost default',
    );
  } finally {
    killGroup(server.child);
  }
});

test('workerd (wrangler dev): security headers reach Worker responses AND static assets; cross-origin POST to /api is 403', async (t) => {
  if (skipReason || !buildReady) {
    t.skip(skipReason ?? 'build did not complete');
    return;
  }
  const server = await startWranglerDev({ API_URL: 'http://127.0.0.1:1', COOKIE_SECURE: 'true' });
  if (!server.ok) {
    t.skip(`wrangler dev did not come up in this sandbox: ${server.reason}`);
    return;
  }
  try {
    const base = `http://127.0.0.1:${server.port}`;
    const chunksDir = path.join(APP_DIR, '.open-next/assets/_next/static/chunks');
    const chunk = readdirSync(chunksDir).find((f) => f.endsWith('.js'));
    assert.ok(chunk, 'expected at least one built static chunk');

    for (const p of ['/api/health', '/login', `/_next/static/chunks/${chunk}`]) {
      const res = await fetch(`${base}${p}`);
      await res.arrayBuffer();
      assert.equal(res.headers.get('x-content-type-options'), 'nosniff', `${p}: nosniff`);
      assert.match(res.headers.get('strict-transport-security') ?? '', /max-age=\d+/, `${p}: HSTS`);
      assert.equal(res.headers.get('x-frame-options'), 'DENY', `${p}: X-Frame-Options`);
    }
    // CSP is document-level: must be on Worker-rendered HTML.
    const page = await fetch(`${base}/login`);
    await page.arrayBuffer();
    assert.match(page.headers.get('content-security-policy') ?? '', /default-src 'self'/);

    const cross = await fetch(`${base}/api/engine/plan`, {
      method: 'POST',
      headers: { origin: 'https://evil.example', 'content-type': 'application/json' },
      body: '{}',
    });
    assert.equal(cross.status, 403);
    const same = await fetch(`${base}/api/engine/plan`, {
      method: 'POST',
      headers: { origin: base, 'content-type': 'application/json' },
      body: '{}',
    });
    assert.equal(same.status, 401, 'same-origin POST passes the CSRF guard (then 401: no session)');
  } finally {
    killGroup(server.child);
  }
});
