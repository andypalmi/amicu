import 'server-only';
import { readJsonBody } from '@/lib/security/body';
import type { CreateConfigRequest, AppendVersionRequest } from '@amicu/api-client';

// Body/param validation for the /api/configs/* BFF routes. Session guarding and
// the upstream-unreachable helper are shared with the engine routes (see
// lib/engine/bff.ts); this only checks request shapes. Name uniqueness, HCL
// parsing, and version existence are the Management API's job - the BFF forwards,
// it does not decide.

/**
 * Parse a JSON body for `POST /api/configs`: require non-empty string `name` and
 * `content` (plus an optional string `message`). Returns the typed body or a 400
 * Response mirroring the API's own error shape.
 */
export async function readCreateConfigBody(
  request: Request,
): Promise<{ body: CreateConfigRequest } | { deny: Response }> {
  const parsed = await readJsonBody(request);
  if ('deny' in parsed) return parsed;
  const raw: unknown = parsed.value;
  const name = (raw as { name?: unknown })?.name;
  if (typeof name !== 'string' || name.trim() === '') {
    return { deny: Response.json({ error: '`name` (string) is required' }, { status: 400 }) };
  }
  const content = (raw as { content?: unknown })?.content;
  if (typeof content !== 'string') {
    return { deny: Response.json({ error: '`content` (string) is required' }, { status: 400 }) };
  }
  const rawMessage = (raw as { message?: unknown })?.message;
  const message = typeof rawMessage === 'string' ? rawMessage : undefined;
  return { body: { name, content, message } };
}

/**
 * Parse a JSON body for `POST /api/configs/{id}/versions`: require a string
 * `content` (plus an optional string `message`). Returns the typed body or a 400
 * Response.
 */
export async function readAppendVersionBody(
  request: Request,
): Promise<{ body: AppendVersionRequest } | { deny: Response }> {
  const parsed = await readJsonBody(request);
  if ('deny' in parsed) return parsed;
  const raw: unknown = parsed.value;
  const content = (raw as { content?: unknown })?.content;
  if (typeof content !== 'string') {
    return { deny: Response.json({ error: '`content` (string) is required' }, { status: 400 }) };
  }
  const rawMessage = (raw as { message?: unknown })?.message;
  const message = typeof rawMessage === 'string' ? rawMessage : undefined;
  return { body: { content, message } };
}

/**
 * Parse a route param expected to be a version number (e.g. the `{no}` segment in
 * `/configs/{id}/versions/{no}` and `/configs/{id}/rollback/{no}`): a positive
 * integer. Returns the number or a 400 Response.
 */
export function readVersionNo(raw: string): { no: number } | { deny: Response } {
  const no = Number(raw);
  if (!Number.isInteger(no) || no < 1) {
    return { deny: Response.json({ error: '`no` must be a positive integer' }, { status: 400 }) };
  }
  return { no };
}

/**
 * Parse the `from`/`to` query parameters for `GET /api/configs/{id}/diff`: both
 * required positive integers. Returns the numbers or a 400 Response mirroring the
 * API's own "missing or malformed" error.
 */
export function readDiffQuery(request: Request): { from: number; to: number } | { deny: Response } {
  const url = new URL(request.url);
  const rawFrom = url.searchParams.get('from');
  const rawTo = url.searchParams.get('to');
  const from = rawFrom === null ? NaN : Number(rawFrom);
  const to = rawTo === null ? NaN : Number(rawTo);
  if (!Number.isInteger(from) || from < 1 || !Number.isInteger(to) || to < 1) {
    return {
      deny: Response.json(
        { error: '`from` and `to` (positive integers) are required query parameters' },
        { status: 400 },
      ),
    };
  }
  return { from, to };
}
