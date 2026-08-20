// Thin BFF placeholder. Real route handlers (bearer attach, response shaping,
// server-side secrets) land with later web-epic slices — no domain logic here.
export const dynamic = 'force-static';

export function GET() {
  return Response.json({ status: 'ok' });
}
