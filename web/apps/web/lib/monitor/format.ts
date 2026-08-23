// Pure, framework-free helpers for rendering the monitoring dashboard
// (measured-vs-estimated). Kept free of React so they can be unit-tested with
// `node --test` (see format.test.ts). These helpers only *present* API-provided
// values - they never compute a divergence verdict, rank, or invent data. The
// measured distribution and the estimate range are laid out side by side; any
// analysis is the engine's (in `suggestions`), never here.

import type { components } from '@amicu/api-client';
// Re-export the tuning-report *types* so the dashboard imports one contract for
// the advisory section. The runtime presentation helpers (confidenceMeta,
// bottleneckLabel, …) are imported directly from lib/recommend/format by the
// dashboard so this pure module stays free of runtime cross-imports (keeping it
// runnable under `node --test`, which does not resolve the `@/` path alias).
export type {
  Confidence,
  Comparison,
  ChangeDirection,
  BottleneckClass,
  Evidence,
  SuggestedChange,
  RejectedAlternative,
  Recommendation,
  TuningReport,
} from '@/lib/recommend/format';

export type MonitorView = components['schemas']['MonitorViewDto'];
export type StackMetrics = components['schemas']['StackMetricsDto'];
export type WorkloadEstimate = components['schemas']['WorkloadEstimateDto'];
export type Distribution = components['schemas']['DistributionDto'];
export type EstimateRange = components['schemas']['EstimateRangeDto'];
export type Money = components['schemas']['MoneyDto'];
export type Provenance = components['schemas']['ProvenanceDto'];
export type Assumption = components['schemas']['AssumptionDto'];

/** Trim a measurement to a short, readable form (no scientific notation). */
export function formatNumber(value: number): string {
  if (!Number.isFinite(value)) return String(value);
  if (Number.isInteger(value)) return String(value);
  return String(Number(value.toFixed(3)));
}

/** A measurement with a unit suffix, e.g. `220 ms`. Empty unit → bare number. */
export function formatMeasure(value: number, unit = ''): string {
  const n = formatNumber(value);
  return unit ? `${n} ${unit}` : n;
}

/** A fraction in [0,1] rendered as a percentage, e.g. `0.97 → "97%"`. */
export function formatPercent(fraction: number): string {
  if (!Number.isFinite(fraction)) return String(fraction);
  return `${formatNumber(fraction * 100)}%`;
}

/** Money rendered as `amount currency`, e.g. `1.5 USD`. Values come from the API. */
export function formatMoney(money: Money): string {
  return `${formatNumber(money.amount)} ${money.currency}`;
}

/** The three percentiles of a distribution as a compact P50/P90/P99 triple. */
export function distributionPoints(
  dist: Distribution,
  unit = '',
): { label: string; value: string }[] {
  return [
    { label: 'P50', value: formatMeasure(dist.p50, unit) },
    { label: 'P90', value: formatMeasure(dist.p90, unit) },
    { label: 'P99', value: formatMeasure(dist.p99, unit) },
  ];
}

/** A stable, human-readable label for an estimate provenance. */
export function provenanceLabel(provenance: Provenance): string {
  switch (provenance) {
    case 'telemetry_aggregate':
      return 'Telemetry aggregate';
    case 'public_benchmark':
      return 'Public benchmark';
    case 'provider_price':
      return 'Provider price';
    case 'formula':
      return 'Formula';
    default:
      return provenance;
  }
}

/** An estimate range rendered as `low–high unit`, e.g. `80–300 ms`. */
export function formatRange(range: EstimateRange, unit = ''): string {
  return `${formatNumber(range.low)}–${formatMeasure(range.high, unit)}`;
}

/**
 * The estimate's stated assumptions as plain strings. Prefers the engine-derived
 * `assumptions` echoed back; falls back to the assumed_* fields so the
 * attribution is always shown even for an older API. Pure formatting - no verdict.
 */
export function estimateAssumptions(estimate: WorkloadEstimate): string[] {
  if (estimate.assumptions && estimate.assumptions.length > 0) {
    return estimate.assumptions.map((a: Assumption) => a.statement);
  }
  return [
    `assumed prompt ${estimate.assumed_prompt_tokens} tokens, generation ${estimate.assumed_generation_tokens} tokens`,
    `assumed concurrency ${estimate.assumed_concurrency}`,
  ];
}

/**
 * Whether a well-formed response body is a MonitorView we can render. Guards the
 * shape before any narrowing so the dashboard never does an unchecked `as` cast.
 */
export function looksLikeMonitorView(body: unknown): body is MonitorView {
  if (!body || typeof body !== 'object') return false;
  const b = body as Record<string, unknown>;
  const metrics = b.metrics as Record<string, unknown> | undefined;
  const estimate = b.estimate as Record<string, unknown> | undefined;
  const suggestions = b.suggestions as Record<string, unknown> | undefined;
  return (
    !!metrics &&
    typeof metrics === 'object' &&
    typeof metrics.deployment === 'string' &&
    !!metrics.latency &&
    !!estimate &&
    typeof estimate === 'object' &&
    typeof estimate.assumed_prompt_tokens === 'number' &&
    !!suggestions &&
    typeof suggestions === 'object' &&
    Array.isArray(suggestions.recommendations)
  );
}
