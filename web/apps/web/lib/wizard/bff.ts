import 'server-only';
import type { components } from '@amicu/api-client';

type WizardAnswersDto = components['schemas']['WizardAnswersDto'];

// Body validation for the /api/presets/[id]/emit BFF route. Session guarding and
// the upstream-unreachable helper are shared with the engine routes (see
// lib/engine/bff.ts); this only checks the wizard-answers request shape.

/**
 * Parse a JSON body and require the `concurrency` + `context_length` numbers the
 * `/presets/{id}/emit` endpoint needs (budget, residency, and latency are
 * optional). Returns the typed body or a 400 Response - mirroring the API's own
 * error shape so the client handles one contract everywhere. The deep field
 * validation (ranges, must-be-positive) is the API's job; here we only reject
 * bodies that are structurally not a wizard-answers request.
 */
export async function readWizardAnswersBody(
  request: Request,
): Promise<{ body: WizardAnswersDto } | { deny: Response }> {
  let raw: unknown;
  try {
    raw = await request.json();
  } catch {
    return { deny: Response.json({ error: 'request body must be JSON' }, { status: 400 }) };
  }
  if (!raw || typeof raw !== 'object') {
    return { deny: Response.json({ error: 'request body must be an object' }, { status: 400 }) };
  }
  const { concurrency, context_length } = raw as {
    concurrency?: unknown;
    context_length?: unknown;
  };
  if (typeof concurrency !== 'number') {
    return { deny: Response.json({ error: '`concurrency` (number) is required' }, { status: 400 }) };
  }
  if (typeof context_length !== 'number') {
    return {
      deny: Response.json({ error: '`context_length` (number) is required' }, { status: 400 }),
    };
  }
  return { body: raw as WizardAnswersDto };
}
