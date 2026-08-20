// Pure, framework-free helpers for rendering the advisory tuning report. Kept
// free of React so they can be unit-tested with `node --test` (see
// format.test.ts). These helpers only *present* API-provided data — they never
// author, rank, or invent rationale, evidence, or confidence.

import type { components } from '@amicu/api-client';

export type Confidence = components['schemas']['ConfidenceDto'];
export type Comparison = components['schemas']['ComparisonDto'];
export type ChangeDirection = components['schemas']['ChangeDirectionDto'];
export type BottleneckClass = components['schemas']['BottleneckClassDto'];
export type Evidence = components['schemas']['EvidenceDto'];
export type SuggestedChange = components['schemas']['SuggestedChangeDto'];
export type RejectedAlternative = components['schemas']['RejectedAlternativeDto'];
export type Recommendation = components['schemas']['RecommendationDto'];
export type TuningReport = components['schemas']['TuningReportDto'];

type Tone = 'default' | 'secondary' | 'destructive' | 'outline';

/**
 * Confidence rendered as a *band* (label + badge tone), never a bare number
 * presented as truth. The API is the sole source of the level; we only style it.
 */
export function confidenceMeta(confidence: Confidence): { label: string; tone: Tone } {
  switch (confidence) {
    case 'high':
      return { label: 'High confidence', tone: 'default' };
    case 'medium':
      return { label: 'Medium confidence', tone: 'secondary' };
    case 'low':
      return { label: 'Low confidence', tone: 'outline' };
    case 'estimated':
      return { label: 'Estimated', tone: 'outline' };
    default:
      return { label: confidence, tone: 'outline' };
  }
}

/** A stable, human-readable label for a bottleneck class. */
export function bottleneckLabel(bottleneck: BottleneckClass): string {
  switch (bottleneck) {
    case 'under_provisioned':
      return 'Under-provisioned';
    case 'over_provisioned':
      return 'Over-provisioned';
    case 'prefill_bound':
      return 'Prefill-bound';
    case 'decode_bound':
      return 'Decode-bound';
    case 'preemption_recompute':
      return 'Preemption / recompute';
    default:
      return bottleneck;
  }
}

/** Presentation for a suggested-change direction: a label and a compact sigil. */
export function directionMeta(direction: ChangeDirection): { label: string; sigil: string } {
  switch (direction) {
    case 'increase':
      return { label: 'Increase', sigil: '↑' };
    case 'decrease':
      return { label: 'Decrease', sigil: '↓' };
    case 'enable':
      return { label: 'Enable', sigil: '⏻' };
    case 'replace':
      return { label: 'Replace', sigil: '⇄' };
    default:
      return { label: direction, sigil: '•' };
  }
}

/** The comparator glyph for an evidence comparison. */
export function comparisonSymbol(comparison: Comparison): string {
  return comparison === 'above' ? '>' : '<';
}

/** Trim a measurement to a short, readable form (no scientific notation). */
export function formatNumber(value: number): string {
  if (!Number.isFinite(value)) return String(value);
  if (Number.isInteger(value)) return String(value);
  // Up to three significant fractional digits, trailing zeros stripped.
  return String(Number(value.toFixed(3)));
}

/**
 * Render one piece of evidence as `metric = observed {>|<} threshold`, e.g.
 * `queue.wait_ms.p99 = 1200 > 500`. Pure string assembly of API-provided values.
 */
export function formatEvidence(evidence: Evidence): string {
  const symbol = comparisonSymbol(evidence.comparison);
  return `${evidence.metric} = ${formatNumber(evidence.observed)} ${symbol} ${formatNumber(
    evidence.threshold,
  )}`;
}

/**
 * True when a BFF response status means the tier gate blocked the call (403) —
 * distinct from an unauthenticated 401. Used to show an upsell, not a crash.
 */
export function isTierDenied(status: number): boolean {
  return status === 403;
}
