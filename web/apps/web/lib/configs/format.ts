// Pure, framework-free helpers for the My Configs UI (list, version history,
// diff, restore, load-into-wizard) and the wizard's Save action. Kept free of
// React (so they can be unit-tested with `node --test`, see format.test.ts) and
// free of runtime cross-imports so the type-stripped runner can execute them -
// only *type* imports from `@amicu/api-client` are used, and those are erased.
//
// These helpers only present what the API confirms; they never invent a config,
// version, or diff. The Management API (via the /api/configs/* BFF) is the sole
// authority on stored configuration - the console renders its responses and
// nothing more.
//
// Deliberately no Date/timestamp formatting here: `new Date` is avoided in this
// testable, potentially-prerendered module. Timestamps are rendered in the
// client components (browser runtime) from the raw Unix seconds the API returns.

import type { components } from '@amicu/api-client';

export type ConfigSummaryDto = components['schemas']['ConfigSummaryDto'];
export type ConfigDetailDto = components['schemas']['ConfigDetailDto'];
export type ConfigDiffDto = components['schemas']['ConfigDiffDto'];
export type VersionMetaDto = components['schemas']['VersionMetaDto'];
export type VersionContentDto = components['schemas']['VersionContentDto'];
export type CreateConfigRequest = components['schemas']['CreateConfigRequest'];
export type AppendVersionRequest = components['schemas']['AppendVersionRequest'];

function isConfigSummary(v: unknown): v is ConfigSummaryDto {
  if (!v || typeof v !== 'object') return false;
  const c = v as Record<string, unknown>;
  return (
    typeof c.id === 'string' &&
    typeof c.name === 'string' &&
    typeof c.current_version === 'number' &&
    typeof c.updated_at === 'number'
  );
}

/** Whether a body is a well-formed ConfigSummaryDto[] (`GET /configs`). */
export function looksLikeConfigSummaryList(body: unknown): body is ConfigSummaryDto[] {
  return Array.isArray(body) && body.every(isConfigSummary);
}

/** Whether a body is a well-formed ConfigDetailDto (`GET/POST /configs/{id}`). */
export function looksLikeConfigDetail(body: unknown): body is ConfigDetailDto {
  if (!body || typeof body !== 'object') return false;
  const c = body as Record<string, unknown>;
  return (
    typeof c.id === 'string' &&
    typeof c.name === 'string' &&
    typeof c.content === 'string' &&
    typeof c.content_sha256 === 'string' &&
    typeof c.current_version === 'number' &&
    typeof c.updated_at === 'number'
  );
}

function isVersionMeta(v: unknown): v is VersionMetaDto {
  if (!v || typeof v !== 'object') return false;
  const m = v as Record<string, unknown>;
  return (
    typeof m.version_no === 'number' &&
    typeof m.created_at === 'number' &&
    typeof m.content_sha256 === 'string'
  );
}

/** Whether a body is a well-formed VersionMetaDto[] (`GET /configs/{id}/versions`). */
export function looksLikeVersionMetaList(body: unknown): body is VersionMetaDto[] {
  return Array.isArray(body) && body.every(isVersionMeta);
}

/** Whether a body is a well-formed VersionMetaDto (an append or rollback result). */
export function looksLikeVersionMeta(body: unknown): body is VersionMetaDto {
  return isVersionMeta(body);
}

/** Whether a body is a well-formed VersionContentDto (`GET .../versions/{no}`). */
export function looksLikeVersionContent(body: unknown): body is VersionContentDto {
  if (!body || typeof body !== 'object') return false;
  const c = body as Record<string, unknown>;
  return (
    typeof c.version_no === 'number' &&
    typeof c.created_at === 'number' &&
    typeof c.content === 'string' &&
    typeof c.content_sha256 === 'string'
  );
}

/** Whether a body is a well-formed ConfigDiffDto (`GET /configs/{id}/diff`). */
export function looksLikeConfigDiff(body: unknown): body is ConfigDiffDto {
  if (!body || typeof body !== 'object') return false;
  const d = body as Record<string, unknown>;
  return typeof d.from === 'number' && typeof d.to === 'number' && typeof d.diff === 'string';
}

/** A short, human-friendly prefix of a hex sha256 digest, for compact display. */
export function shortSha(sha: string): string {
  return sha.slice(0, 10);
}

/** The API's own error string, if the body carries one. */
export function apiErrorMessage(body: unknown): string | null {
  return body &&
    typeof body === 'object' &&
    typeof (body as { error?: unknown }).error === 'string'
    ? (body as { error: string }).error
    : null;
}

/** Map a non-OK /api/configs/* response to a friendly message, preferring the
 *  API's own error string. Shared by the My Configs console and the wizard's
 *  Save action, since both call the same BFF routes. */
export function errorMessage(status: number, body: unknown): string {
  const err = apiErrorMessage(body);
  if (status === 401) return 'Your session has expired. Reload the page or sign in again.';
  if (status === 400) return err ?? 'That request was malformed.';
  if (status === 404) return err ?? 'That configuration or version no longer exists.';
  if (status === 409) return err ?? 'A configuration with that name already exists.';
  if (status === 502) return 'The configuration service is unreachable.';
  return err ?? `Request failed (${status}).`;
}

export type DiffLineKind = 'add' | 'remove' | 'hunk' | 'header' | 'context';

/** Classify one line of a unified diff for display styling. */
export function classifyDiffLine(line: string): DiffLineKind {
  if (line.startsWith('+++') || line.startsWith('---')) return 'header';
  if (line.startsWith('@@')) return 'hunk';
  if (line.startsWith('+')) return 'add';
  if (line.startsWith('-')) return 'remove';
  return 'context';
}
