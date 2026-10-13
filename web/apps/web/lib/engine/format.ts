// Pure, framework-free helpers for rendering engine results. Kept free of React
// so they can be unit-tested with `node --test` (see format.test.ts).

import type { components } from '@amicu/api-client';

export type Diagnostic = components['schemas']['Diagnostic'];
export type DiagnosticReport = components['schemas']['DiagnosticReport'];
export type Severity = components['schemas']['Severity'];
export type Plan = components['schemas']['Plan'];
export type PlannedAction = components['schemas']['PlannedAction'];
export type ActionKind = components['schemas']['ActionKind'];

/** Severity render order: errors first, then warnings, then info. */
export const SEVERITY_ORDER: readonly Severity[] = ['error', 'warning', 'info'];

/** Group a report's diagnostics by severity, preserving report order within each. */
export function groupBySeverity(
  diagnostics: readonly Diagnostic[],
): Record<Severity, Diagnostic[]> {
  const groups: Record<Severity, Diagnostic[]> = { error: [], warning: [], info: [] };
  for (const d of diagnostics) {
    // Unknown severities are treated as info so nothing is silently dropped.
    (groups[d.severity] ?? groups.info).push(d);
  }
  return groups;
}

/** Count diagnostics per severity. */
export function severityCounts(
  diagnostics: readonly Diagnostic[],
): Record<Severity, number> {
  const g = groupBySeverity(diagnostics);
  return { error: g.error.length, warning: g.warning.length, info: g.info.length };
}

type Tone = 'default' | 'secondary' | 'destructive' | 'outline' | 'success' | 'warning';

/** Badge variant + label + short glyph for a diagnostic severity. */
export function severityMeta(severity: Severity): { label: string; tone: Tone } {
  switch (severity) {
    case 'error':
      return { label: 'Error', tone: 'destructive' };
    case 'warning':
      return { label: 'Warning', tone: 'default' };
    default:
      return { label: 'Info', tone: 'secondary' };
  }
}

/** Presentation for a plan action kind: label, badge tone, and a diff sigil. */
export function actionKindMeta(kind: ActionKind): {
  label: string;
  tone: Tone;
  sigil: string;
} {
  switch (kind) {
    case 'create':
      return { label: 'Create', tone: 'success', sigil: '+' };
    case 'update':
      return { label: 'Update', tone: 'warning', sigil: '~' };
    case 'replace':
      return { label: 'Replace', tone: 'outline', sigil: '±' };
    case 'destroy':
      return { label: 'Destroy', tone: 'destructive', sigil: '-' };
    default:
      return { label: kind, tone: 'outline', sigil: '?' };
  }
}

/** Count planned actions by kind, in a stable, display-friendly order. */
export function summarizePlan(
  actions: readonly PlannedAction[],
): { kind: ActionKind; count: number }[] {
  const order: ActionKind[] = ['create', 'update', 'replace', 'destroy'];
  const counts = new Map<ActionKind, number>();
  for (const a of actions) counts.set(a.kind, (counts.get(a.kind) ?? 0) + 1);
  return order
    .filter((k) => counts.has(k))
    .map((k) => ({ kind: k, count: counts.get(k)! }));
}

/**
 * True when a BFF response status means the tier gate blocked the call (403) -
 * distinct from an unauthenticated 401. Used to show an upgrade hint, not a crash.
 */
export function isTierDenied(status: number): boolean {
  return status === 403;
}
