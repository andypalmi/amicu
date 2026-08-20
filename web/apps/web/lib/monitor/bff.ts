import 'server-only';
import type { MonitorRequest } from '@amicu/api-client';

// Body validation for the /api/monitor BFF route. Session guarding and the
// upstream-unreachable helper are shared with the engine routes (see
// lib/engine/bff.ts); this only checks the monitor request shape.

/**
 * Parse a JSON body and require the `metrics` + `estimate` objects the
 * `/monitor` endpoint needs. Returns the typed body or a 400 Response —
 * mirroring the API's own error shape so the client handles one contract
 * everywhere. The deep field validation is the API's job; here we only reject
 * bodies that are structurally not a monitor request.
 */
export async function readMonitorBody(
  request: Request,
): Promise<{ body: MonitorRequest } | { deny: Response }> {
  let raw: unknown;
  try {
    raw = await request.json();
  } catch {
    return { deny: Response.json({ error: 'request body must be JSON' }, { status: 400 }) };
  }
  if (!raw || typeof raw !== 'object') {
    return { deny: Response.json({ error: 'request body must be an object' }, { status: 400 }) };
  }
  const { metrics, estimate } = raw as { metrics?: unknown; estimate?: unknown };
  if (!metrics || typeof metrics !== 'object') {
    return { deny: Response.json({ error: '`metrics` (object) is required' }, { status: 400 }) };
  }
  if (!estimate || typeof estimate !== 'object') {
    return { deny: Response.json({ error: '`estimate` (object) is required' }, { status: 400 }) };
  }
  return { body: raw as MonitorRequest };
}
