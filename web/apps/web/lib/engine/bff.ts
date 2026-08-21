import 'server-only';
import { getSession } from '@/lib/auth/session';
import { readAccessToken } from '@/lib/auth/cookies';
import { logServerError, type ServerErrorContext } from '@/lib/observability/log-server-error';

// Shared plumbing for the /api/engine/* BFF routes. Each route requires a valid
// session, forwards the bearer token upstream, and mirrors the Management API's
// status + JSON straight back — the browser never sees the token or the API base.

/** A resolved bearer token, or a ready-to-return error Response when unauthorized. */
export type Guard = { token: string } | { deny: Response };

/** Require a session and a readable access token, or produce a 401 Response. */
export async function guardEngine(): Promise<Guard> {
  const session = await getSession();
  const token = session ? await readAccessToken() : undefined;
  if (!token) {
    return { deny: Response.json({ error: 'unauthenticated' }, { status: 401 }) };
  }
  return { token };
}

/**
 * Parse a JSON body and require a string `config` (plus an optional string
 * `prior`). Returns the fields or a 400 Response — mirroring the API's own
 * shape so the client handles one error contract everywhere.
 */
export async function readConfigBody(
  request: Request,
): Promise<{ config: string; prior?: string | null } | { deny: Response }> {
  let body: unknown;
  try {
    body = await request.json();
  } catch {
    return { deny: Response.json({ error: 'request body must be JSON' }, { status: 400 }) };
  }
  const config = (body as { config?: unknown })?.config;
  if (typeof config !== 'string') {
    return { deny: Response.json({ error: '`config` (string) is required' }, { status: 400 }) };
  }
  const rawPrior = (body as { prior?: unknown })?.prior;
  const prior = typeof rawPrior === 'string' ? rawPrior : undefined;
  return { config, prior };
}

/**
 * A 502 for when the Management API itself is unreachable.
 *
 * The client-facing body is intentionally generic — no internal message is ever
 * echoed. When the caught `error` is passed, it is logged server-side (stderr
 * only) with the given `context` BEFORE the 502 is returned, so upstream outages
 * are visible to operators without leaking anything to the browser.
 */
export function upstreamUnreachable(error?: unknown, context?: ServerErrorContext): Response {
  if (arguments.length > 0) {
    logServerError(error, context);
  }
  return Response.json(
    { error: 'the management API is unreachable' },
    { status: 502 },
  );
}
