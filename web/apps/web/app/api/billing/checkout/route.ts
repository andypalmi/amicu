import { engineClient } from '@/lib/engine/client';
import { guardEngine, upstreamUnreachable } from '@/lib/engine/bff';
import { readCheckoutBody } from '@/lib/billing/bff';

// BFF: POST /api/billing/checkout → Management API POST /billing/checkout.
// Session-guarded; attaches the bearer token server-side and mirrors the upstream
// status + JSON (200 { url } to redirect to Stripe Checkout, 400 unknown plan,
// 401 auth, 503 when billing is not configured on the server). No secret ever
// reaches the browser — it only receives the hosted Stripe URL.
export async function POST(request: Request): Promise<Response> {
  const guard = await guardEngine();
  if ('deny' in guard) return guard.deny;

  const parsed = await readCheckoutBody(request);
  if ('deny' in parsed) return parsed.deny;

  try {
    const { data, error, response } = await engineClient().POST('/billing/checkout', {
      body: parsed.body,
      headers: { Authorization: `Bearer ${guard.token}` },
    });
    return Response.json(data ?? error ?? null, { status: response.status });
  } catch {
    return upstreamUnreachable();
  }
}
