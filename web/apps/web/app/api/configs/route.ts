import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readCreateConfigBody } from '@/lib/configs/bff';

// BFF: GET /api/configs -> Management API GET /configs.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON (200 ConfigSummaryDto[], 401/403 auth).
export async function GET(): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  try {
    const { data, error, response } = await engineClient().GET('/configs', {
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'GET /api/configs', op: 'configs.list' });
  }
}

// BFF: POST /api/configs -> Management API POST /configs.
// Session-guarded; forwards {name, content, message?} and mirrors the upstream
// status + JSON (201 ConfigDetailDto, 400 HCL parse error, 401/403 auth, 409 name
// already exists).
export async function POST(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readCreateConfigBody(request);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().POST('/configs', {
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch (err) {
    return upstreamUnreachable(err, { route: 'POST /api/configs', op: 'configs.create' });
  }
}
