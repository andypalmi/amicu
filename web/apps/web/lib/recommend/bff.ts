import 'server-only';
import { readJsonBody } from '@/lib/security/body';
import type { RecommendRequest, components } from '@amicu/api-client';

type CustomizeRequestDto = components['schemas']['CustomizeRequestDto'];

// Body validation for the /api/recommend BFF route. Session guarding and the
// upstream-unreachable helper are shared with the engine routes (see
// lib/engine/bff.ts); this only checks the recommend request shape.

/**
 * Parse a JSON body and require the `metrics` + `estimate` objects the
 * `/recommend` endpoint needs (thresholds are optional). Returns the typed body
 * or a 400 Response - mirroring the API's own error shape so the client handles
 * one contract everywhere. The deep field validation is the API's job; here we
 * only reject bodies that are structurally not a recommend request.
 */
export async function readRecommendBody(
  request: Request,
): Promise<{ body: RecommendRequest } | { deny: Response }> {
  const parsed = await readJsonBody(request);
  if ('deny' in parsed) return parsed;
  const raw: unknown = parsed.value;
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
  return { body: raw as RecommendRequest };
}

/**
 * Parse a JSON body and require the `hcl` (string) the `/recommend/customize`
 * endpoint needs; `base_graph` is optional and, if present, only checked for
 * being an object (its shape is the API's job to validate as a `Graph`).
 * Returns the typed body or a 400 Response, mirroring `readRecommendBody`
 * above.
 */
export async function readCustomizeBody(
  request: Request,
): Promise<{ body: CustomizeRequestDto } | { deny: Response }> {
  const parsed = await readJsonBody(request);
  if ('deny' in parsed) return parsed;
  const raw: unknown = parsed.value;
  if (!raw || typeof raw !== 'object') {
    return { deny: Response.json({ error: 'request body must be an object' }, { status: 400 }) };
  }
  const { hcl, base_graph } = raw as { hcl?: unknown; base_graph?: unknown };
  if (typeof hcl !== 'string') {
    return { deny: Response.json({ error: '`hcl` (string) is required' }, { status: 400 }) };
  }
  if (base_graph !== undefined && base_graph !== null && typeof base_graph !== 'object') {
    return {
      deny: Response.json({ error: '`base_graph`, if present, must be an object' }, { status: 400 }),
    };
  }
  return { body: raw as CustomizeRequestDto };
}
